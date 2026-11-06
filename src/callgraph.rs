//! Symbol lookups over the stored chunks: where a name is defined, what calls it, and, for
//! search, the definitions a query names with their direct callers and callees.
//!
//! Chunks store names only (`chunks.defines`, `chunks.calls`, see [`crate::chunk::Chunk`]). A call
//! is tied to a definition here, by [`resolve`]: a definition in the caller's own file first, then
//! one the call's path or import points at, then the only definition with that name. When several
//! definitions share the name and nothing tells them apart, the call is ambiguous and is reported
//! as such rather than guessed. Calls through traits, interfaces, callbacks and other dynamic
//! dispatch are not seen.

use std::collections::HashSet;

use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::error::AppError;
use crate::search::STOP_WORDS;

/// Definitions of one name looked at per lookup.
const MAX_DEFINITIONS: i64 = 100;
/// Chunks calling a name looked at per lookup.
pub const MAX_CALLERS: usize = 200;
/// Query words looked up as symbols by [`related`].
const RELATED_NAMES: usize = 8;
/// A query word with more definitions than this is too common to point anywhere (`new`, `get`).
const RELATED_DEFINITIONS_PER_NAME: usize = 5;
/// Rows taken by each query of [`related`].
const RELATED_BRANCH: i64 = 50;
/// The longest list [`related`] returns.
const RELATED_LIMIT: usize = 30;
/// Callee names of the matched definitions looked up by [`related`].
const RELATED_CALLEE_NAMES: usize = 64;

/// The tenant and the optional kind and source filters every lookup applies.
#[derive(Debug, Clone, Copy)]
pub struct Scope<'a> {
    pub tenant_id: Uuid,
    pub kinds: Option<&'a [String]>,
    pub source: Option<&'a str>,
}

/// A chunk as the lookups see it.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Site {
    pub chunk_id: i64,
    pub document_id: Uuid,
    pub source_id: Uuid,
    pub path: Option<String>,
    pub defines: Vec<String>,
    pub calls: Vec<String>,
}

/// How a call was tied to a definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// The definition is in the caller's own file.
    SameFile,
    /// The call's path or an import names the definition's module or type.
    Imported,
    /// No other definition has the name.
    Unique,
    /// Several definitions have the name and nothing tells them apart.
    Ambiguous,
}

/// A symbol as asked for: `chunk_file`, `Plan::limits`, `store.Search`, `crate::chunk::chunk_file`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    /// The module or type before the name, if one was given.
    pub qualifier: Option<String>,
    pub name: String,
}

fn is_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
        && !s.starts_with(|c: char| c.is_ascii_digit())
}

/// Parses what a caller typed. `None` when the last part is not a name.
pub fn parse_symbol(s: &str) -> Option<Symbol> {
    let s = s.trim().trim_end_matches("()").trim();
    let parts: Vec<&str> = s
        .split("::")
        .flat_map(|p| p.split('.'))
        .filter(|p| !p.is_empty())
        .collect();
    if parts.iter().any(|p| !is_name(p)) {
        return None;
    }
    let (name, before) = parts.split_last()?;
    if !is_name(name) {
        return None;
    }
    let qualifier = before
        .last()
        .filter(|q| is_name(q) && !matches!(**q, "crate" | "self" | "super" | "Self"))
        .map(|q| q.to_string());
    Some(Symbol {
        qualifier,
        name: name.to_string(),
    })
}

/// The names a file is imported by: its stem (`plans` of `src/billing/plans.rs`) unless that is
/// `mod`, `index`, `__init__`, `lib` or `main`, and the name of its directory.
fn module_names(path: Option<&str>) -> Vec<&str> {
    let Some(path) = path else {
        return Vec::new();
    };
    let mut parts = path.rsplit('/');
    let mut out = Vec::new();
    if let Some(file) = parts.next() {
        let stem = file.split('.').next().unwrap_or(file);
        if !matches!(stem, "mod" | "index" | "__init__" | "lib" | "main" | "") {
            out.push(stem);
        }
    }
    if let Some(dir) = parts.next().filter(|d| !d.is_empty()) {
        out.push(dir);
    }
    out
}

/// Whether the qualifier `q` of a call to `name` points at `def`: `def` defines `q::name`, or
/// its file is the module `q`.
fn qualifier_matches(q: &str, name: &str, def: &Site) -> bool {
    let qualified = format!("{q}::{name}");
    def.defines.contains(&qualified)
        || module_names(def.path.as_deref())
            .iter()
            .any(|m| m.eq_ignore_ascii_case(q))
}

/// The qualifiers `caller` calls `name` through (`store` of `store::index_text`).
fn qualifiers<'a>(caller: &'a Site, name: &str) -> Vec<&'a str> {
    caller
        .calls
        .iter()
        .filter_map(|c| c.strip_suffix(name)?.strip_suffix("::"))
        .filter(|q| !q.is_empty())
        .collect()
}

/// Which of `defs` (each defining `name`) a call to `name` from `caller` reaches, as indexes into
/// `defs`, and how that was decided:
///
/// 1. definitions in the caller's file, when the call has no path or its path agrees;
/// 2. definitions the call's path or import points at;
/// 3. the only definition, for a call by bare name;
/// 4. otherwise all of them, ambiguous.
///
/// `None` when the caller does not call `name`, or calls it only through a path that points at
/// none of `defs` (`String::from` is not a call to some indexed `from`).
pub fn resolve(name: &str, caller: &Site, defs: &[&Site]) -> Option<(Vec<usize>, Resolution)> {
    let eligible: Vec<usize> = (0..defs.len())
        .filter(|&i| defs[i].source_id == caller.source_id)
        .collect();
    let bare = caller.calls.iter().any(|c| c == name);
    let quals = qualifiers(caller, name);
    if eligible.is_empty() || (!bare && quals.is_empty()) {
        return None;
    }
    let by_path: Vec<usize> = eligible
        .iter()
        .copied()
        .filter(|&i| quals.iter().any(|q| qualifier_matches(q, name, defs[i])))
        .collect();
    let same: Vec<usize> = eligible
        .iter()
        .copied()
        .filter(|&i| defs[i].document_id == caller.document_id)
        .collect();
    if !same.is_empty() {
        let agreed: Vec<usize> = same
            .iter()
            .copied()
            .filter(|i| by_path.contains(i))
            .collect();
        if !agreed.is_empty() {
            let resolution = if agreed.len() == 1 {
                Resolution::SameFile
            } else {
                Resolution::Ambiguous
            };
            return Some((agreed, resolution));
        }
        if by_path.is_empty() && bare {
            let resolution = if same.len() == 1 {
                Resolution::SameFile
            } else {
                Resolution::Ambiguous
            };
            return Some((same, resolution));
        }
    }
    if !by_path.is_empty() {
        let resolution = if by_path.len() == 1 {
            Resolution::Imported
        } else {
            Resolution::Ambiguous
        };
        return Some((by_path, resolution));
    }
    if !bare {
        return None;
    }
    if eligible.len() == 1 {
        return Some((eligible, Resolution::Unique));
    }
    Some((eligible, Resolution::Ambiguous))
}

const SITE_COLUMNS: &str = "c.id AS chunk_id, c.document_id, d.source_id, left(d.metadata->>'path', 2048) AS path, \
     c.defines, c.calls";

/// Chunks whose `column` (`defines` or `calls`) holds any of `names`, in path and line order.
async fn sites_with(
    connection: &mut PgConnection,
    scope: &Scope<'_>,
    column: &str,
    names: &[String],
    limit: i64,
) -> Result<Vec<Site>, AppError> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    debug_assert!(matches!(column, "defines" | "calls"));
    // Probe GIN one name at a time before source joins. A wide overlap predicate can
    // otherwise make the planner scan every chunk in the source, even for absent names.
    // Materialize ids only, not the potentially wide metadata arrays.
    let sites = sqlx::query_as(&format!(
        "WITH matched AS MATERIALIZED (
             SELECT DISTINCT site.id
             FROM unnest($2::text[]) AS names(name)
             CROSS JOIN LATERAL (
                 SELECT id FROM chunks
                 WHERE tenant_id = $1 AND {column} @> ARRAY[names.name]
                 OFFSET 0
             ) site
         )
         SELECT {SITE_COLUMNS}
         FROM matched m
         JOIN chunks c ON c.id = m.id
         JOIN documents d ON d.id = c.document_id
         JOIN sources s ON s.id = d.source_id
         WHERE c.tenant_id = $1
           AND ($3::text[] IS NULL OR d.kind = ANY($3)) AND ($4::text IS NULL OR s.name = $4)
         ORDER BY d.metadata->>'path', c.start_line, c.id
         LIMIT $5"
    ))
    .bind(scope.tenant_id)
    .bind(names)
    .bind(scope.kinds)
    .bind(scope.source)
    .bind(limit)
    .fetch_all(connection)
    .await?;
    Ok(sites)
}

/// What a call to `name` is stored as: the bare name, and `::name` for calls through a path.
fn call_keys(name: &str) -> [String; 2] {
    [name.to_string(), format!("::{name}")]
}

/// The chunks that define `symbol`, in path and line order. With a qualifier, only those it
/// points at: `Plan::limits` is a `limits` defined in `Plan`, or in a file or directory named
/// `Plan`.
pub async fn definitions(
    pool: &PgPool,
    scope: &Scope<'_>,
    symbol: &Symbol,
) -> Result<Vec<Site>, AppError> {
    Ok(definition_lookup(pool, scope, symbol).await?.definitions)
}

/// Definitions with an explicit indication that the candidate set may have been capped.
pub async fn definition_lookup(
    pool: &PgPool,
    scope: &Scope<'_>,
    symbol: &Symbol,
) -> Result<Callers, AppError> {
    let mut connection = pool.acquire().await?;
    let all = sites_with(
        &mut connection,
        scope,
        "defines",
        std::slice::from_ref(&symbol.name),
        MAX_DEFINITIONS,
    )
    .await?;
    let truncated = all.len() == MAX_DEFINITIONS as usize;
    let definitions = all
        .into_iter()
        .filter(|s| match &symbol.qualifier {
            Some(q) => qualifier_matches(q, &symbol.name, s),
            None => true,
        })
        .collect();
    Ok(Callers {
        definitions,
        truncated,
        ..Default::default()
    })
}

/// A chunk that calls the symbol asked for.
#[derive(Debug, Clone)]
pub struct Caller {
    pub site: Site,
    pub resolution: Resolution,
    /// How many definitions the call may reach: 1 unless ambiguous.
    pub definitions: usize,
}

#[derive(Debug, Clone, Default)]
pub struct Callers {
    /// The definitions of the symbol asked for.
    pub definitions: Vec<Site>,
    /// Resolved callers first, then the ambiguous ones, each in path and line order.
    pub callers: Vec<Caller>,
    /// The definition or caller candidate set may have been capped.
    pub truncated: bool,
}

/// The chunks whose calls reach a definition of `symbol`. A chunk is not its own caller.
pub async fn callers(
    pool: &PgPool,
    scope: &Scope<'_>,
    symbol: &Symbol,
) -> Result<Callers, AppError> {
    let mut connection = pool.acquire().await?;
    let name = symbol.name.as_str();
    let all = sites_with(
        &mut connection,
        scope,
        "defines",
        std::slice::from_ref(&symbol.name),
        MAX_DEFINITIONS,
    )
    .await?;
    let targets: HashSet<i64> = all
        .iter()
        .filter(|s| match &symbol.qualifier {
            Some(q) => qualifier_matches(q, name, s),
            None => true,
        })
        .map(|s| s.chunk_id)
        .collect();
    // Never infer uniqueness from a capped set of definitions.
    if all.len() == MAX_DEFINITIONS as usize {
        return Ok(Callers {
            definitions: all
                .into_iter()
                .filter(|s| targets.contains(&s.chunk_id))
                .collect(),
            truncated: true,
            ..Default::default()
        });
    }
    if targets.is_empty() {
        return Ok(Callers::default());
    }
    let limit = i64::try_from(MAX_CALLERS + 1).unwrap_or(i64::MAX);
    let mut sites = sites_with(&mut connection, scope, "calls", &call_keys(name), limit).await?;
    let truncated = sites.len() > MAX_CALLERS;
    sites.truncate(MAX_CALLERS);

    let refs: Vec<&Site> = all.iter().collect();
    let mut resolved = Vec::new();
    let mut ambiguous = Vec::new();
    for site in sites {
        let Some((picks, resolution)) = resolve(name, &site, &refs) else {
            continue;
        };
        let picked: Vec<i64> = picks.iter().map(|&i| refs[i].chunk_id).collect();
        if !picked.iter().any(|id| targets.contains(id)) || picked == [site.chunk_id] {
            continue;
        }
        let caller = Caller {
            site,
            resolution,
            definitions: picked.len(),
        };
        if resolution == Resolution::Ambiguous {
            ambiguous.push(caller);
        } else {
            resolved.push(caller);
        }
    }
    resolved.extend(ambiguous);
    Ok(Callers {
        definitions: all
            .into_iter()
            .filter(|s| targets.contains(&s.chunk_id))
            .collect(),
        callers: resolved,
        truncated,
    })
}

/// Explicit symbol references, not incidental English words that happen to be code names.
/// A short name-only query or a call/definition question also accepts plain identifiers.
pub fn query_symbols(query: &str) -> Vec<Symbol> {
    const INTENT: &[&str] = &[
        "call",
        "calls",
        "called",
        "caller",
        "callers",
        "callee",
        "callees",
        "define",
        "defined",
        "definition",
        "definitions",
        "reach",
        "reaches",
        "connect",
        "connects",
        "connected",
        "invoke",
        "invokes",
        "invoked",
    ];
    let mut candidates: Vec<(Symbol, bool)> = Vec::new();
    let mut graph_intent = false;
    for token in query
        .split(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | ':' | '.')))
        .take(128)
    {
        let token = token.trim_matches(|c| c == ':' || c == '.');
        let Some(symbol) = parse_symbol(token) else {
            continue;
        };
        let lower = symbol.name.to_lowercase();
        if INTENT.contains(&lower.as_str()) {
            graph_intent = true;
            continue;
        }
        if symbol.name.chars().count() < 3
            || STOP_WORDS.contains(&symbol.name.to_lowercase().as_str())
            || candidates.iter().any(|(s, _)| s == &symbol)
        {
            continue;
        }
        let explicit = symbol.qualifier.is_some()
            || symbol.name.contains('_')
            || symbol.name.chars().any(|c| c.is_ascii_digit())
            || (symbol.name.chars().any(char::is_uppercase)
                && symbol.name.chars().any(char::is_lowercase))
            || query.contains(&format!("`{token}`"));
        candidates.push((symbol, explicit));
    }
    let name_only = candidates.len() == 1;
    candidates
        .into_iter()
        .filter(|(_, explicit)| *explicit || graph_intent || name_only)
        .map(|(symbol, _)| symbol)
        .take(RELATED_NAMES)
        .collect()
}

/// The name a `calls` entry is a call to: `index_text` of `store::index_text` and of `::index_text`.
fn called_name(entry: &str) -> &str {
    entry.rsplit("::").next().unwrap_or(entry)
}

/// The fourth list of search: chunks that define a symbol the query names, in query order, then
/// the chunks that call them, then the chunks they call. Only calls tied to one of those
/// definitions count, not ambiguous ones. A name with more than 5 definitions is skipped as too
/// common. Empty, after one query at most, when the query names no indexed symbol.
pub async fn related(pool: &PgPool, scope: &Scope<'_>, query: &str) -> Result<Vec<i64>, AppError> {
    let mut connection = pool.acquire().await?;
    related_on(&mut connection, scope, query).await
}

/// Uses the search's already acquired connection through expansion and final hit loading.
pub(crate) async fn related_on(
    connection: &mut PgConnection,
    scope: &Scope<'_>,
    query: &str,
) -> Result<Vec<i64>, AppError> {
    let symbols = query_symbols(query);
    if symbols.is_empty() {
        return Ok(Vec::new());
    }
    let mut names: Vec<String> = symbols.iter().map(|s| s.name.clone()).collect();
    names.dedup();
    let defs = sites_with(connection, scope, "defines", &names, RELATED_BRANCH).await?;
    if defs.len() == RELATED_BRANCH as usize {
        return Ok(Vec::new());
    }

    let mut out: Vec<i64> = Vec::new();
    // (name, the indexes in `defs` of all its definitions, the ids of those the query means)
    let mut matched: Vec<(&str, Vec<usize>, HashSet<i64>)> = Vec::new();
    for symbol in &symbols {
        let name = symbol.name.as_str();
        let all: Vec<usize> = (0..defs.len())
            .filter(|&i| defs[i].defines.iter().any(|d| d == name))
            .collect();
        let meant: Vec<usize> = all
            .iter()
            .copied()
            .filter(|&i| match &symbol.qualifier {
                Some(q) => qualifier_matches(q, name, &defs[i]),
                None => true,
            })
            .collect();
        if meant.is_empty() || meant.len() > RELATED_DEFINITIONS_PER_NAME {
            continue;
        }
        out.extend(meant.iter().map(|&i| defs[i].chunk_id));
        matched.push((name, all, meant.iter().map(|&i| defs[i].chunk_id).collect()));
    }
    if matched.is_empty() {
        return Ok(Vec::new());
    }
    let mut seen = HashSet::new();
    out.retain(|id| seen.insert(*id));
    if out.len() >= RELATED_LIMIT {
        out.truncate(RELATED_LIMIT);
        return Ok(out);
    }

    let caller_keys: Vec<String> = matched
        .iter()
        .flat_map(|(name, _, _)| call_keys(name))
        .collect();
    let matched_names: HashSet<&str> = matched.iter().map(|(n, _, _)| *n).collect();
    let mut callee_names: Vec<String> = Vec::new();
    let mut callee_seen = HashSet::new();
    'names: for (_, _, meant) in &matched {
        for def in defs.iter().filter(|d| meant.contains(&d.chunk_id)) {
            for entry in &def.calls {
                if callee_names.len() == RELATED_CALLEE_NAMES {
                    break 'names;
                }
                let name = called_name(entry);
                if !matched_names.contains(name) && callee_seen.insert(name) {
                    callee_names.push(name.to_string());
                }
            }
        }
    }
    // Reuse a pooled connection rather than opening another one on the first search.
    let callers = sites_with(connection, scope, "calls", &caller_keys, RELATED_BRANCH).await?;

    for caller in &callers {
        for (name, all, meant) in &matched {
            let candidates: Vec<&Site> = all.iter().map(|&i| &defs[i]).collect();
            if let Some((picks, resolution)) = resolve(name, caller, &candidates)
                && resolution != Resolution::Ambiguous
                && picks
                    .iter()
                    .any(|&i| meant.contains(&candidates[i].chunk_id))
                && !picks
                    .iter()
                    .all(|&i| candidates[i].chunk_id == caller.chunk_id)
            {
                out.push(caller.chunk_id);
            }
        }
    }
    seen.clear();
    out.retain(|id| seen.insert(*id));
    if out.len() >= RELATED_LIMIT {
        out.truncate(RELATED_LIMIT);
        return Ok(out);
    }
    let callees = sites_with(connection, scope, "defines", &callee_names, RELATED_BRANCH).await?;
    if !callees.is_empty() && callees.len() < RELATED_BRANCH as usize {
        for (_, _, meant) in &matched {
            for def in defs.iter().filter(|d| meant.contains(&d.chunk_id)) {
                for name in def.calls.iter().map(|e| called_name(e)) {
                    let candidates: Vec<&Site> = callees
                        .iter()
                        .filter(|c| c.defines.iter().any(|d| d == name))
                        .collect();
                    if let Some((picks, resolution)) = resolve(name, def, &candidates)
                        && resolution != Resolution::Ambiguous
                    {
                        out.extend(picks.iter().map(|&i| candidates[i].chunk_id));
                    }
                }
            }
        }
    }

    seen.clear();
    out.retain(|id| seen.insert(*id));
    out.truncate(RELATED_LIMIT);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(id: i64, doc: u128, path: &str, defines: &[&str], calls: &[&str]) -> Site {
        Site {
            chunk_id: id,
            document_id: Uuid::from_u128(doc),
            source_id: Uuid::from_u128(1),
            path: Some(path.to_string()),
            defines: defines.iter().map(|s| s.to_string()).collect(),
            calls: calls.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn sym(q: Option<&str>, name: &str) -> Symbol {
        Symbol {
            qualifier: q.map(str::to_string),
            name: name.to_string(),
        }
    }

    #[test]
    fn symbols_parse_with_paths_dots_and_parentheses() {
        assert_eq!(parse_symbol("chunk_file"), Some(sym(None, "chunk_file")));
        assert_eq!(
            parse_symbol(" chunk_file() "),
            Some(sym(None, "chunk_file"))
        );
        assert_eq!(
            parse_symbol("Plan::limits"),
            Some(sym(Some("Plan"), "limits"))
        );
        assert_eq!(
            parse_symbol("crate::chunk::chunk_file"),
            Some(sym(Some("chunk"), "chunk_file"))
        );
        assert_eq!(
            parse_symbol("store.Search"),
            Some(sym(Some("store"), "Search"))
        );
        assert_eq!(parse_symbol("self::helper"), Some(sym(None, "helper")));
        assert_eq!(parse_symbol(""), None);
        assert_eq!(parse_symbol("a b"), None);
        assert_eq!(parse_symbol("Plan::"), Some(sym(None, "Plan")));
    }

    #[test]
    fn module_names_skip_index_files() {
        assert_eq!(
            module_names(Some("src/billing/plans.rs")),
            ["plans", "billing"]
        );
        assert_eq!(module_names(Some("src/chunk/mod.rs")), ["chunk"]);
        assert_eq!(module_names(Some("app/db/__init__.py")), ["db"]);
        assert_eq!(module_names(Some("main.go")), Vec::<&str>::new());
        assert_eq!(module_names(None), Vec::<&str>::new());
    }

    #[test]
    fn the_callers_own_file_wins_then_the_path_then_a_unique_name() {
        let local = site(1, 1, "src/mcp.rs", &["search", "LoreMcp::search"], &[]);
        let module = site(2, 2, "src/search.rs", &["search"], &[]);
        let defs = [&local, &module];

        // A bare call in mcp.rs reaches mcp.rs's own `search`.
        let caller = site(10, 1, "src/mcp.rs", &[], &["search"]);
        assert_eq!(
            resolve("search", &caller, &defs),
            Some((vec![0], Resolution::SameFile))
        );
        // An imported or path call goes where the path points, even from the same file.
        let caller = site(11, 1, "src/mcp.rs", &[], &["search::search", "::search"]);
        assert_eq!(
            resolve("search", &caller, &defs),
            Some((vec![1], Resolution::Imported))
        );
        // A bare call from elsewhere cannot tell the two apart.
        let caller = site(12, 3, "src/http/search.rs", &[], &["search"]);
        assert_eq!(
            resolve("search", &caller, &defs),
            Some((vec![0, 1], Resolution::Ambiguous))
        );
        // With one definition, a bare call from anywhere reaches it.
        assert_eq!(
            resolve("search", &caller, &[&module]),
            Some((vec![0], Resolution::Unique))
        );
    }

    #[test]
    fn a_type_path_points_at_the_types_method() {
        let plan = site(1, 1, "src/plan.rs", &["new", "Plan::new"], &[]);
        let user = site(2, 2, "src/user.rs", &["new", "User::new"], &[]);
        let caller = site(10, 3, "src/main.rs", &[], &["Plan::new", "::new"]);
        assert_eq!(
            resolve("new", &caller, &[&plan, &user]),
            Some((vec![0], Resolution::Imported))
        );
    }

    #[test]
    fn duplicate_local_or_imported_definitions_are_ambiguous() {
        let a = site(1, 1, "src/a/common.rs", &["work"], &[]);
        let b = site(2, 1, "src/a/common.rs", &["work"], &[]);
        let local = site(3, 1, "src/a/common.rs", &[], &["work"]);
        assert_eq!(
            resolve("work", &local, &[&a, &b]),
            Some((vec![0, 1], Resolution::Ambiguous))
        );
        let imported = site(4, 2, "src/caller.rs", &[], &["common::work", "::work"]);
        assert_eq!(
            resolve("work", &imported, &[&a, &b]),
            Some((vec![0, 1], Resolution::Ambiguous))
        );
        let mut foreign = a.clone();
        foreign.source_id = Uuid::from_u128(2);
        assert_eq!(resolve("work", &local, &[&foreign]), None);
    }

    #[test]
    fn a_path_call_to_something_not_indexed_reaches_nothing() {
        let local = site(1, 1, "src/a.rs", &["from"], &[]);
        let caller = site(10, 1, "src/a.rs", &[], &["String::from", "::from"]);
        assert_eq!(resolve("from", &caller, &[&local]), None);
        // A chunk that does not call the name at all.
        let other = site(11, 1, "src/a.rs", &[], &["into"]);
        assert_eq!(resolve("from", &other, &[&local]), None);
        assert_eq!(resolve("from", &caller, &[]), None);
    }

    #[test]
    fn query_symbols_keep_identifiers_and_drop_stop_words() {
        assert_eq!(
            query_symbols("what calls chunk_file? and how does store::index_text get it"),
            [sym(None, "chunk_file"), sym(Some("store"), "index_text"),]
        );
        assert!(query_symbols("how is it").is_empty());
        assert!(
            query_symbols("how are the full-text and vector results merged into one ranking?")
                .is_empty()
        );
        assert_eq!(query_symbols("where checkout"), [sym(None, "checkout")]);
        assert_eq!(query_symbols("what calls persist?"), [sym(None, "persist")]);
        assert_eq!(
            query_symbols("how does checkout reach persist?"),
            [sym(None, "checkout"), sym(None, "persist")]
        );
        for q in [
            "what kinds of documents can lore store?",
            "where are the HTTP routes registered?",
            "what is the default time budget of a deep mode question?",
            "what tables hold indexed documents and chunks?",
            "how is a new API key generated?",
        ] {
            assert!(query_symbols(q).is_empty(), "{q}");
        }
        assert_eq!(
            query_symbols("how does `search` find text?"),
            [sym(None, "search")]
        );
        let many = (0..20)
            .map(|i| format!("name{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(query_symbols(&many).len(), RELATED_NAMES);
    }
}
