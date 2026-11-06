//! Splits file content into searchable chunks: code items via tree-sitter, SQL statements,
//! markdown sections, and plain line windows. Everything here is pure (no I/O, no database) and total: any input,
//! however malformed, yields chunks rather than a panic, so request handlers can call it directly.
//!
//! Repo files go through [`chunk_file`], which cuts what does not fit a chunk. The documents API
//! uses [`chunk_markdown_split`] instead, which splits rather than cuts so that all of the text
//! stays searchable, and stops as soon as a chunk limit is passed.

mod calls;
mod code;
mod lines;
mod markdown;
mod sql;

use std::path::Path;

pub use markdown::{chunk_markdown, chunk_markdown_split};

/// Part of the stored hash of every repo file. Bump it whenever [`chunk_file`] chunks some file
/// differently, so that `lore repo push --full` chunks files indexed by an older version again
/// instead of skipping them as unchanged.
pub const CHUNKER_VERSION: u32 = 3;

/// Chunks are cut to this many characters (not bytes) so one huge line cannot blow up a row.
/// [`chunk_markdown_split`] sizes its chunks to fit, so the cut never loses text there.
const MAX_CHUNK_CHARS: usize = 8000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkKind {
    /// A code item: function, type, impl, class, or a piece of one.
    Item,
    /// A markdown section.
    Section,
    /// A plain window of lines (top-level statements, unsupported languages).
    Lines,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub text: String,
    /// 1-based, inclusive.
    pub start_line: u32,
    /// 1-based, inclusive.
    pub end_line: u32,
    pub symbols: Vec<String>,
    pub kind: ChunkKind,
    /// The chunk defines a closed list of values: an enum, a block of constants, a constant array,
    /// seed rows, a `CHECK ... IN` list. Search ranks these on their own as well, because "which X
    /// are there" is answered by one of them and is easily outranked by code that only uses X.
    pub enumerates: bool,
    /// The functions, methods, types and SQL objects this chunk defines, as they are called: a
    /// method also as `Type::method`. Each definition is on one chunk only, the smallest one
    /// holding its first line.
    pub defines: Vec<String>,
    /// What the code in this chunk calls, by name. A call that names where its target lives, by a
    /// path (`store::index_text`, `Plan::new`) or through an import, is stored as
    /// `qualifier::name` and `::name`; any other call as the bare name. Nothing is resolved
    /// here: `callgraph` ties calls to definitions when asked.
    pub calls: Vec<String>,
}

impl Chunk {
    /// Every chunk is built here, so the 8,000 character cap holds for all of them.
    pub(crate) fn new(
        text: String,
        start_line: u32,
        end_line: u32,
        symbols: Vec<String>,
        kind: ChunkKind,
    ) -> Self {
        Chunk {
            text: truncate_chars(text, MAX_CHUNK_CHARS),
            start_line,
            end_line,
            symbols,
            kind,
            enumerates: false,
            defines: Vec::new(),
            calls: Vec::new(),
        }
    }

    pub(crate) fn enumerating(self, enumerates: bool) -> Self {
        Chunk { enumerates, ..self }
    }

    /// A chunk over `lines`, where `lines[0]` is the 0-based line `first` of the file. None for an
    /// empty slice.
    pub(crate) fn from_lines(
        lines: &[&str],
        first: usize,
        symbols: Vec<String>,
        kind: ChunkKind,
    ) -> Option<Self> {
        if lines.is_empty() {
            return None;
        }
        Some(Chunk::new(
            lines.join("\n"),
            line_number(first),
            line_number(first.saturating_add(lines.len() - 1)),
            symbols,
            kind,
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Rust,
    Go,
    TypeScript,
    Tsx,
    JavaScript,
    Python,
    Sql,
    Markdown,
    Other,
}

/// Maps a path to a language by extension (case-insensitive).
pub fn language_for(path: &str) -> Language {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("rs") => Language::Rust,
        Some("go") => Language::Go,
        Some("ts" | "mts" | "cts") => Language::TypeScript,
        Some("tsx") => Language::Tsx,
        Some("js" | "jsx" | "mjs" | "cjs") => Language::JavaScript,
        Some("py") => Language::Python,
        Some("sql") => Language::Sql,
        Some("md" | "mdx") => Language::Markdown,
        _ => Language::Other,
    }
}

/// Chunks one file, dispatching on its language. Line numbers are 1-based and inclusive, and
/// `\r\n` line endings are normalised to `\n` in chunk text.
pub fn chunk_file(path: &str, content: &str) -> Vec<Chunk> {
    match language_for(path) {
        Language::Markdown => markdown::chunk_markdown(content, file_stem(path)),
        // The statement names of SQL chunks are the tables, views and functions they create.
        Language::Sql => sql::chunk(content)
            .into_iter()
            .map(|c| Chunk {
                defines: c
                    .symbols
                    .iter()
                    .filter(|s| s.len() <= calls::MAX_NAME_BYTES)
                    .take(calls::MAX_NAMES_PER_CHUNK)
                    .cloned()
                    .collect(),
                ..c
            })
            .collect(),
        Language::Other => lines::chunk(content),
        lang => code::chunk(lang, content),
    }
}

/// The text a Postgres full-text index sees: the chunk, then every identifier in the text and the
/// symbols split into lowercase words, so `MaxSeatsPerPlan` also matches a search for "seats".
pub fn lexical_text(chunk: &Chunk) -> String {
    let mut words = split_words(&chunk.text);
    for symbol in &chunk.symbols {
        words.extend(split_words(symbol));
    }
    format!("{}\n{}", chunk.text, words.join(" "))
}

/// The text that gets embedded: the path and symbols give the model context the body lacks.
pub fn embed_text(path: &str, chunk: &Chunk) -> String {
    format!("{path}\n{}\n{}", chunk.symbols.join(" "), chunk.text)
}

/// 0-based line index to 1-based line number, saturating instead of overflowing.
pub(crate) fn line_number(index: usize) -> u32 {
    u32::try_from(index.saturating_add(1)).unwrap_or(u32::MAX)
}

fn file_stem(path: &str) -> &str {
    Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
}

/// Keeps the first `max` characters, cutting on a char boundary.
fn truncate_chars(mut s: String, max: usize) -> String {
    if let Some((idx, _)) = s.char_indices().nth(max) {
        s.truncate(idx);
    }
    s
}

/// Splits every alphanumeric run in `s` into lowercase words at case changes (digits stay with
/// the word before them). `MaxSeatsPerPlan` gives `max seats per plan`, `HTTPServer` gives
/// `http server`. Underscores, hyphens and all other punctuation are separators.
fn split_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    for token in s.split(|c: char| !c.is_alphanumeric()) {
        let chars: Vec<char> = token.chars().collect();
        let mut start = 0;
        for i in 1..chars.len() {
            let (prev, cur) = (chars[i - 1], chars[i]);
            let next = chars.get(i + 1).copied();
            // aB or 1B: a new word starts at an uppercase letter after a lowercase letter or digit.
            let lower_to_upper = cur.is_uppercase() && (prev.is_lowercase() || prev.is_numeric());
            // HTTPServer: the last capital of an acronym starts the next word.
            let acronym_end =
                cur.is_uppercase() && prev.is_uppercase() && next.is_some_and(|n| n.is_lowercase());
            if lower_to_upper || acronym_end {
                words.push(chars[start..i].iter().collect::<String>().to_lowercase());
                start = i;
            }
        }
        if start < chars.len() {
            words.push(chars[start..].iter().collect::<String>().to_lowercase());
        }
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk_with(text: &str, symbols: &[&str]) -> Chunk {
        Chunk::new(
            text.to_string(),
            1,
            1,
            symbols.iter().map(|s| s.to_string()).collect(),
            ChunkKind::Item,
        )
    }

    /// The word list `lexical_text` appends after the chunk text.
    fn words_of(text: &str) -> String {
        let lexical = lexical_text(&chunk_with(text, &[]));
        lexical.split_once('\n').map(|x| x.1).unwrap().to_string()
    }

    #[test]
    fn language_for_maps_extensions() {
        let cases = [
            ("src/lib.rs", Language::Rust),
            ("main.go", Language::Go),
            ("a.ts", Language::TypeScript),
            ("a.mts", Language::TypeScript),
            ("a.cts", Language::TypeScript),
            ("ui/App.tsx", Language::Tsx),
            ("a.js", Language::JavaScript),
            ("a.jsx", Language::JavaScript),
            ("a.mjs", Language::JavaScript),
            ("a.cjs", Language::JavaScript),
            ("tool.py", Language::Python),
            ("migrations/0001_init.sql", Language::Sql),
            ("SEED.SQL", Language::Sql),
            ("README.md", Language::Markdown),
            ("page.mdx", Language::Markdown),
            ("UPPER.RS", Language::Rust),
            ("Main.java", Language::Other),
            ("Makefile", Language::Other),
            (".rs", Language::Other),
            ("dir.rs/file", Language::Other),
            ("", Language::Other),
        ];
        for (path, want) in cases {
            assert_eq!(language_for(path), want, "path {path:?}");
        }
    }

    #[test]
    fn identifier_words_are_split_and_lowercased() {
        assert_eq!(words_of("MaxSeatsPerPlan"), "max seats per plan");
        assert_eq!(words_of("max_seats"), "max seats");
        assert_eq!(words_of("max-seats"), "max seats");
        assert_eq!(words_of("HTTPServer"), "http server");
        assert_eq!(
            words_of("parseHTTPResponse2Json"),
            "parse http response2 json"
        );
    }

    #[test]
    fn lexical_text_is_text_newline_words_with_symbols() {
        let chunk = chunk_with("fn max_seats()", &["Plan::maxSeats"]);
        assert_eq!(
            lexical_text(&chunk),
            "fn max_seats()\nfn max seats plan max seats"
        );
    }

    #[test]
    fn embed_text_is_path_symbols_text() {
        let chunk = chunk_with("body", &["Plan", "Plan::limits"]);
        assert_eq!(
            embed_text("src/plan.rs", &chunk),
            "src/plan.rs\nPlan Plan::limits\nbody"
        );
    }

    #[test]
    fn text_over_8000_chars_is_cut_on_a_char_boundary() {
        // Two-byte characters: a byte-based cut at 8000 would be fine here, but 3 and 4 byte
        // characters mixed in would not be, so use all three widths.
        let content = "é€𝄞".repeat(4000);
        assert!(content.chars().count() > MAX_CHUNK_CHARS);
        for path in ["a.txt", "a.md", "a.rs", "a.sql"] {
            let chunks = chunk_file(path, &content);
            assert!(!chunks.is_empty(), "{path}");
            for c in &chunks {
                assert_eq!(c.text.chars().count(), MAX_CHUNK_CHARS, "{path}");
                assert!(c.text.chars().all(|ch| "é€𝄞".contains(ch)), "{path}");
            }
        }
        // The first 8000 characters are kept, not some other slice.
        let chunks = chunk_file("a.txt", &content);
        assert!(content.starts_with(&chunks[0].text));
    }

    #[test]
    fn short_text_is_not_truncated() {
        assert_eq!(truncate_chars("héllo".into(), 5), "héllo");
        assert_eq!(truncate_chars("héllo".into(), 2), "hé");
        assert_eq!(truncate_chars(String::new(), 0), "");
    }

    #[test]
    fn empty_and_blank_files_give_no_chunks() {
        for path in [
            "a.rs", "a.go", "a.ts", "a.tsx", "a.js", "a.py", "a.sql", "a.md", "a.txt",
        ] {
            assert!(chunk_file(path, "").is_empty(), "{path} empty");
            assert!(chunk_file(path, "\n\n  \n").is_empty(), "{path} blank");
        }
    }

    #[test]
    fn unknown_extension_gets_line_windows() {
        let content: String = (1..=100).map(|i| format!("line {i}\n")).collect();
        let chunks = chunk_file("notes.txt", &content);
        assert!(chunks.iter().all(|c| c.kind == ChunkKind::Lines));
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks.last().map(|c| c.end_line), Some(100));
    }

    #[test]
    fn markdown_default_symbol_is_the_file_stem() {
        let chunks = chunk_file("docs/guide.md", "intro text\n\n# Heading\nbody\n");
        assert_eq!(chunks[0].symbols, vec!["guide".to_string()]);
        assert_eq!(chunks[1].symbols, vec!["Heading".to_string()]);
    }
}
