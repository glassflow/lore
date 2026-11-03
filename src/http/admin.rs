use std::collections::HashMap;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::{Admin, Scope, generate_key};
use crate::error::AppError;
use crate::http::info::modes;
use crate::http::{AppJson, AppState};
use crate::llm::Models;
use crate::repo;
use crate::repo::progress::PushProgress;

#[derive(Deserialize)]
pub struct CreateTenant {
    name: String,
}

#[derive(Serialize)]
pub struct TenantCreated {
    id: Uuid,
    name: String,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct TenantItem {
    id: Uuid,
    name: String,
    created_at: DateTime<Utc>,
}

#[derive(Serialize)]
pub struct TenantList {
    tenants: Vec<TenantItem>,
}

#[derive(Deserialize, Default)]
struct CreateKey {
    scopes: Option<Vec<String>>,
}

#[derive(Serialize)]
pub struct KeyCreated {
    id: Uuid,
    key: String,
    prefix: String,
    scopes: Vec<Scope>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct KeyItem {
    id: Uuid,
    prefix: String,
    scopes: Vec<String>,
    created_at: DateTime<Utc>,
    revoked_at: Option<DateTime<Utc>>,
}

#[derive(Serialize)]
pub struct KeyList {
    keys: Vec<KeyItem>,
}

/// 1 to 64 characters of `[a-z0-9-]`.
fn valid_tenant_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

pub async fn create_tenant(
    _admin: Admin,
    State(state): State<AppState>,
    AppJson(req): AppJson<CreateTenant>,
) -> Result<(StatusCode, Json<TenantCreated>), AppError> {
    if !valid_tenant_name(&req.name) {
        return Err(AppError::BadRequest(
            "tenant name must be 1 to 64 characters of a-z, 0-9 and -".to_string(),
        ));
    }
    let id = Uuid::new_v4();
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO tenants (id, name) VALUES ($1, $2) ON CONFLICT (name) DO NOTHING RETURNING id",
    )
    .bind(id)
    .bind(&req.name)
    .fetch_optional(&state.pool)
    .await?;
    match inserted {
        Some(id) => Ok((
            StatusCode::CREATED,
            Json(TenantCreated { id, name: req.name }),
        )),
        None => Err(AppError::Conflict(format!(
            "tenant {:?} already exists",
            req.name
        ))),
    }
}

pub async fn list_tenants(
    _admin: Admin,
    State(state): State<AppState>,
) -> Result<Json<TenantList>, AppError> {
    let tenants: Vec<TenantItem> = sqlx::query_as(TENANTS_SQL).fetch_all(&state.pool).await?;
    Ok(Json(TenantList { tenants }))
}

/// Parses the optional scopes of a key request. Missing means both scopes. An empty list or an
/// unknown scope is a 400. Duplicates are dropped.
fn parse_scopes(requested: Option<Vec<String>>) -> Result<Vec<Scope>, AppError> {
    let Some(names) = requested else {
        return Ok(vec![Scope::Ingest, Scope::Read]);
    };
    if names.is_empty() {
        return Err(AppError::BadRequest("scopes must not be empty".to_string()));
    }
    let mut scopes = Vec::new();
    for name in &names {
        let s = Scope::parse(name)?;
        if !scopes.contains(&s) {
            scopes.push(s);
        }
    }
    Ok(scopes)
}

pub async fn create_key(
    _admin: Admin,
    State(state): State<AppState>,
    Path(tenant_id): Path<Uuid>,
    body: Bytes,
) -> Result<(StatusCode, Json<KeyCreated>), AppError> {
    // An empty body means "defaults", so `curl -X POST` works without `-d '{}'`.
    let req: CreateKey = if body.iter().all(u8::is_ascii_whitespace) {
        CreateKey::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|e| AppError::BadRequest(format!("invalid JSON body: {e}")))?
    };
    let scopes = parse_scopes(req.scopes)?;
    let scope_names: Vec<String> = scopes.iter().map(|s| s.to_string()).collect();

    let key = generate_key();
    let id = Uuid::new_v4();
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO api_keys (id, tenant_id, prefix, key_hash, scopes) \
         SELECT $1, t.id, $3, $4, $5 FROM tenants t WHERE t.id = $2 \
         RETURNING id",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(&key.prefix)
    .bind(&key.hash)
    .bind(&scope_names)
    .fetch_optional(&state.pool)
    .await?;
    let id = inserted.ok_or(AppError::NotFound)?;
    Ok((
        StatusCode::CREATED,
        Json(KeyCreated {
            id,
            key: key.plaintext,
            prefix: key.prefix,
            scopes,
        }),
    ))
}

pub async fn list_keys(
    _admin: Admin,
    State(state): State<AppState>,
    Path(tenant_id): Path<Uuid>,
) -> Result<Json<KeyList>, AppError> {
    let exists: Option<Uuid> = sqlx::query_scalar("SELECT id FROM tenants WHERE id = $1")
        .bind(tenant_id)
        .fetch_optional(&state.pool)
        .await?;
    if exists.is_none() {
        return Err(AppError::NotFound);
    }
    let keys: Vec<KeyItem> = sqlx::query_as(
        "SELECT id, prefix, scopes, created_at, revoked_at FROM api_keys \
         WHERE tenant_id = $1 ORDER BY created_at, id",
    )
    .bind(tenant_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(KeyList { keys }))
}

/// Revokes a key. Revoking an already revoked key keeps the first `revoked_at` and is a 204.
pub async fn revoke_key(
    _admin: Admin,
    State(state): State<AppState>,
    Path(key_id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let done =
        sqlx::query("UPDATE api_keys SET revoked_at = COALESCE(revoked_at, now()) WHERE id = $1")
            .bind(key_id)
            .execute(&state.pool)
            .await?;
    if done.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// What the server runs and holds, for the admin page.
#[derive(Serialize)]
pub struct Overview {
    /// The server's clock, so a client can show times relative to it.
    now: DateTime<Utc>,
    server: ServerFacts,
    storage: Storage,
    tenants: Vec<TenantOverview>,
}

#[derive(Serialize)]
pub struct ServerFacts {
    version: String,
    llm: String,
    embedder: String,
    modes: Vec<&'static str>,
    models: Models,
    memory: Memory,
    tenants: usize,
    sources: usize,
    documents: i64,
    chunks: i64,
}

/// The lore process's resident memory. Read from `/proc/self/status`, so only on Linux; both
/// are `None` elsewhere.
#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct Memory {
    rss_bytes: Option<u64>,
    /// The highest resident memory since the process started.
    peak_rss_bytes: Option<u64>,
}

/// How much room the index takes.
#[derive(Serialize)]
pub struct Storage {
    /// The whole Postgres database, `pg_database_size`.
    database_bytes: i64,
    /// Repo mirrors and checkouts under `<data_dir>/repos`.
    repos_bytes: u64,
    /// lore's tables, largest first.
    tables: Vec<TableSize>,
    /// Their indexes, largest first.
    indexes: Vec<IndexSize>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct TableSize {
    name: String,
    /// Postgres's estimate from the last analyze; `None` before the first one.
    rows_estimate: Option<i64>,
    /// Rows and out-of-line values (TOAST), without indexes.
    data_bytes: i64,
    index_bytes: i64,
    total_bytes: i64,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct IndexSize {
    name: String,
    table: String,
    /// `hnsw` (vectors), `gin` (full-text), `btree` and so on.
    method: String,
    bytes: i64,
}

#[derive(Serialize)]
pub struct TenantOverview {
    id: Uuid,
    name: String,
    created_at: DateTime<Utc>,
    sources: Vec<SourceOverview>,
    keys: Vec<KeyItem>,
}

/// A source with what it holds.
#[derive(Serialize, sqlx::FromRow)]
pub struct SourceOverview {
    #[serde(skip_serializing)]
    tenant_id: Uuid,
    id: Uuid,
    name: String,
    kind: String,
    indexed_commit: Option<String>,
    documents: i64,
    chunks: i64,
    /// The later of a repo's last push and the source's newest document write. `None` when
    /// nothing was indexed yet.
    last_indexed_at: Option<DateTime<Utc>>,
    /// A repo's remote, fetched by sync. `None` when it has none.
    remote_url: Option<String>,
    /// When the last sync ended, whether it worked or not.
    last_sync_at: Option<DateTime<Utc>>,
    /// The remote's head at the last sync that worked.
    last_sync_commit: Option<String>,
    /// Why the last sync failed; `None` when it worked.
    last_sync_error: Option<String>,
    /// A repo's mirror and checkout on disk. `None` for other kinds, which keep nothing there.
    #[sqlx(skip)]
    disk_bytes: Option<u64>,
}

#[derive(sqlx::FromRow)]
struct KeyRow {
    tenant_id: Uuid,
    #[sqlx(flatten)]
    key: KeyItem,
}

const TENANTS_SQL: &str = "SELECT id, name, created_at FROM tenants ORDER BY created_at, name";

/// Every source with its document and chunk counts. Chunks are counted per document first,
/// which the `chunks_document` index answers without reading the chunk rows. A source's own
/// `updated_at` counts as an index time only once it has an indexed commit: before that it is
/// when the source was created.
const SOURCES_SQL: &str = "
    SELECT s.tenant_id, s.id, s.name, s.kind, s.indexed_commit,
           s.remote_url, s.last_sync_at, s.last_sync_commit, s.last_sync_error,
           count(d.id) AS documents,
           COALESCE(sum(c.n), 0)::bigint AS chunks,
           GREATEST(CASE WHEN s.indexed_commit IS NOT NULL THEN s.updated_at END,
                    max(d.updated_at)) AS last_indexed_at
    FROM sources s
    LEFT JOIN documents d ON d.source_id = s.id AND d.tenant_id = s.tenant_id
    LEFT JOIN (SELECT document_id, count(*) AS n FROM chunks GROUP BY document_id) c
      ON c.document_id = d.id
    GROUP BY s.id
    ORDER BY s.kind, s.name";

/// The tables of lore's schema with their sizes. `pg_total_relation_size` counts the table, its
/// TOAST table and its indexes.
const TABLES_SQL: &str = "
    SELECT c.relname AS name,
           CASE WHEN c.reltuples < 0 THEN NULL ELSE c.reltuples::bigint END AS rows_estimate,
           pg_total_relation_size(c.oid) - pg_indexes_size(c.oid) AS data_bytes,
           pg_indexes_size(c.oid) AS index_bytes,
           pg_total_relation_size(c.oid) AS total_bytes
    FROM pg_class c
    JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE n.nspname = current_schema() AND c.relkind = 'r'
    ORDER BY total_bytes DESC, name";

const INDEXES_SQL: &str = "
    SELECT i.relname AS name, t.relname AS \"table\", am.amname AS method,
           pg_relation_size(i.oid) AS bytes
    FROM pg_index x
    JOIN pg_class i ON i.oid = x.indexrelid
    JOIN pg_class t ON t.oid = x.indrelid
    JOIN pg_namespace n ON n.oid = t.relnamespace
    JOIN pg_am am ON am.oid = i.relam
    WHERE n.nspname = current_schema()
    ORDER BY bytes DESC, name";

/// Every tenant with its sources and keys, the server's facts and totals, and how much room
/// the index takes. The database reads share one snapshot, so the totals add up. The repo
/// directories are walked on the blocking pool.
pub async fn overview(
    _admin: Admin,
    State(state): State<AppState>,
) -> Result<Json<Overview>, AppError> {
    let data_dir = state.config.data_dir.clone();
    let disk = tokio::task::spawn_blocking(move || repo::disk_usage(&data_dir));

    let mut tx = state.pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let tenants: Vec<TenantItem> = sqlx::query_as(TENANTS_SQL).fetch_all(&mut *tx).await?;
    let mut sources: Vec<SourceOverview> = sqlx::query_as(SOURCES_SQL).fetch_all(&mut *tx).await?;
    let keys: Vec<KeyRow> = sqlx::query_as(
        "SELECT tenant_id, id, prefix, scopes, created_at, revoked_at FROM api_keys \
         ORDER BY created_at, id",
    )
    .fetch_all(&mut *tx)
    .await?;
    let database_bytes: i64 = sqlx::query_scalar("SELECT pg_database_size(current_database())")
        .fetch_one(&mut *tx)
        .await?;
    let tables: Vec<TableSize> = sqlx::query_as(TABLES_SQL).fetch_all(&mut *tx).await?;
    let indexes: Vec<IndexSize> = sqlx::query_as(INDEXES_SQL).fetch_all(&mut *tx).await?;
    tx.commit().await?;

    let disk = disk
        .await
        .map_err(|e| AppError::Internal(anyhow::Error::from(e).context("disk usage task")))?;
    for s in sources.iter_mut().filter(|s| s.kind == "repo") {
        s.disk_bytes = Some(disk.get(&s.id).copied().unwrap_or(0));
    }
    let storage = Storage {
        database_bytes,
        repos_bytes: disk.values().sum(),
        tables,
        indexes,
    };

    let server = ServerFacts {
        version: state.config.version.clone(),
        llm: state.llm.name().to_string(),
        embedder: state.embedder.name().to_string(),
        modes: modes(state.llm.as_ref()),
        models: state.models.clone(),
        memory: process_memory(),
        tenants: tenants.len(),
        sources: sources.len(),
        documents: sources.iter().map(|s| s.documents).sum(),
        chunks: sources.iter().map(|s| s.chunks).sum(),
    };
    let mut out: Vec<TenantOverview> = tenants
        .into_iter()
        .map(|t| TenantOverview {
            id: t.id,
            name: t.name,
            created_at: t.created_at,
            sources: Vec::new(),
            keys: Vec::new(),
        })
        .collect();
    let at: HashMap<Uuid, usize> = out.iter().enumerate().map(|(i, t)| (t.id, i)).collect();
    for s in sources {
        if let Some(&i) = at.get(&s.tenant_id) {
            out[i].sources.push(s);
        }
    }
    for k in keys {
        if let Some(&i) = at.get(&k.tenant_id) {
            out[i].keys.push(k.key);
        }
    }
    Ok(Json(Overview {
        now: Utc::now(),
        server,
        storage,
        tenants: out,
    }))
}

/// The process's resident memory now and at its peak, from `/proc/self/status` (Linux only).
fn process_memory() -> Memory {
    std::fs::read_to_string("/proc/self/status")
        .map(|text| parse_proc_status(&text))
        .unwrap_or_default()
}

/// Reads `VmRSS` and `VmHWM` (resident memory now and at its peak) from the text of
/// `/proc/self/status`, where they are given in KiB.
fn parse_proc_status(text: &str) -> Memory {
    let field = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.trim().strip_suffix("kB"))
            .and_then(|n| n.trim().parse::<u64>().ok())
            .map(|kib| kib * 1024)
    };
    Memory {
        rss_bytes: field("VmRSS:"),
        peak_rss_bytes: field("VmHWM:"),
    }
}

/// A push with its tenant's name.
#[derive(Serialize)]
pub struct PushItem {
    /// `None` when the tenant no longer exists.
    tenant: Option<String>,
    #[serde(flatten)]
    push: PushProgress,
}

#[derive(Serialize)]
pub struct PushList {
    now: DateTime<Utc>,
    /// When the server started: pushes before that are not known.
    since: DateTime<Utc>,
    /// Newest first.
    running: Vec<PushItem>,
    /// The last pushes that ended, newest first.
    recent: Vec<PushItem>,
}

/// The running repo pushes with their progress, and the recent ones that ended.
pub async fn list_pushes(
    _admin: Admin,
    State(state): State<AppState>,
) -> Result<Json<PushList>, AppError> {
    let snap = state.pushes.snapshot();
    let mut ids: Vec<Uuid> = snap
        .running
        .iter()
        .chain(&snap.recent)
        .map(|p| p.tenant_id)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    let names: HashMap<Uuid, String> = if ids.is_empty() {
        HashMap::new()
    } else {
        sqlx::query_as::<_, (Uuid, String)>("SELECT id, name FROM tenants WHERE id = ANY($1)")
            .bind(&ids)
            .fetch_all(&state.pool)
            .await?
            .into_iter()
            .collect()
    };
    let item = |push: PushProgress| PushItem {
        tenant: names.get(&push.tenant_id).cloned(),
        push,
    };
    Ok(Json(PushList {
        now: Utc::now(),
        since: snap.since,
        running: snap.running.into_iter().map(item).collect(),
        recent: snap.recent.into_iter().map(item).collect(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_names() {
        assert!(valid_tenant_name("demo"));
        assert!(valid_tenant_name("a"));
        assert!(valid_tenant_name("glass-flow-2"));
        assert!(valid_tenant_name(&"a".repeat(64)));
        assert!(!valid_tenant_name(""));
        assert!(!valid_tenant_name(&"a".repeat(65)));
        assert!(!valid_tenant_name("Demo"));
        assert!(!valid_tenant_name("de mo"));
        assert!(!valid_tenant_name("de_mo"));
        assert!(!valid_tenant_name("démo"));
    }

    #[test]
    fn scope_defaults_and_validation() {
        assert_eq!(
            parse_scopes(None).unwrap(),
            vec![Scope::Ingest, Scope::Read]
        );
        assert_eq!(
            parse_scopes(Some(vec!["read".into(), "read".into()])).unwrap(),
            vec![Scope::Read]
        );
        assert!(parse_scopes(Some(vec![])).is_err());
        assert!(parse_scopes(Some(vec!["root".into()])).is_err());
    }

    #[test]
    fn memory_comes_from_proc_status() {
        let text = "Name:\tlore\nVmPeak:\t 9999 kB\nVmHWM:\t  409600 kB\nVmRSS:\t  262144 kB\n";
        assert_eq!(
            parse_proc_status(text),
            Memory {
                rss_bytes: Some(262_144 * 1024),
                peak_rss_bytes: Some(409_600 * 1024),
            }
        );
        assert_eq!(parse_proc_status(""), Memory::default());
        assert_eq!(
            parse_proc_status("VmRSS:\tlots kB\n").rss_bytes,
            None,
            "a value that is not a number is left out"
        );
    }
}
