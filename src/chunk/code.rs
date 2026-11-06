//! Structure-aware chunking for Rust, Go, TypeScript, TSX, JavaScript and Python: one chunk per
//! top-level item, carrying its symbol names, with oversized items split further.

use tree_sitter::{Node, Parser};

use super::lines::{self, windows};
use super::{Chunk, ChunkKind, Language, calls};

/// An item of at most this many lines is one chunk.
const MAX_ITEM_LINES: usize = 150;
/// A container over the limit keeps its first lines as a header chunk.
const HEADER_LINES: usize = 20;
const WINDOW_LINES: usize = 120;
const WINDOW_OVERLAP: usize = 20;
/// Top-level code outside any item is grouped into chunks of at most this many lines.
const GAP_LINES: usize = 60;

/// 0-based inclusive line range.
#[derive(Clone, Copy)]
struct Span {
    start: usize,
    end: usize,
}

impl Span {
    fn len(self) -> usize {
        self.end - self.start + 1
    }
}

pub(super) struct Source<'a> {
    pub(super) text: &'a str,
    pub(super) lines: &'a [&'a str],
}

pub fn chunk(lang: Language, content: &str) -> Vec<Chunk> {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let Some(tree) = parse(lang, content) else {
        return lines::chunk(content);
    };
    let src = Source {
        text: content,
        lines: &lines,
    };
    let root = tree.root_node();
    let mut cursor = root.walk();
    let top: Vec<Node> = root.children(&mut cursor).collect();

    // Item spans include the comments and attributes directly above them.
    let mut items: Vec<(Node, Span)> = Vec::new();
    let mut covered = vec![false; lines.len()];
    for (i, node) in top.iter().enumerate() {
        let Some(inner) = item_node(lang, *node) else {
            continue;
        };
        let Some(span) = item_span(&src, &top, i) else {
            continue;
        };
        mark(&mut covered, span);
        items.push((inner, span));
    }

    let covered_lines = covered.iter().filter(|c| **c).count();
    if root.has_error() && covered_lines * 2 < lines.len() {
        return lines::chunk(content);
    }

    let mut out = Vec::new();
    for (inner, span) in &items {
        out.extend(item_chunks(&src, *inner, *span));
    }
    out.extend(gap_chunks(&src, &covered, 0, lines.len() - 1, &[], false));
    // Items and gaps were collected separately; put them back in file order.
    out.sort_by_key(|c| c.start_line);
    calls::annotate(lang, &src, root, &mut out);
    out
}

fn parse(lang: Language, content: &str) -> Option<tree_sitter::Tree> {
    let language = match lang {
        Language::Rust => tree_sitter::Language::new(tree_sitter_rust::LANGUAGE),
        Language::Go => tree_sitter::Language::new(tree_sitter_go::LANGUAGE),
        Language::TypeScript => {
            tree_sitter::Language::new(tree_sitter_typescript::LANGUAGE_TYPESCRIPT)
        }
        Language::Tsx => tree_sitter::Language::new(tree_sitter_typescript::LANGUAGE_TSX),
        Language::JavaScript => tree_sitter::Language::new(tree_sitter_javascript::LANGUAGE),
        Language::Python => tree_sitter::Language::new(tree_sitter_python::LANGUAGE),
        Language::Sql | Language::Markdown | Language::Other => return None,
    };
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    parser.parse(content, None)
}

/// The node an item's symbols come from, if `node` is an item: the node itself, or the
/// declaration inside an `export` statement or a Python decorated definition.
fn item_node<'t>(lang: Language, node: Node<'t>) -> Option<Node<'t>> {
    let inner = unwrap_wrapper(node)?;
    is_item_kind(lang, inner.kind()).then_some(inner)
}

fn unwrap_wrapper(node: Node<'_>) -> Option<Node<'_>> {
    match node.kind() {
        "export_statement" => node.child_by_field_name("declaration"),
        "decorated_definition" => node.child_by_field_name("definition"),
        _ => Some(node),
    }
}

pub(super) fn is_item_kind(lang: Language, kind: &str) -> bool {
    match lang {
        Language::Rust => matches!(
            kind,
            "function_item"
                | "struct_item"
                | "enum_item"
                | "union_item"
                | "impl_item"
                | "trait_item"
                | "mod_item"
                | "const_item"
                | "static_item"
                | "type_item"
                | "macro_definition"
        ),
        Language::Go => matches!(
            kind,
            "function_declaration"
                | "method_declaration"
                | "type_declaration"
                | "const_declaration"
                | "var_declaration"
        ),
        Language::TypeScript | Language::Tsx | Language::JavaScript => matches!(
            kind,
            "function_declaration"
                | "class_declaration"
                | "interface_declaration"
                | "type_alias_declaration"
                | "enum_declaration"
                | "lexical_declaration"
        ),
        Language::Python => matches!(kind, "function_definition" | "class_definition"),
        Language::Sql | Language::Markdown | Language::Other => false,
    }
}

/// Items whose members are chunked separately when the item is too long.
pub(super) fn is_container_kind(kind: &str) -> bool {
    matches!(
        kind,
        "impl_item"
            | "trait_item"
            | "mod_item"
            | "class_declaration"
            | "interface_declaration"
            | "class_definition"
    )
}

/// Comments, Rust attributes and TS/JS decorators: they travel with the item below them.
fn is_leading_trivia(kind: &str) -> bool {
    matches!(
        kind,
        "line_comment" | "block_comment" | "comment" | "attribute_item" | "decorator"
    )
}

/// The last line of a node. A node that ends just after a newline does not own the next line.
fn end_row(node: Node<'_>) -> usize {
    let (start, end) = (node.start_position(), node.end_position());
    if end.column == 0 && end.row > start.row {
        end.row - 1
    } else {
        end.row
    }
}

/// The span of `siblings[i]`, extended upward over the comments, attributes and decorators that
/// sit directly above it with no blank line between. Clamped to the file, so it cannot index out
/// of range whatever the tree says.
fn item_span(src: &Source, siblings: &[Node], i: usize) -> Option<Span> {
    let node = siblings.get(i)?;
    let last = src.lines.len().checked_sub(1)?;
    let mut start = node.start_position().row.min(last);
    let end = end_row(*node).min(last).max(start);
    let mut j = i;
    while j > 0 {
        let Some(prev) = siblings.get(j - 1) else {
            break;
        };
        if !is_leading_trivia(prev.kind()) || end_row(*prev) + 1 < start {
            break;
        }
        // A comment that trails the previous item's last line belongs to that item.
        let trails = j >= 2
            && siblings
                .get(j - 2)
                .is_some_and(|before| end_row(*before) == prev.start_position().row);
        if trails {
            break;
        }
        start = prev.start_position().row.min(start);
        j -= 1;
    }
    Some(Span { start, end })
}

fn mark(covered: &mut [bool], span: Span) {
    for c in covered.iter_mut().skip(span.start).take(span.len()) {
        *c = true;
    }
}

pub(super) fn node_text<'a>(src: &'a Source, node: Node<'_>) -> &'a str {
    src.text.get(node.byte_range()).unwrap_or("")
}

fn field_text(src: &Source, node: Node<'_>, field: &str) -> Option<String> {
    let text = node_text(src, node.child_by_field_name(field)?).trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// The names an item is known by.
pub(super) fn symbols_of(src: &Source, node: Node<'_>) -> Vec<String> {
    match node.kind() {
        // `impl Trait for Type` is known as `Type`.
        "impl_item" => field_text(src, node, "type").into_iter().collect(),
        "lexical_declaration" => {
            let mut cursor = node.walk();
            let declarator = node
                .named_children(&mut cursor)
                .find(|c| c.kind() == "variable_declarator");
            // A destructuring pattern has no single name.
            declarator
                .and_then(|d| d.child_by_field_name("name"))
                .filter(|n| n.kind() == "identifier")
                .map(|n| node_text(src, n).to_string())
                .into_iter()
                .collect()
        }
        "type_declaration" | "const_declaration" | "var_declaration" => {
            let mut names = Vec::new();
            go_spec_names(src, node, &mut names);
            names
        }
        _ => field_text(src, node, "name").into_iter().collect(),
    }
}

/// Names declared by the specs of a Go `type`, `const` or `var` declaration (grouped
/// declarations have several; `var` specs may sit inside a `var_spec_list`).
fn go_spec_names(src: &Source, node: Node<'_>, out: &mut Vec<String>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "type_spec" | "type_alias" | "const_spec" | "var_spec" => {
                let mut inner = child.walk();
                for name in child.children_by_field_name("name", &mut inner) {
                    out.push(node_text(src, name).to_string());
                }
            }
            "var_spec_list" => go_spec_names(src, child, out),
            _ => {}
        }
    }
}

fn item_chunks(src: &Source, inner: Node<'_>, span: Span) -> Vec<Chunk> {
    let symbols = symbols_of(src, inner);
    let enumerates = is_enumeration(src, inner);
    let chunks = if span.len() > MAX_ITEM_LINES
        && is_container_kind(inner.kind())
        && let Some(chunks) = container_chunks(src, inner, span, &symbols)
    {
        chunks
    } else {
        plain_item_chunks(src, span, symbols)
    };
    chunks
        .into_iter()
        .map(|c| c.enumerating(enumerates))
        .collect()
}

/// Whether an item defines a closed list of values: an enum, a Python `Enum` class, a Go `const`
/// block of two or more, a constant array or slice of two or more elements, or a TypeScript union
/// of two or more literal types.
fn is_enumeration(src: &Source, node: Node<'_>) -> bool {
    match node.kind() {
        "enum_item" | "enum_declaration" => true,
        "const_declaration" => named_children_of_kind(node, "const_spec") >= 2,
        "const_item" | "static_item" => node.child_by_field_name("value").is_some_and(is_list),
        // Go: `var Scopes = []Scope{Read, Write}`.
        "var_declaration" => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .filter(|spec| spec.kind() == "var_spec")
                .filter_map(|spec| spec.child_by_field_name("value"))
                .any(|values| first_named_child(values).is_some_and(is_list))
        }
        // TS/JS: `const KINDS = ['a', 'b'] as const`.
        "lexical_declaration" => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .filter(|d| d.kind() == "variable_declarator")
                .filter_map(|d| d.child_by_field_name("value"))
                .any(is_list)
        }
        // TS: `type Status = 'open' | 'closed'`.
        "type_alias_declaration" => node
            .child_by_field_name("value")
            .is_some_and(|v| v.kind() == "union_type" && count_descendants(v, "literal_type") >= 2),
        "class_definition" => node
            .child_by_field_name("superclasses")
            .is_some_and(|bases| {
                node_text(src, bases)
                    .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .any(|base| matches!(base, "Enum" | "IntEnum" | "StrEnum" | "Flag" | "IntFlag"))
            }),
        _ => false,
    }
}

/// A literal list of two or more elements, under any `&`, `as const` or parentheses: a Rust
/// array or slice, a Go composite literal, a JS/TS array. A Rust `[x; n]` repeat is not a list.
fn is_list(node: Node<'_>) -> bool {
    match node.kind() {
        "reference_expression" => node.child_by_field_name("value").is_some_and(is_list),
        "as_expression" | "satisfies_expression" | "parenthesized_expression" => {
            first_named_child(node).is_some_and(is_list)
        }
        "array_expression" => {
            node.child_by_field_name("length").is_none() && count_elements(node) >= 2
        }
        "array" => count_elements(node) >= 2,
        "composite_literal" => node
            .child_by_field_name("body")
            .is_some_and(|body| count_elements(body) >= 2),
        _ => false,
    }
}

fn first_named_child(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|c| !c.kind().contains("comment"))
}

/// Named children that are not comments.
fn count_elements(node: Node<'_>) -> usize {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|c| !c.kind().contains("comment"))
        .count()
}

fn named_children_of_kind(node: Node<'_>, kind: &str) -> usize {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|c| c.kind() == kind)
        .count()
}

fn count_descendants(node: Node<'_>, kind: &str) -> usize {
    let mut cursor = node.walk();
    let own = usize::from(node.kind() == kind);
    let children: Vec<Node> = node.named_children(&mut cursor).collect();
    own + children
        .into_iter()
        .map(|c| count_descendants(c, kind))
        .sum::<usize>()
}

/// One chunk for an item that fits, line windows for one that does not.
fn plain_item_chunks(src: &Source, span: Span, symbols: Vec<String>) -> Vec<Chunk> {
    let Some(item_lines) = src.lines.get(span.start..=span.end) else {
        return Vec::new();
    };
    if span.len() <= MAX_ITEM_LINES {
        return Chunk::from_lines(item_lines, span.start, symbols, ChunkKind::Item)
            .into_iter()
            .collect();
    }
    windows(
        item_lines,
        span.start,
        WINDOW_LINES,
        WINDOW_OVERLAP,
        &symbols,
        ChunkKind::Item,
    )
}

/// A header chunk (the first 20 lines, symbol = container name) plus one chunk per named direct
/// child of the container's body, with any other body code grouped into line chunks. None when
/// the item has no usable body or members, so the caller falls back to line windows.
fn container_chunks(
    src: &Source,
    inner: Node<'_>,
    span: Span,
    symbols: &[String],
) -> Option<Vec<Chunk>> {
    let container = symbols.first()?;
    let body = inner.child_by_field_name("body")?;
    let mut cursor = body.walk();
    let children: Vec<Node> = body.named_children(&mut cursor).collect();

    let mut covered = vec![false; src.lines.len()];
    let header = Span {
        start: span.start,
        end: span.end.min(span.start + HEADER_LINES - 1),
    };
    mark(&mut covered, header);

    let mut out = Vec::new();
    let mut members = 0;
    for (i, child) in children.iter().enumerate() {
        let Some(member) = unwrap_wrapper(*child) else {
            continue;
        };
        let Some(name) = symbols_of(src, member).into_iter().next() else {
            continue;
        };
        let Some(child_span) = item_span(src, &children, i) else {
            continue;
        };
        mark(&mut covered, child_span);
        let child_symbols = vec![name.clone(), format!("{container}::{name}")];
        out.extend(plain_item_chunks(src, child_span, child_symbols));
        members += 1;
    }
    if members == 0 {
        return None;
    }

    let header_lines = src.lines.get(header.start..=header.end)?;
    out.extend(Chunk::from_lines(
        header_lines,
        header.start,
        vec![container.clone()],
        ChunkKind::Item,
    ));
    // Class attributes and the like: body code that is neither the header nor a named member.
    out.extend(gap_chunks(
        src,
        &covered,
        span.start,
        span.end,
        std::slice::from_ref(container),
        true,
    ));
    out.sort_by_key(|c| c.start_line);
    Some(out)
}

/// `Lines` chunks (at most 60 lines each) over the runs of lines in `from..=to` that no item
/// covers. Blank edges of a run are trimmed and blank runs are skipped. With `skip_punctuation`,
/// runs with no letter or digit (a lone closing brace) are skipped too.
fn gap_chunks(
    src: &Source,
    covered: &[bool],
    from: usize,
    to: usize,
    symbols: &[String],
    skip_punctuation: bool,
) -> Vec<Chunk> {
    let mut out = Vec::new();
    let to = to.min(covered.len().saturating_sub(1));
    let mut i = from;
    while i <= to {
        if covered.get(i).copied().unwrap_or(true) {
            i += 1;
            continue;
        }
        let run_start = i;
        while i <= to && !covered.get(i).copied().unwrap_or(true) {
            i += 1;
        }
        let Some(run) = src.lines.get(run_start..i) else {
            continue;
        };
        let Some(first) = run.iter().position(|l| !l.trim().is_empty()) else {
            continue;
        };
        let last = run
            .iter()
            .rposition(|l| !l.trim().is_empty())
            .unwrap_or(first);
        let Some(text_lines) = run.get(first..=last) else {
            continue;
        };
        if skip_punctuation
            && !text_lines
                .iter()
                .any(|l| l.chars().any(char::is_alphanumeric))
        {
            continue;
        }
        out.extend(windows(
            text_lines,
            run_start + first,
            GAP_LINES,
            0,
            symbols,
            ChunkKind::Lines,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::chunk_file;

    fn syms(c: &Chunk) -> Vec<&str> {
        c.symbols.iter().map(String::as_str).collect()
    }

    fn range(c: &Chunk) -> (u32, u32) {
        (c.start_line, c.end_line)
    }

    fn items(chunks: &[Chunk]) -> Vec<&Chunk> {
        chunks
            .iter()
            .filter(|c| c.kind == ChunkKind::Item)
            .collect()
    }

    /// Every non-blank line of `content` is inside some chunk's line range.
    fn assert_covers_all_text(content: &str, chunks: &[Chunk]) {
        for (i, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let n = (i + 1) as u32;
            assert!(
                chunks.iter().any(|c| c.start_line <= n && n <= c.end_line),
                "line {n} ({line:?}) is in no chunk"
            );
        }
    }

    const RUST_SRC: &str = "\
use std::fmt;

/// A billing plan.
/// Plans cap seats.
#[derive(Debug)]
enum Plan {
    Free,
    Pro,
}

impl Plan {
    fn limits(&self) -> u32 {
        match self {
            Plan::Free => 1,
            Plan::Pro => 10,
        }
    }
}

fn main() {
    println!(\"{:?}\", Plan::Pro.limits());
}
";

    #[test]
    fn rust_enum_impl_and_main() {
        let chunks = chunk_file("src/plan.rs", RUST_SRC);
        let items = items(&chunks);
        assert_eq!(items.len(), 3);

        // Starts at the doc comment, not at the derive or the `enum` line.
        assert_eq!(syms(items[0]), vec!["Plan"]);
        assert_eq!(range(items[0]), (3, 9));
        assert!(items[0].text.starts_with("/// A billing plan."));
        assert!(items[0].text.contains("#[derive(Debug)]"));
        assert!(items[0].text.ends_with('}'));

        assert_eq!(syms(items[1]), vec!["Plan"]);
        assert_eq!(range(items[1]), (11, 18));
        assert!(items[1].text.contains("fn limits"));

        assert_eq!(syms(items[2]), vec!["main"]);
        assert_eq!(range(items[2]), (20, 22));

        // The `use` line is top-level code outside any item.
        let gaps: Vec<&Chunk> = chunks
            .iter()
            .filter(|c| c.kind == ChunkKind::Lines)
            .collect();
        assert_eq!(gaps.len(), 1);
        assert_eq!(range(gaps[0]), (1, 1));
        assert!(gaps[0].symbols.is_empty());
        // Chunks come back in file order.
        let starts: Vec<u32> = chunks.iter().map(|c| c.start_line).collect();
        assert_eq!(starts, vec![1, 3, 11, 20]);
    }

    #[test]
    fn enumerations_are_flagged() {
        let cases = [
            ("a.rs", "enum Plan { Free, Pro }\n", true),
            (
                "a.rs",
                "const KINDS: [&str; 2] = [\"code\", \"doc\"];\n",
                true,
            ),
            (
                "a.rs",
                "static KINDS: &[&str] = &[\"code\", \"doc\"];\n",
                true,
            ),
            ("a.rs", "const ZEROS: [u8; 32] = [0; 32];\n", false),
            ("a.rs", "const LIMIT: u32 = 100;\n", false),
            ("a.rs", "struct Plan { seats: u32 }\n", false),
            (
                "a.go",
                "package p\n\nconst (\n\tRead Scope = \"read\"\n\tWrite Scope = \"write\"\n)\n",
                true,
            ),
            ("a.go", "package p\n\nconst Limit = 10\n", false),
            (
                "a.go",
                "package p\n\nvar Scopes = []Scope{Read, Write}\n",
                true,
            ),
            ("a.go", "package p\n\nvar limit = 10\n", false),
            ("a.ts", "export enum Status { Open, Closed }\n", true),
            ("a.ts", "export type Status = 'open' | 'closed';\n", true),
            ("a.ts", "type Id = string | number;\n", false),
            (
                "a.ts",
                "export const KINDS = ['code', 'doc'] as const;\n",
                true,
            ),
            ("a.js", "const one = [1];\n", false),
            (
                "a.py",
                "class Status(str, Enum):\n    OPEN = 'open'\n    CLOSED = 'closed'\n",
                true,
            ),
            ("a.py", "class Plan(Base):\n    pass\n", false),
        ];
        for (path, src, want) in cases {
            let chunks = chunk_file(path, src);
            let item = chunks
                .iter()
                .find(|c| c.kind == ChunkKind::Item)
                .unwrap_or_else(|| panic!("{path} {src:?}: no item in {chunks:#?}"));
            assert_eq!(item.enumerates, want, "{path} {src:?}");
        }
    }

    #[test]
    fn comment_separated_by_a_blank_line_is_not_prepended() {
        let src = "// stray note\n\nfn f() {}\n";
        let chunks = chunk_file("a.rs", src);
        let items = items(&chunks);
        assert_eq!(range(items[0]), (3, 3));
        assert_eq!(items[0].text, "fn f() {}");
    }

    #[test]
    fn trailing_comment_of_previous_item_is_not_stolen() {
        let src = "fn a() {} // about a\nfn b() {}\n";
        let chunks = chunk_file("a.rs", src);
        let items = items(&chunks);
        assert_eq!(items.len(), 2);
        assert_eq!(range(items[1]), (2, 2));
    }

    #[test]
    fn walks_upward_through_mixed_comments_and_attributes() {
        let src = "/* block */\n#[inline]\n// line\n#[must_use]\nfn f() -> u8 { 1 }\n";
        let chunks = chunk_file("a.rs", src);
        let items = items(&chunks);
        assert_eq!(range(items[0]), (1, 5));
        assert!(items[0].text.starts_with("/* block */"));
    }

    fn big_impl() -> String {
        // 10 methods of 18 lines each plus the impl lines: well over 150 lines.
        let mut s = String::from("impl Plan {\n");
        for m in 0..10 {
            s.push_str(&format!("    fn method{m}(&self) -> u32 {{\n"));
            for k in 0..15 {
                s.push_str(&format!("        let v{k} = {k};\n"));
            }
            s.push_str("        0\n    }\n\n");
        }
        s.push_str("}\n");
        s
    }

    #[test]
    fn rust_big_impl_splits_into_header_and_methods() {
        let src = big_impl();
        assert!(src.lines().count() > 150);
        let chunks = chunk_file("plan.rs", &src);
        assert!(
            chunks.iter().all(|c| c.kind == ChunkKind::Item),
            "{chunks:#?}"
        );
        // Header first: the first 20 lines, symbol = container name.
        assert_eq!(syms(&chunks[0]), vec!["Plan"]);
        assert_eq!(range(&chunks[0]), (1, 20));
        // One chunk per method after the header (method0 and method1 also start inside it).
        let methods: Vec<&Chunk> = chunks.iter().skip(1).collect();
        assert_eq!(methods.len(), 10);
        for (m, c) in methods.iter().enumerate() {
            assert_eq!(
                syms(c),
                vec![format!("method{m}"), format!("Plan::method{m}")]
            );
            assert!(c.text.starts_with(&format!("    fn method{m}")));
            assert!(c.text.ends_with("    }"));
        }
        // Line math: method0 starts on line 2 and is 18 lines long.
        assert_eq!(range(methods[0]), (2, 19));
    }

    #[test]
    fn oversized_non_container_becomes_windows_with_the_item_symbols() {
        let mut src = String::from("fn huge() {\n");
        for i in 0..250 {
            src.push_str(&format!("    let x{i} = {i};\n"));
        }
        src.push_str("}\n");
        let chunks = chunk_file("a.rs", &src);
        let ranges: Vec<(u32, u32)> = chunks.iter().map(range).collect();
        assert_eq!(ranges, vec![(1, 120), (101, 220), (201, 252)]);
        assert!(chunks.iter().all(|c| syms(c) == vec!["huge"]));
    }

    #[test]
    fn item_of_exactly_150_lines_stays_whole_and_151_splits() {
        let make = |body_lines: usize| {
            let mut s = String::from("fn f() {\n");
            for i in 0..body_lines {
                s.push_str(&format!("    let x{i} = 0;\n"));
            }
            s.push_str("}\n");
            s
        };
        assert_eq!(chunk_file("a.rs", &make(148)).len(), 1);
        assert!(chunk_file("a.rs", &make(149)).len() > 1);
    }

    #[test]
    fn rust_trait_and_mod_containers() {
        let mut src = String::from("trait Store {\n    fn get(&self);\n");
        for i in 0..160 {
            src.push_str(&format!("    // filler {i}\n"));
        }
        src.push_str("    fn put(&self) {}\n}\n");
        let chunks = chunk_file("a.rs", &src);
        let names: Vec<Vec<&str>> = chunks.iter().map(syms).collect();
        assert!(names.contains(&vec!["Store"]));
        assert!(names.contains(&vec!["get", "Store::get"]));
        assert!(names.contains(&vec!["put", "Store::put"]));
    }

    #[test]
    fn rust_impl_for_trait_uses_the_type_name() {
        let src = "impl fmt::Display for Plan {\n    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result { Ok(()) }\n}\n";
        let chunks = chunk_file("a.rs", src);
        assert_eq!(syms(&chunks[0]), vec!["Plan"]);
    }

    #[test]
    fn go_func_and_type() {
        let src = "\
package plan

import \"fmt\"

// Plan is a billing plan.
type Plan struct {
\tName  string
\tSeats int
}

// Limits returns the seat cap.
func (p Plan) Limits() int {
\treturn p.Seats
}

func main() {
\tfmt.Println(Plan{}.Limits())
}

const MaxSeats = 10
";
        let chunks = chunk_file("plan.go", src);
        let items = items(&chunks);
        assert_eq!(items.len(), 4);
        assert_eq!(syms(items[0]), vec!["Plan"]);
        assert_eq!(range(items[0]), (5, 9));
        assert!(items[0].text.starts_with("// Plan is a billing plan."));
        assert_eq!(syms(items[1]), vec!["Limits"]);
        assert_eq!(range(items[1]), (11, 14));
        assert_eq!(syms(items[2]), vec!["main"]);
        assert_eq!(range(items[2]), (16, 18));
        assert_eq!(syms(items[3]), vec!["MaxSeats"]);
        assert_eq!(range(items[3]), (20, 20));
        // `package` and `import` are top-level code, grouped into one Lines chunk.
        assert_eq!(chunks[0].kind, ChunkKind::Lines);
        assert_eq!(range(&chunks[0]), (1, 3));
    }

    #[test]
    fn go_grouped_type_declaration_names_every_type() {
        let src = "package p\n\ntype (\n\tA int\n\tB string\n)\n";
        let chunks = chunk_file("p.go", src);
        let items = items(&chunks);
        assert_eq!(syms(items[0]), vec!["A", "B"]);
    }

    #[test]
    fn typescript_exported_function_and_class_with_methods() {
        let src = "\
import { Plan } from \"./plan\";

/** Seat cap. */
export function limits(plan: Plan): number {
  return plan.seats;
}

export class Billing {
  private seats = 1;

  charge(amount: number): void {
    this.seats += amount;
  }
}

const MAX = 10;

interface Options {
  verbose: boolean;
}

type Id = string;

enum Color {
  Red,
}
";
        let chunks = chunk_file("billing.ts", src);
        let items = items(&chunks);
        let found: Vec<(Vec<&str>, (u32, u32))> =
            items.iter().map(|c| (syms(c), range(c))).collect();
        assert_eq!(
            found,
            vec![
                (vec!["limits"], (3, 6)),
                (vec!["Billing"], (8, 14)),
                (vec!["MAX"], (16, 16)),
                (vec!["Options"], (18, 20)),
                (vec!["Id"], (22, 22)),
                (vec!["Color"], (24, 26)),
            ]
        );
        assert!(items[0].text.starts_with("/** Seat cap. */"));
        assert!(items[0].text.contains("export function limits"));
        // The import is top-level code.
        assert_eq!(chunks[0].kind, ChunkKind::Lines);
        assert_eq!(range(&chunks[0]), (1, 1));
    }

    #[test]
    fn typescript_big_class_splits_into_header_and_methods() {
        let mut src = String::from("export class Billing {\n");
        for m in 0..10 {
            src.push_str(&format!("  @Log()\n  method{m}(): number {{\n"));
            for k in 0..14 {
                src.push_str(&format!("    const v{k} = {k};\n"));
            }
            src.push_str("    return 0;\n  }\n\n");
        }
        src.push_str("}\n");
        let chunks = chunk_file("b.ts", &src);
        assert_eq!(syms(&chunks[0]), vec!["Billing"]);
        assert_eq!(range(&chunks[0]), (1, 20));
        let m0 = chunks
            .iter()
            .find(|c| syms(c) == vec!["method0", "Billing::method0"])
            .expect("method0 chunk");
        // The decorator above the method is part of its chunk.
        assert!(m0.text.starts_with("  @Log()"), "{:?}", m0.text);
        assert_eq!(range(m0), (2, 19));
        assert!(
            chunks
                .iter()
                .any(|c| syms(c) == vec!["method9", "Billing::method9"])
        );
    }

    #[test]
    fn tsx_component() {
        let src = "\
import React from \"react\";

export default function Greeting({ name }: { name: string }) {
  return <div className=\"greeting\">Hello, {name}</div>;
}

const Footer = () => <footer>bye</footer>;
";
        let chunks = chunk_file("Greeting.tsx", src);
        let items = items(&chunks);
        assert_eq!(items.len(), 2);
        assert_eq!(syms(items[0]), vec!["Greeting"]);
        assert_eq!(range(items[0]), (3, 5));
        assert!(items[0].text.contains("<div className"));
        assert_eq!(syms(items[1]), vec!["Footer"]);
        assert_eq!(range(items[1]), (7, 7));
    }

    #[test]
    fn javascript_function_and_class() {
        let src = "function add(a, b) {\n  return a + b;\n}\n\nclass Calc {\n  run() {}\n}\n";
        let chunks = chunk_file("calc.js", src);
        let found: Vec<Vec<&str>> = chunks.iter().map(syms).collect();
        assert_eq!(found, vec![vec!["add"], vec!["Calc"]]);
    }

    #[test]
    fn python_decorated_function_and_class() {
        let src = "\
import functools


# Cache results.
@functools.cache
def limits(plan):
    return 10


class Plan:
    \"\"\"A plan.\"\"\"

    def seats(self):
        return 1
";
        let chunks = chunk_file("plan.py", src);
        let items = items(&chunks);
        assert_eq!(items.len(), 2);
        assert_eq!(syms(items[0]), vec!["limits"]);
        assert_eq!(range(items[0]), (4, 7));
        assert!(
            items[0]
                .text
                .starts_with("# Cache results.\n@functools.cache")
        );
        assert_eq!(syms(items[1]), vec!["Plan"]);
        assert_eq!(range(items[1]), (10, 14));
        assert_eq!(chunks[0].kind, ChunkKind::Lines);
        assert_eq!(range(&chunks[0]), (1, 1));
    }

    #[test]
    fn python_big_class_keeps_methods_and_class_attributes() {
        let mut src = String::from("class Plan:\n    seats = 1\n\n");
        for m in 0..10 {
            src.push_str(&format!("    @property\n    def method{m}(self):\n"));
            for k in 0..14 {
                src.push_str(&format!("        v{k} = {k}\n"));
            }
            src.push_str("        return 0\n\n");
        }
        src.push_str("    tail = 2\n");
        let chunks = chunk_file("plan.py", &src);
        assert_eq!(syms(&chunks[0]), vec!["Plan"]);
        let m3 = chunks
            .iter()
            .find(|c| syms(c) == vec!["method3", "Plan::method3"])
            .expect("method3 chunk");
        assert!(m3.text.starts_with("    @property\n    def method3"));
        // `tail = 2` is past the header and is not a method: it is kept as a line chunk.
        let tail = chunks.last().expect("chunks");
        assert_eq!(tail.kind, ChunkKind::Lines);
        assert_eq!(tail.text, "    tail = 2");
        assert_eq!(syms(tail), vec!["Plan"]);
        // Nothing else in the class body is left over as a line chunk.
        assert_eq!(
            chunks.iter().filter(|c| c.kind == ChunkKind::Lines).count(),
            1
        );
    }

    #[test]
    fn syntax_error_file_still_yields_chunks_covering_its_content() {
        // Mostly garbage: the fallback to line windows keeps every line.
        let garbage = "fn ((( {{{ ]]] let = = ;\n".repeat(30) + "fn ok() {}\n";
        let chunks = chunk_file("bad.rs", &garbage);
        assert!(!chunks.is_empty());
        assert_covers_all_text(&garbage, &chunks);
        assert!(chunks.iter().all(|c| c.kind == ChunkKind::Lines));

        // A small error among good items: items survive and the broken part is still covered.
        let mostly_good = "\
fn good_one() -> u32 {
    1
}

fn broken( {
    let = ;

fn good_two() -> u32 {
    2
}
";
        let chunks = chunk_file("mixed.rs", mostly_good);
        assert!(!chunks.is_empty());
        assert_covers_all_text(mostly_good, &chunks);
        assert!(chunks.iter().any(|c| syms(c) == vec!["good_one"]));
    }

    #[test]
    fn syntax_error_in_every_language_still_covers_the_file() {
        let cases = [
            ("a.go", "func ((( {{{ )))\n"),
            ("a.ts", "class { { { ( ( (\n"),
            ("a.tsx", "<<< >>> ((( {{{\n"),
            ("a.js", "function ( { } ] [\n"),
            ("a.py", "def ((( :::\n    ]]] x = =\n"),
        ];
        for (path, content) in cases {
            let chunks = chunk_file(path, content);
            assert!(!chunks.is_empty(), "{path}");
            assert_covers_all_text(content, &chunks);
        }
    }

    #[test]
    fn crlf_and_no_trailing_newline() {
        let src = "// note\r\nfn a() {\r\n    1;\r\n}\r\nfn b() {}";
        let chunks = chunk_file("a.rs", src);
        let items = items(&chunks);
        assert_eq!(items.len(), 2);
        assert_eq!(range(items[0]), (1, 4));
        assert_eq!(items[0].text, "// note\nfn a() {\n    1;\n}");
        assert_eq!(range(items[1]), (5, 5));
        assert_eq!(items[1].text, "fn b() {}");
    }

    #[test]
    fn one_very_long_line_does_not_panic() {
        let src = format!("fn f() {{ let s = \"{}\"; }}\n", "é".repeat(50_000));
        let chunks = chunk_file("a.rs", &src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text.chars().count(), 8000);
        assert_eq!(syms(&chunks[0]), vec!["f"]);
    }

    #[test]
    fn code_without_items_is_line_chunks() {
        let chunks = chunk_file("a.py", "x = 1\ny = 2\n");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, ChunkKind::Lines);
        assert_eq!(range(&chunks[0]), (1, 2));
    }

    #[test]
    fn top_level_code_runs_are_capped_at_60_lines() {
        let src: String = (0..130).map(|i| format!("x{i} = {i}\n")).collect();
        let chunks = chunk_file("a.py", &src);
        let ranges: Vec<(u32, u32)> = chunks.iter().map(range).collect();
        assert_eq!(ranges, vec![(1, 60), (61, 120), (121, 130)]);
    }
}
