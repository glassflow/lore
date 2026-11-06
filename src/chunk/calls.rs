//! Definitions and call sites of a parsed code file, attached to its chunks: what the
//! `definition` and `callers` tools and the symbol list of search read. Calls are kept as names
//! with, where the code says it, the module or type they go through. Nothing is resolved here.

use std::collections::HashMap;

use tree_sitter::Node;

use super::code::{Source, is_container_kind, is_item_kind, node_text, symbols_of};
use super::{Chunk, Language};

/// A chunk keeps at most this many `defines` and this many `calls` entries.
pub(super) const MAX_NAMES_PER_CHUNK: usize = 256;
/// Longer names are not kept (minified code, generated tables).
pub(super) const MAX_NAME_BYTES: usize = 128;

/// What a file imports. `items` maps a name imported on its own (`use crate::store::index_text`,
/// `from app.db import get_user`, `import { limits } from './plans'`) to the module it comes from;
/// `modules` maps a module alias (`import app.db as db`, `import * as plans from './plans'`, a Go
/// import) to the module's own name.
#[derive(Default)]
struct Imports {
    items: HashMap<String, String>,
    modules: HashMap<String, String>,
    originals: HashMap<String, String>,
}

#[derive(Clone, Default)]
struct Context {
    /// The type, trait, class or module the node is declared in.
    container: Option<String>,
    /// Inside a function body: only nested functions and classes count as definitions there.
    in_function: bool,
}

/// Fills `defines` and `calls` of `chunks`, which were cut from the file whose syntax tree is
/// `root`. Each definition and call goes to the smallest chunk holding its first line, so a
/// container's header chunk and its member chunks do not repeat each other.
pub(super) fn annotate(lang: Language, src: &Source, root: Node<'_>, chunks: &mut [Chunk]) {
    let imports = imports(lang, src, root);
    let mut defines: Vec<(u32, Vec<String>)> = Vec::new();
    let mut calls: Vec<(u32, Vec<String>)> = Vec::new();
    // An explicit stack: a deeply nested file (minified code) must not overflow the call stack.
    let mut stack = vec![(root, Context::default())];
    while let Some((node, ctx)) = stack.pop() {
        let kind = node.kind();
        let line = super::line_number(node.start_position().row);
        let mut inner = ctx.clone();
        if is_definition(lang, kind, ctx.in_function) {
            let names = definition_names(src, node, ctx.container.as_deref());
            if is_container_kind(kind) || matches!(kind, "trait_item" | "mod_item") {
                inner.container = names.first().cloned();
            }
            if !names.is_empty() {
                defines.push((line, names));
            }
        }
        if is_function(kind) {
            inner.in_function = true;
        }
        if matches!(kind, "call_expression" | "call" | "new_expression")
            && let Some(names) = call_names(lang, src, node, &imports)
        {
            calls.push((line, names));
        }
        let mut cursor = node.walk();
        let children: Vec<Node> = node.named_children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            stack.push((child, inner.clone()));
        }
    }
    attach(chunks, defines, |c| &mut c.defines);
    attach(chunks, calls, |c| &mut c.calls);
}

fn attach(
    chunks: &mut [Chunk],
    sites: Vec<(u32, Vec<String>)>,
    field: impl Fn(&mut Chunk) -> &mut Vec<String>,
) {
    for (line, names) in sites {
        let best = chunks
            .iter()
            .enumerate()
            .filter(|(_, c)| c.start_line <= line && line <= c.end_line)
            .min_by_key(|(i, c)| (c.end_line.saturating_sub(c.start_line), *i))
            .map(|(i, _)| i);
        let Some(chunk) = best.and_then(|i| chunks.get_mut(i)) else {
            continue;
        };
        let list = field(chunk);
        for name in names {
            if list.len() >= MAX_NAMES_PER_CHUNK {
                break;
            }
            if name.len() <= MAX_NAME_BYTES && !list.contains(&name) {
                list.push(name);
            }
        }
    }
}

fn is_function(kind: &str) -> bool {
    matches!(
        kind,
        "function_item"
            | "function_declaration"
            | "generator_function_declaration"
            | "function_definition"
            | "method_definition"
            | "method_declaration"
            | "arrow_function"
            | "function_expression"
            | "generator_function"
            | "closure_expression"
            | "func_literal"
            | "lambda"
    )
}

fn is_definition(lang: Language, kind: &str, in_function: bool) -> bool {
    if in_function {
        return matches!(
            kind,
            "function_item"
                | "function_declaration"
                | "generator_function_declaration"
                | "function_definition"
                | "class_definition"
                | "class_declaration"
        );
    }
    is_item_kind(lang, kind)
        || matches!(
            kind,
            "function_signature_item"
                | "method_definition"
                | "method_declaration"
                | "generator_function_declaration"
        )
}

/// The names a definition is called by: its own name, and `Container::name` inside a type,
/// trait, class or module (or on a Go method's receiver type).
fn definition_names(src: &Source, node: Node<'_>, container: Option<&str>) -> Vec<String> {
    let mut names: Vec<String> = symbols_of(src, node)
        .iter()
        .filter_map(|s| base_name(s))
        .collect();
    let container = if node.kind() == "method_declaration" {
        go_receiver(src, node)
    } else {
        container.map(str::to_string)
    };
    if let (Some(container), [name]) = (container, names.as_slice()) {
        let qualified = format!("{container}::{name}");
        names.push(qualified);
    }
    names
}

/// `Wrapper<T>` as `Wrapper`, `fmt::Display` as `Display`. `None` for what is not a plain name
/// (`dyn Trait`, `&str`, a tuple type).
fn base_name(s: &str) -> Option<String> {
    let s = s.split('<').next().unwrap_or(s).trim();
    let s = s.rsplit("::").next().unwrap_or(s);
    is_name(s).then(|| s.to_string())
}

fn is_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_NAME_BYTES
        && s.chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
        && !s.starts_with(|c: char| c.is_ascii_digit())
}

/// Receivers that say nothing about where a method is defined.
fn is_self(s: &str) -> bool {
    matches!(s, "self" | "Self" | "this" | "cls" | "super" | "crate")
}

/// The receiver type of a Go method: `Plan` for `func (p *Plan) Limits()`.
fn go_receiver(src: &Source, node: Node<'_>) -> Option<String> {
    let mut stack = vec![node.child_by_field_name("receiver")?];
    while let Some(n) = stack.pop() {
        if n.kind() == "type_identifier" {
            return base_name(node_text(src, n));
        }
        let mut cursor = n.walk();
        let children: Vec<Node> = n.named_children(&mut cursor).collect();
        stack.extend(children.into_iter().rev());
    }
    None
}

/// The entries a call adds to `calls`: `name`, or `qualifier::name` and `::name` when the call
/// says where its target lives.
fn call_names(
    lang: Language,
    src: &Source,
    node: Node<'_>,
    imports: &Imports,
) -> Option<Vec<String>> {
    let func = node
        .child_by_field_name("function")
        .or_else(|| node.child_by_field_name("constructor"))?;
    let (mut name, mut qualifier) = callee(lang, src, func, imports, 0)?;
    let mut bare = func;
    for _ in 0..4 {
        if bare.kind() != "generic_function" {
            break;
        }
        bare = bare.child_by_field_name("function")?;
    }
    if matches!(bare.kind(), "identifier" | "type_identifier") {
        qualifier = qualifier.or_else(|| imports.items.get(&name).cloned());
        name = imports.originals.get(&name).cloned().unwrap_or(name);
    }
    if !is_name(&name) {
        return None;
    }
    match qualifier.filter(|q| is_name(q) && !is_self(q)) {
        Some(q) => Some(vec![format!("{q}::{name}"), format!("::{name}")]),
        None => Some(vec![name]),
    }
}

/// The called name, and the module or type the call goes through when the code names one. A
/// method called on a value (`self.store.get()`, `rows.next()`) has no qualifier: its type is
/// unknown without type inference.
fn callee(
    lang: Language,
    src: &Source,
    f: Node<'_>,
    imports: &Imports,
    depth: usize,
) -> Option<(String, Option<String>)> {
    let text = |n: Node<'_>| node_text(src, n).to_string();
    // Go, Python and JS: `x.f()` goes through a module only when `x` is an imported module, or
    // (Python, JS) an imported name such as a class.
    let through_import = |object: Node<'_>| -> Option<String> {
        if object.kind() != "identifier" {
            return None;
        }
        let o = text(object);
        if let Some(module) = imports.modules.get(&o) {
            return Some(module.clone());
        }
        (lang != Language::Go && imports.items.contains_key(&o))
            .then(|| imports.originals.get(&o).cloned().unwrap_or(o))
    };
    match f.kind() {
        "identifier" | "type_identifier" => Some((text(f), None)),
        // Rust `path::name` and `Type::name`.
        "scoped_identifier" => {
            let name = text(f.child_by_field_name("name")?);
            let qualifier = f
                .child_by_field_name("path")
                .and_then(|p| last_segment(src, p))
                .map(|q| imports.modules.get(&q).cloned().unwrap_or(q));
            Some((name, qualifier))
        }
        "generic_function" if depth < 4 => callee(
            lang,
            src,
            f.child_by_field_name("function")?,
            imports,
            depth + 1,
        ),
        "field_expression" => Some((text(f.child_by_field_name("field")?), None)),
        "selector_expression" => Some((
            text(f.child_by_field_name("field")?),
            f.child_by_field_name("operand").and_then(through_import),
        )),
        "attribute" => Some((
            text(f.child_by_field_name("attribute")?),
            f.child_by_field_name("object").and_then(through_import),
        )),
        "member_expression" => Some((
            text(f.child_by_field_name("property")?),
            f.child_by_field_name("object").and_then(through_import),
        )),
        _ => None,
    }
}

/// The last name of a Rust path: `store` of `crate::store`, `Plan` of `Plan` or `Vec<u8>`.
fn last_segment(src: &Source, mut n: Node<'_>) -> Option<String> {
    loop {
        match n.kind() {
            "identifier" | "type_identifier" => {
                let t = node_text(src, n);
                return (!is_self(t)).then(|| t.to_string());
            }
            "scoped_identifier" | "scoped_type_identifier" => {
                n = n.child_by_field_name("name")?;
            }
            "generic_type" => n = n.child_by_field_name("type")?,
            _ => return None,
        }
    }
}

fn imports(lang: Language, src: &Source, root: Node<'_>) -> Imports {
    let mut out = Imports::default();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        match (lang, node.kind()) {
            (Language::Rust, "use_declaration") => {
                if let Some(arg) = node.child_by_field_name("argument") {
                    rust_use(src, arg, &[], &mut out, 0);
                }
                continue;
            }
            (Language::Python, "import_from_statement") => {
                python_from_import(src, node, &mut out);
                continue;
            }
            (Language::Python, "import_statement") => {
                let mut cursor = node.walk();
                for name in node.children_by_field_name("name", &mut cursor) {
                    if name.kind() == "aliased_import"
                        && let (Some(module), Some(alias)) = (
                            name.child_by_field_name("name"),
                            name.child_by_field_name("alias"),
                        )
                        && let Some(seg) = dotted_last(node_text(src, module))
                    {
                        out.modules.insert(node_text(src, alias).to_string(), seg);
                    }
                }
                continue;
            }
            (Language::TypeScript | Language::Tsx | Language::JavaScript, "import_statement") => {
                js_import(src, node, &mut out);
                continue;
            }
            (Language::Go, "import_spec") => {
                go_import(src, node, &mut out);
                continue;
            }
            _ => {}
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    out
}

/// The last two path segments, all the name-only resolver needs. Bound work on deep paths.
fn rust_segments(src: &Source, mut n: Node<'_>) -> Vec<String> {
    let mut segs = Vec::new();
    while segs.len() < 2 {
        match n.kind() {
            "scoped_identifier" => {
                if let Some(name) = n.child_by_field_name("name") {
                    segs.push(node_text(src, name).to_string());
                }
                let Some(path) = n.child_by_field_name("path") else {
                    break;
                };
                n = path;
            }
            "identifier" | "crate" | "self" | "super" => {
                segs.push(node_text(src, n).to_string());
                break;
            }
            _ => break,
        }
    }
    segs.reverse();
    segs
}

fn last_module(segs: &[String]) -> Option<String> {
    segs.iter()
        .rev()
        .find(|s| !is_self(s))
        .filter(|s| is_name(s))
        .cloned()
}

fn rust_use(src: &Source, n: Node<'_>, prefix: &[String], out: &mut Imports, depth: usize) {
    if depth >= 64 {
        return;
    }
    match n.kind() {
        "scoped_identifier" | "identifier" => {
            let mut segs = prefix.to_vec();
            segs.extend(rust_segments(src, n));
            if let Some(name) = segs.pop()
                && is_name(&name)
                && !is_self(&name)
                && let Some(module) = last_module(&segs)
            {
                out.items.insert(name, module);
            }
        }
        "use_as_clause" => {
            let (Some(path), Some(alias)) = (
                n.child_by_field_name("path"),
                n.child_by_field_name("alias"),
            ) else {
                return;
            };
            let mut segs = prefix.to_vec();
            segs.extend(rust_segments(src, path));
            let alias = node_text(src, alias).to_string();
            let Some(name) = segs.pop() else {
                return;
            };
            // The alias may name a module (`use crate::store as db`) or an item.
            if is_name(&name) && !is_self(&name) {
                out.modules.insert(alias.clone(), name.clone());
                out.originals.insert(alias.clone(), name);
            }
            if let Some(module) = last_module(&segs) {
                out.items.insert(alias, module);
            }
        }
        "scoped_use_list" => {
            let mut segs = prefix.to_vec();
            if let Some(path) = n.child_by_field_name("path") {
                segs.extend(rust_segments(src, path));
            }
            if let Some(list) = n.child_by_field_name("list") {
                rust_use(src, list, &segs, out, depth + 1);
            }
        }
        "use_list" => {
            let mut cursor = n.walk();
            for child in n.named_children(&mut cursor) {
                rust_use(src, child, prefix, out, depth + 1);
            }
        }
        _ => {}
    }
}

/// The last part of `a.b.c`, or of a relative `..b`.
fn dotted_last(s: &str) -> Option<String> {
    s.rsplit('.')
        .find(|p| !p.is_empty())
        .map(str::trim)
        .filter(|p| is_name(p))
        .map(str::to_string)
}

fn python_from_import(src: &Source, node: Node<'_>, out: &mut Imports) {
    let Some(module) = node
        .child_by_field_name("module_name")
        .and_then(|m| dotted_last(node_text(src, m)))
    else {
        return;
    };
    let mut cursor = node.walk();
    for name in node.children_by_field_name("name", &mut cursor) {
        let (imported, alias) = match name.kind() {
            "aliased_import" => (
                name.child_by_field_name("name"),
                name.child_by_field_name("alias"),
            ),
            _ => (Some(name), None),
        };
        let Some(imported) = imported.and_then(|i| dotted_last(node_text(src, i))) else {
            continue;
        };
        let alias = alias.map_or_else(|| imported.clone(), |a| node_text(src, a).to_string());
        out.originals.insert(alias.clone(), imported);
        out.items.insert(alias, module.clone());
    }
}

/// The module name of an import path: `plans` of `./billing/plans.ts`, `billing` of
/// `./billing/index`, `react` of `react`.
fn path_module(path: &str) -> Option<String> {
    let mut parts = path.split('/').filter(|p| !p.is_empty() && *p != ".");
    let mut last = parts.next_back()?;
    let stem = |p: &'_ str| -> String {
        match p.rsplit_once('.') {
            Some((stem, _)) if !stem.is_empty() => stem.to_string(),
            _ => p.to_string(),
        }
    };
    let mut module = stem(last);
    if module == "index" {
        last = parts.next_back()?;
        module = stem(last);
    }
    let module = module.trim_start_matches('@').to_string();
    is_name(&module).then_some(module)
}

fn js_import(src: &Source, node: Node<'_>, out: &mut Imports) {
    let Some(module) = node
        .child_by_field_name("source")
        .map(|s| node_text(src, s).trim_matches(|c| c == '"' || c == '\'' || c == '`'))
        .and_then(path_module)
    else {
        return;
    };
    let mut cursor = node.walk();
    let Some(clause) = node
        .named_children(&mut cursor)
        .find(|c| c.kind() == "import_clause")
    else {
        return;
    };
    let mut cursor = clause.walk();
    for part in clause.named_children(&mut cursor) {
        match part.kind() {
            "identifier" => {
                out.items
                    .insert(node_text(src, part).to_string(), module.clone());
            }
            "namespace_import" => {
                let mut c = part.walk();
                if let Some(id) = part
                    .named_children(&mut c)
                    .find(|n| n.kind() == "identifier")
                {
                    out.modules
                        .insert(node_text(src, id).to_string(), module.clone());
                }
            }
            "named_imports" => {
                let mut c = part.walk();
                for spec in part.named_children(&mut c) {
                    if spec.kind() != "import_specifier" {
                        continue;
                    }
                    let local = spec
                        .child_by_field_name("alias")
                        .or_else(|| spec.child_by_field_name("name"));
                    if let Some(local) = local {
                        if let Some(name) = spec.child_by_field_name("name") {
                            out.originals.insert(
                                node_text(src, local).to_string(),
                                node_text(src, name).to_string(),
                            );
                        }
                        out.items
                            .insert(node_text(src, local).to_string(), module.clone());
                    }
                }
            }
            _ => {}
        }
    }
}

fn go_import(src: &Source, node: Node<'_>, out: &mut Imports) {
    let Some(module) = node
        .child_by_field_name("path")
        .map(|p| node_text(src, p).trim_matches(|c| c == '"' || c == '`'))
        .and_then(|p| p.rsplit('/').next())
        .filter(|m| is_name(m))
        .map(str::to_string)
    else {
        return;
    };
    let alias = match node.child_by_field_name("name") {
        Some(n) if n.kind() == "package_identifier" => node_text(src, n).to_string(),
        // A dot or blank import names nothing to call through.
        Some(_) => return,
        None => module.clone(),
    };
    out.modules.insert(alias, module);
}

#[cfg(test)]
mod tests {
    use crate::chunk::{Chunk, chunk_file};

    fn find<'a>(chunks: &'a [Chunk], symbol: &str) -> &'a Chunk {
        chunks
            .iter()
            .find(|c| c.symbols.iter().any(|s| s == symbol))
            .unwrap_or_else(|| panic!("no chunk for {symbol}: {chunks:#?}"))
    }

    fn strs(v: &[String]) -> Vec<&str> {
        v.iter().map(String::as_str).collect()
    }

    #[test]
    fn rust_calls_keep_paths_and_imports_and_methods_are_bare() {
        let src = "\
use crate::store::{self, index_text};
use super::lines::windows as win;

pub fn index(state: &State) {
    let chunks = chunk_file(path, text);
    index_text(state, chunks);
    store::delete(state);
    Plan::new();
    Self::helper();
    win(1);
    state.pool.begin();
    std::fs::read(path).unwrap();
}
";
        let chunks = chunk_file("src/repo.rs", src);
        let item = find(&chunks, "index");
        assert_eq!(strs(&item.defines), ["index"]);
        assert_eq!(
            strs(&item.calls),
            [
                "chunk_file",
                "store::index_text",
                "::index_text",
                "store::delete",
                "::delete",
                "Plan::new",
                "::new",
                "helper",
                "lines::windows",
                "::windows",
                "begin",
                // The outer call of a chain starts on the same line and is seen first.
                "unwrap",
                "fs::read",
                "::read",
            ]
        );
    }

    #[test]
    fn methods_in_an_impl_are_defined_by_name_and_as_type_method() {
        let src = "\
pub struct Plan;

impl<T> Wrapper<T> {
    pub fn new() -> Self { Self::make() }
    fn make() -> Self { todo!() }
}

trait Store {
    fn get(&self);
}
";
        let chunks = chunk_file("src/plan.rs", src);
        assert_eq!(strs(&find(&chunks, "Plan").defines), ["Plan"]);
        // The chunk keeps the type as written; the definition is the plain name.
        let imp = find(&chunks, "Wrapper<T>");
        assert_eq!(
            strs(&imp.defines),
            ["Wrapper", "new", "Wrapper::new", "make", "Wrapper::make"]
        );
        assert_eq!(strs(&imp.calls), ["make"]);
        assert_eq!(
            strs(&find(&chunks, "Store").defines),
            ["Store", "get", "Store::get"]
        );
    }

    #[test]
    fn a_definition_goes_to_the_smallest_chunk_only() {
        // A big impl is split into a header and one chunk per method; the first methods start
        // inside the header's lines.
        let mut src = String::from("impl Plan {\n");
        for m in 0..10 {
            src.push_str(&format!("    fn method{m}(&self) -> u32 {{\n"));
            for k in 0..15 {
                src.push_str(&format!("        let v{k} = helper{m}();\n"));
            }
            src.push_str("        0\n    }\n\n");
        }
        src.push_str("}\n");
        let chunks = chunk_file("plan.rs", &src);
        let header = &chunks[0];
        assert_eq!(strs(&header.defines), ["Plan"]);
        assert!(header.calls.is_empty(), "{:?}", header.calls);
        let m0 = find(&chunks, "Plan::method0");
        assert_eq!(strs(&m0.defines), ["method0", "Plan::method0"]);
        assert_eq!(strs(&m0.calls), ["helper0"]);
        let total: usize = chunks
            .iter()
            .map(|c| c.defines.iter().filter(|d| *d == "method1").count())
            .sum();
        assert_eq!(total, 1);
    }

    #[test]
    fn go_calls_through_imported_packages_are_qualified() {
        let src = "\
package api

import (
\t\"fmt\"
\tdb \"example.com/app/internal/store\"
)

func (h *UserHandler) Get(id int) {
\tu := db.FindUser(id)
\th.render(u)
\tfmt.Println(helper())
}
";
        let chunks = chunk_file("api/handlers.go", src);
        let get = find(&chunks, "Get");
        assert_eq!(strs(&get.defines), ["Get", "UserHandler::Get"]);
        assert_eq!(
            strs(&get.calls),
            [
                "store::FindUser",
                "::FindUser",
                "render",
                "fmt::Println",
                "::Println",
                "helper"
            ]
        );
    }

    #[test]
    fn python_imports_and_class_methods() {
        let src = "\
from app.db import get_user as fetch
import app.billing as billing


class Handler:
    def handle(self, uid):
        user = fetch(uid)
        self.render(user)
        billing.charge(user)
        return Report(user)

    def render(self, user):
        pass
";
        let chunks = chunk_file("app/api.py", src);
        let class = find(&chunks, "Handler");
        assert_eq!(
            strs(&class.defines),
            [
                "Handler",
                "handle",
                "Handler::handle",
                "render",
                "Handler::render"
            ]
        );
        assert_eq!(
            strs(&class.calls),
            [
                "db::get_user",
                "::get_user",
                "render",
                "billing::charge",
                "::charge",
                "Report"
            ]
        );
    }

    #[test]
    fn typescript_imports_classes_and_new() {
        let src = "\
import { limits as planLimits } from './billing/plans';
import * as api from '../api/index';
import Store from './store';

export class Billing {
  charge(plan: Plan): void {
    const l = planLimits(plan);
    api.post('/charge', l);
    Store.save(l);
    this.log(l);
    new Invoice(l);
  }
}

export const total = (xs: number[]) => sum(xs);
";
        let chunks = chunk_file("web/billing.ts", src);
        let class = find(&chunks, "Billing");
        assert_eq!(
            strs(&class.defines),
            ["Billing", "charge", "Billing::charge"]
        );
        assert_eq!(
            strs(&class.calls),
            [
                "plans::limits",
                "::limits",
                "api::post",
                "::post",
                "Store::save",
                "::save",
                "log",
                "Invoice"
            ]
        );
        let total = find(&chunks, "total");
        assert_eq!(strs(&total.defines), ["total"]);
        assert_eq!(strs(&total.calls), ["sum"]);
    }

    #[test]
    fn local_variables_in_functions_are_not_definitions() {
        let src =
            "function outer() {\n  const x = 1;\n  function inner() {}\n  return inner();\n}\n";
        let chunks = chunk_file("a.js", src);
        assert_eq!(strs(&chunks[0].defines), ["outer", "inner"]);
        assert_eq!(strs(&chunks[0].calls), ["inner"]);
    }

    #[test]
    fn top_level_calls_land_on_line_chunks_and_sql_defines_its_objects() {
        let chunks = chunk_file("a.py", "setup()\nrun(main)\n");
        assert_eq!(strs(&chunks[0].calls), ["setup", "run"]);
        let sql = chunk_file("m.sql", "CREATE TABLE plans (id int);\n");
        assert_eq!(strs(&sql[0].defines), ["plans"]);
        assert!(
            chunk_file("a.md", "# fn main()\n")
                .iter()
                .all(|c| c.defines.is_empty())
        );
    }

    #[test]
    fn unparseable_names_are_dropped() {
        let long = "x".repeat(200);
        let src = format!("fn f() {{ {long}(); (get)(); }}\n");
        let chunks = chunk_file("a.rs", &src);
        assert!(chunks[0].calls.is_empty(), "{:?}", chunks[0].calls);
    }

    #[test]
    fn generic_aliases_resolve_but_value_methods_do_not_borrow_imports() {
        let chunks = chunk_file(
            "a.rs",
            "use crate::db::load as fetch;\nfn run() { fetch::<u8>(); object.fetch(); }",
        );
        assert_eq!(
            strs(&chunks.last().unwrap().calls),
            ["db::load", "::load", "fetch"]
        );
    }

    #[test]
    fn calls_are_capped_and_deep_imports_do_not_recurse_without_bound() {
        let body = (0..400).map(|i| format!("call_{i}();")).collect::<String>();
        let chunks = chunk_file("a.rs", &format!("fn run() {{ {body} }}"));
        assert_eq!(chunks[0].calls.len(), super::MAX_NAMES_PER_CHUNK);
        let imports = format!(
            "use {}entry{};\nfn run() {{ entry(); }}",
            "m::{".repeat(100),
            "}".repeat(100)
        );
        let chunks = chunk_file("a.rs", &imports);
        assert!(!chunks.is_empty());
    }
}
