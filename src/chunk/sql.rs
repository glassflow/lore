//! SQL: one chunk per statement, carrying the names of the tables, indexes and other objects it
//! creates or changes. Statements about the same object (a table and its indexes) share a chunk,
//! and an `INSERT ... VALUES` also carries the key of each row, so the rows a seed migration adds
//! can be found, and counted, from the chunk's symbols. Statements are found by a splitter that
//! knows quotes and comments rather than by a grammar, so Postgres, ClickHouse, MySQL and SQLite
//! files all chunk the same way.

use super::lines::{self, windows};
use super::{Chunk, ChunkKind};

/// A statement of at most this many lines is one chunk; a longer one becomes line windows.
const MAX_ITEM_LINES: usize = 150;
const WINDOW_LINES: usize = 120;
const WINDOW_OVERLAP: usize = 20;
/// Several statements share a chunk only while it stays within this many lines.
const GROUP_LINES: usize = 60;
/// The most row keys one chunk carries in its symbols.
const MAX_ROW_KEYS: usize = 20;
/// A string literal longer than this is a value, not a row key.
const MAX_KEY_CHARS: usize = 64;

/// 0-based inclusive line range.
#[derive(Clone, Copy)]
struct Span {
    start: usize,
    end: usize,
}

pub fn chunk(content: &str) -> Vec<Chunk> {
    let lines: Vec<&str> = content.lines().collect();
    let Some(last_line) = lines.len().checked_sub(1) else {
        return Vec::new();
    };
    let statements = split(content);
    if statements.is_empty() {
        return lines::chunk(content);
    }
    let blank = |l: usize| lines.get(l).is_none_or(|text| text.trim().is_empty());

    let mut groups: Vec<Group> = Vec::new();
    let mut prev_end: Option<usize> = None;
    for s in &statements {
        let first = s.first.min(last_line);
        let end = s.last.min(last_line).max(first);
        // The comments since the previous statement travel with this one.
        let from = prev_end.map_or(0, |p| p + 1).min(first);
        let start = (from..first).find(|&l| !blank(l)).unwrap_or(first);
        prev_end = Some(end);
        let span = Span { start, end };
        let about = about(&s.code);
        match groups.last_mut() {
            Some(g) if g.takes(&about, span) => g.add(about, span),
            _ => groups.push(Group::new(about, span)),
        }
    }
    // Comments after the last statement belong to it.
    if let Some(g) = groups.last_mut()
        && let Some(end) = (g.span.end..=last_line).rev().find(|&l| !blank(l))
    {
        g.span.end = end;
    }
    groups.iter().flat_map(|g| g.chunks(&lines)).collect()
}

/// Statements that share a chunk.
struct Group {
    span: Span,
    /// The object the group is about: its first statement's that names one.
    subject: Option<String>,
    names: Vec<String>,
    keys: Vec<String>,
    /// Some statement of the group lists values: seed rows, an enum type, a `CHECK ... IN`.
    enumerates: bool,
}

impl Group {
    fn new(about: About, span: Span) -> Self {
        let mut g = Group {
            span,
            subject: None,
            names: Vec::new(),
            keys: Vec::new(),
            enumerates: false,
        };
        g.add(about, span);
        g
    }

    /// Whether the statement `about`, over `span`, joins this group: it starts on the group's
    /// last line, or it is about the same object (or one of the two is about none) and the group
    /// stays within [`GROUP_LINES`].
    fn takes(&self, about: &About, span: Span) -> bool {
        if span.start <= self.span.end {
            return true;
        }
        let related = match (&self.subject, &about.subject) {
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
            _ => true,
        };
        related && span.end.saturating_sub(self.span.start) < GROUP_LINES
    }

    fn add(&mut self, about: About, span: Span) {
        self.span.end = self.span.end.max(span.end);
        self.enumerates |= about.enumerates;
        if self.subject.is_none() {
            self.subject = about.subject;
        }
        for name in about.names {
            if !self.names.contains(&name) {
                self.names.push(name);
            }
        }
        for key in about.keys {
            if self.keys.len() < MAX_ROW_KEYS && !self.keys.contains(&key) {
                self.keys.push(key);
            }
        }
    }

    /// One chunk for a group that fits, line windows for one that does not. A window carries the
    /// object names but not the row keys, which name rows that are mostly in other windows.
    fn chunks(&self, lines: &[&str]) -> Vec<Chunk> {
        let Some(group_lines) = lines.get(self.span.start..=self.span.end) else {
            return Vec::new();
        };
        let kind = if self.names.is_empty() {
            ChunkKind::Lines
        } else {
            ChunkKind::Item
        };
        if group_lines.len() <= MAX_ITEM_LINES {
            let symbols = self.names.iter().chain(&self.keys).cloned().collect();
            return Chunk::from_lines(group_lines, self.span.start, symbols, kind)
                .map(|c| c.enumerating(self.enumerates))
                .into_iter()
                .collect();
        }
        windows(
            group_lines,
            self.span.start,
            WINDOW_LINES,
            WINDOW_OVERLAP,
            &self.names,
            kind,
        )
        .into_iter()
        .map(|c| c.enumerating(self.enumerates))
        .collect()
    }
}

/// One statement of the file.
struct Statement {
    /// 0-based line of its first character outside a comment.
    first: usize,
    /// 0-based line of its last character outside a comment, before the closing `;`.
    last: usize,
    /// Its text, with each comment replaced by a space.
    code: String,
}

/// Splits `content` at each `;` outside a string, a quoted identifier and a comment. Text after
/// the last `;` is a statement too, and empty statements are dropped.
fn split(content: &str) -> Vec<Statement> {
    let chars: Vec<char> = content.chars().collect();
    let mut out = Vec::new();
    let mut current: Option<Statement> = None;
    let mut line = 0;
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        let next = chars.get(i + 1).copied();
        let comment_end = match (c, next) {
            ('-', Some('-')) => Some(line_end(&chars, i)),
            ('/', Some('*')) => Some(block_comment_end(&chars, i)),
            _ => None,
        };
        if let Some(end) = comment_end {
            line += newlines(chars.get(i..end).unwrap_or_default());
            if let Some(s) = current.as_mut() {
                s.code.push(' ');
            }
            i = end;
            continue;
        }
        if c.is_whitespace() {
            if c == '\n' {
                line += 1;
            }
            if let Some(s) = current.as_mut() {
                s.code.push(c);
            }
            i += 1;
            continue;
        }
        if c == ';' {
            out.extend(current.take());
            i += 1;
            continue;
        }
        let end = match c {
            '\'' => quoted_end(&chars, i, '\'', true),
            '"' | '`' => quoted_end(&chars, i, c, false),
            '$' => dollar_quoted_end(&chars, i).unwrap_or(i + 1),
            _ => i + 1,
        };
        let piece = chars.get(i..end).unwrap_or_default();
        let s = current.get_or_insert_with(|| Statement {
            first: line,
            last: line,
            code: String::new(),
        });
        line += newlines(piece);
        s.code.extend(piece);
        s.last = line;
        i = end;
    }
    out.extend(current);
    out
}

fn newlines(chars: &[char]) -> usize {
    chars.iter().filter(|&&c| c == '\n').count()
}

/// The index of the newline that ends the line comment starting at `i`, or the end of input.
fn line_end(chars: &[char], i: usize) -> usize {
    chars
        .get(i..)
        .and_then(|rest| rest.iter().position(|&c| c == '\n'))
        .map_or(chars.len(), |p| i + p)
}

/// Just past the `*/` that closes the block comment starting at `i`, counting nested `/*` as
/// Postgres does, or the end of input.
fn block_comment_end(chars: &[char], i: usize) -> usize {
    let mut depth = 0;
    let mut j = i;
    while let Some(&c) = chars.get(j) {
        let next = chars.get(j + 1).copied();
        if c == '/' && next == Some('*') {
            depth += 1;
            j += 2;
        } else if c == '*' && next == Some('/') {
            depth -= 1;
            j += 2;
            if depth == 0 {
                return j;
            }
        } else {
            j += 1;
        }
    }
    chars.len()
}

/// Just past the `quote` that closes the quoted text starting at `i`, or the end of input. A
/// doubled quote stands for itself, and with `backslash` so does a quote after a backslash.
fn quoted_end(chars: &[char], i: usize, quote: char, backslash: bool) -> usize {
    let mut j = i + 1;
    while let Some(&c) = chars.get(j) {
        if backslash && c == '\\' {
            j += 2;
        } else if c == quote {
            if chars.get(j + 1) == Some(&quote) {
                j += 2;
            } else {
                return j + 1;
            }
        } else {
            j += 1;
        }
    }
    chars.len()
}

/// Just past the end of the dollar-quoted string (`$$...$$`, `$body$...$body$`) starting at `i`,
/// or the end of input. None when the `$` at `i` does not open one, as in a `$1` parameter.
fn dollar_quoted_end(chars: &[char], i: usize) -> Option<usize> {
    let rest = chars.get(i + 1..)?;
    let tag_len = rest
        .iter()
        .position(|&c| !(c.is_alphanumeric() || c == '_'))?;
    if rest.get(tag_len) != Some(&'$') || rest.first().is_some_and(char::is_ascii_digit) {
        return None;
    }
    let delimiter = chars.get(i..i + tag_len + 2)?;
    let body = i + delimiter.len();
    let close = chars
        .get(body..)?
        .windows(delimiter.len())
        .position(|w| w == delimiter);
    Some(close.map_or(chars.len(), |p| body + p + delimiter.len()))
}

/// What a statement is about.
#[derive(Debug, Default, PartialEq)]
struct About {
    /// The object later statements are grouped by: a table, or what was created or changed.
    subject: Option<String>,
    /// Every object the statement names, the subject included.
    names: Vec<String>,
    /// The first string literal of each `VALUES` row of an `INSERT`.
    keys: Vec<String>,
    /// The statement lists values: two or more `VALUES` rows, an enum type (`AS ENUM`, ClickHouse
    /// `Enum8(...)`), or a `CHECK (... IN (...))`.
    enumerates: bool,
}

impl About {
    fn of(names: Vec<String>, subject: Option<String>) -> Self {
        let subject = subject.or_else(|| names.first().cloned());
        About {
            subject,
            names,
            keys: Vec::new(),
            enumerates: false,
        }
    }
}

/// Modifiers between `CREATE`, `ALTER` or `DROP` and the kind of object.
const MODIFIERS: [&str; 12] = [
    "OR",
    "REPLACE",
    "TEMP",
    "TEMPORARY",
    "UNLOGGED",
    "UNIQUE",
    "GLOBAL",
    "LOCAL",
    "MATERIALIZED",
    "RECURSIVE",
    "CONSTRAINT",
    "FOREIGN",
];

/// Objects that are created on a table, `CREATE <kind> name ... ON table`, and so are grouped
/// with it.
const ON_TABLE: [&str; 4] = ["INDEX", "TRIGGER", "POLICY", "RULE"];

/// Reads the objects a statement names from its first few words. A statement this does not
/// recognise (`BEGIN`, `SET`, `DO`, `SELECT`, `GRANT`) is about nothing.
fn about(code: &str) -> About {
    let tokens = tokenize(code);
    let mut p = Cursor {
        tokens: &tokens,
        i: 0,
    };
    let Some(verb) = p.word() else {
        return About::default();
    };
    let mut about = match verb.to_ascii_uppercase().as_str() {
        "CREATE" => create(&mut p),
        "ALTER" => {
            p.kind();
            p.eat(&["IF", "EXISTS"]);
            p.eat(&["ONLY"]);
            About::of(p.name().into_iter().collect(), None)
        }
        "DROP" => drop(&mut p),
        "INSERT" => insert(&mut p),
        "UPDATE" => {
            p.eat(&["ONLY"]);
            About::of(p.name().into_iter().collect(), None)
        }
        "DELETE" => {
            p.eat(&["FROM"]);
            p.eat(&["ONLY"]);
            About::of(p.name().into_iter().collect(), None)
        }
        "TRUNCATE" => {
            p.eat(&["TABLE"]);
            p.eat(&["ONLY"]);
            About::of(p.name().into_iter().collect(), None)
        }
        "COMMENT" => {
            p.eat(&["ON"]);
            let kind = p.kind();
            let Some(name) = p.name() else {
                return About::default();
            };
            // `COMMENT ON COLUMN t.c` is about the table `t`.
            let subject = if kind.eq_ignore_ascii_case("COLUMN") {
                name.rsplit_once('.').map(|(table, _)| table.to_string())
            } else {
                None
            };
            About::of(vec![name], subject)
        }
        _ => About::default(),
    };
    about.enumerates |= lists_values(&tokens);
    about
}

fn create(p: &mut Cursor) -> About {
    let kind = p.kind().to_ascii_uppercase();
    p.eat(&["CONCURRENTLY"]);
    p.eat(&["IF", "NOT", "EXISTS"]);
    if !ON_TABLE.contains(&kind.as_str()) {
        return About::of(p.name().into_iter().collect(), None);
    }
    // `CREATE INDEX ON t (...)`: Postgres makes up the index name.
    let name = if p.at("ON") { None } else { p.name() };
    let table = if p.seek("ON") {
        p.eat(&["ONLY"]);
        p.name()
    } else {
        None
    };
    About::of(name.into_iter().chain(table.clone()).collect(), table)
}

fn drop(p: &mut Cursor) -> About {
    let kind = p.kind().to_ascii_uppercase();
    p.eat(&["CONCURRENTLY"]);
    p.eat(&["IF", "EXISTS"]);
    let mut names = Vec::new();
    while let Some(name) = p.name() {
        names.push(name);
        if !p.punct(',') {
            break;
        }
    }
    // `DROP TRIGGER t ON table` is about the table.
    let table = if ON_TABLE.contains(&kind.as_str()) && p.eat(&["ON"]) {
        p.name()
    } else {
        None
    };
    names.extend(table.clone());
    About::of(names, table)
}

/// The table, and with `VALUES` the first string literal of each row: in a seed such as
/// `INSERT INTO plans (id, key, config) VALUES (1, 'core', '{...}'), (2, 'pro', '{...}')` the
/// keys are `core` and `pro`.
fn insert(p: &mut Cursor) -> About {
    p.eat(&["INTO"]);
    p.eat(&["TABLE"]);
    let mut about = About::of(p.name().into_iter().collect(), None);
    if !p.seek("VALUES") {
        return about;
    }
    let mut depth = 0usize;
    let mut rows = 0usize;
    // Per row: whether its first literal has been seen, and that literal when it is key-like.
    let mut seen = false;
    let mut key: Option<String> = None;
    for token in p.rest() {
        match token {
            Token::Punct('(') => {
                if depth == 0 {
                    rows += 1;
                    seen = false;
                    key = None;
                }
                depth += 1;
            }
            Token::Punct(')') => {
                depth = depth.saturating_sub(1);
                if depth == 0
                    && let Some(k) = key.take()
                    && about.keys.len() < MAX_ROW_KEYS
                    && !about.keys.contains(&k)
                {
                    about.keys.push(k);
                }
            }
            Token::Str(s) if depth == 1 && !seen => {
                seen = true;
                key = Some(s.trim().to_string()).filter(|s| is_key_like(s));
            }
            // `ON CONFLICT`, `RETURNING`: the rows are over.
            Token::Word(_) if depth == 0 => break,
            _ => {}
        }
    }
    about.enumerates = rows >= 2;
    about
}

/// Whether the tokens list values: `CHECK (... IN (...))`, `AS ENUM (...)`, or a ClickHouse
/// `Enum8(...)` / `Enum16(...)` column type.
fn lists_values(tokens: &[Token]) -> bool {
    let is_word =
        |t: Option<&Token>, w: &str| matches!(t, Some(Token::Word(x)) if x.eq_ignore_ascii_case(w));
    let open = |t: Option<&Token>| t == Some(&Token::Punct('('));
    tokens.iter().enumerate().any(|(i, t)| match t {
        Token::Word(w) if w.eq_ignore_ascii_case("ENUM") => open(tokens.get(i + 1)),
        Token::Word(w) if w.eq_ignore_ascii_case("Enum8") || w.eq_ignore_ascii_case("Enum16") => {
            open(tokens.get(i + 1))
        }
        Token::Word(w) if w.eq_ignore_ascii_case("CHECK") && open(tokens.get(i + 1)) => {
            // An `IN (` inside the CHECK's parentheses.
            let mut depth = 0usize;
            for (j, t) in tokens.iter().enumerate().skip(i + 1) {
                match t {
                    Token::Punct('(') => depth += 1,
                    Token::Punct(')') => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            return false;
                        }
                    }
                    _ if is_word(Some(t), "IN") && open(tokens.get(j + 1)) => return true,
                    _ => {}
                }
            }
            false
        }
        _ => false,
    })
}

/// A short one-line literal that is not JSON: a name, a code, a status.
fn is_key_like(s: &str) -> bool {
    !s.is_empty()
        && s.chars().count() <= MAX_KEY_CHARS
        && !s.contains('\n')
        && !s.starts_with(['{', '['])
}

#[derive(Debug, PartialEq)]
enum Token {
    /// A keyword, bare identifier or number, as written.
    Word(String),
    /// A `"quoted"` or `` `quoted` `` identifier, without its quotes.
    Quoted(String),
    /// A string literal: the content of a `'...'` one, or the whole of a dollar-quoted one.
    Str(String),
    /// Any other character that is not whitespace.
    Punct(char),
}

fn tokenize(code: &str) -> Vec<Token> {
    let chars: Vec<char> = code.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        let end = if c.is_whitespace() {
            i + 1
        } else if c.is_alphanumeric() || c == '_' {
            let len = chars
                .get(i..)
                .and_then(|rest| {
                    rest.iter()
                        .position(|&c| !(c.is_alphanumeric() || c == '_'))
                })
                .unwrap_or(chars.len() - i);
            out.push(Token::Word(collect(&chars, i, i + len)));
            i + len
        } else if c == '\'' {
            let end = quoted_end(&chars, i, '\'', true);
            out.push(Token::Str(unquote(&chars, i, end, '\'')));
            end
        } else if c == '"' || c == '`' {
            let end = quoted_end(&chars, i, c, false);
            out.push(Token::Quoted(unquote(&chars, i, end, c)));
            end
        } else if let Some(end) = (c == '$').then(|| dollar_quoted_end(&chars, i)).flatten() {
            out.push(Token::Str(collect(&chars, i, end)));
            end
        } else {
            out.push(Token::Punct(c));
            i + 1
        };
        i = end.max(i + 1);
    }
    out
}

fn collect(chars: &[char], from: usize, to: usize) -> String {
    chars.get(from..to).unwrap_or_default().iter().collect()
}

/// The text between the quotes of `chars[from..to]`, with doubled quotes made single.
fn unquote(chars: &[char], from: usize, to: usize, quote: char) -> String {
    let inner = collect(chars, from + 1, to.saturating_sub(1).max(from + 1));
    let doubled: String = [quote, quote].iter().collect();
    inner.replace(&doubled, &quote.to_string())
}

/// Reads tokens left to right. Every method leaves the position unchanged when it does not match.
struct Cursor<'a> {
    tokens: &'a [Token],
    i: usize,
}

impl<'a> Cursor<'a> {
    /// True when the next token is the keyword `kw`.
    fn at(&self, kw: &str) -> bool {
        self.word_at(self.i)
            .is_some_and(|w| w.eq_ignore_ascii_case(kw))
    }

    fn word_at(&self, i: usize) -> Option<&'a str> {
        match self.tokens.get(i) {
            Some(Token::Word(w)) => Some(w),
            _ => None,
        }
    }

    /// The next token, when it is a word.
    fn word(&mut self) -> Option<&'a str> {
        let w = self.word_at(self.i)?;
        self.i += 1;
        Some(w)
    }

    /// The tokens from the position on.
    fn rest(&self) -> &'a [Token] {
        self.tokens.get(self.i..).unwrap_or_default()
    }

    /// Steps over the keywords `kws` when they come next, in order.
    fn eat(&mut self, kws: &[&str]) -> bool {
        let hit = kws.iter().enumerate().all(|(k, kw)| {
            self.word_at(self.i + k)
                .is_some_and(|w| w.eq_ignore_ascii_case(kw))
        });
        if hit {
            self.i += kws.len();
        }
        hit
    }

    fn punct(&mut self, c: char) -> bool {
        let hit = self.tokens.get(self.i) == Some(&Token::Punct(c));
        if hit {
            self.i += 1;
        }
        hit
    }

    /// The kind of object after `CREATE`, `ALTER`, `DROP` or `COMMENT ON`, past any modifiers:
    /// `TABLE`, `INDEX`, `VIEW`. Empty when there is none.
    fn kind(&mut self) -> String {
        while self
            .word_at(self.i)
            .is_some_and(|w| MODIFIERS.iter().any(|m| m.eq_ignore_ascii_case(w)))
        {
            self.i += 1;
        }
        self.word().unwrap_or_default().to_string()
    }

    /// A name, qualified or not and quoted or not, joined with dots and without quotes:
    /// `users`, `public.users`, `"Users"`.
    fn name(&mut self) -> Option<String> {
        let tokens = self.tokens;
        let mut parts = Vec::new();
        while let Some(Token::Word(part) | Token::Quoted(part)) = tokens.get(self.i) {
            parts.push(part.as_str());
            self.i += 1;
            let dotted = tokens.get(self.i) == Some(&Token::Punct('.'))
                && matches!(
                    tokens.get(self.i + 1),
                    Some(Token::Word(_) | Token::Quoted(_))
                );
            if !dotted {
                break;
            }
            self.i += 1;
        }
        (!parts.is_empty()).then(|| parts.join("."))
    }

    /// Moves just past the next keyword `kw` outside parentheses, when there is one.
    fn seek(&mut self, kw: &str) -> bool {
        let mut depth = 0usize;
        let tokens = self.tokens;
        for (j, token) in tokens.iter().enumerate().skip(self.i) {
            match token {
                Token::Punct('(') => depth += 1,
                Token::Punct(')') => depth = depth.saturating_sub(1),
                Token::Word(w) if depth == 0 && w.eq_ignore_ascii_case(kw) => {
                    self.i = j + 1;
                    return true;
                }
                _ => {}
            }
        }
        false
    }
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

    fn names(code: &str) -> (Option<String>, Vec<String>) {
        let a = about(code);
        (a.subject, a.names)
    }

    fn named(subject: &str, names: &[&str]) -> (Option<String>, Vec<String>) {
        (
            Some(subject.to_string()),
            names.iter().map(|n| n.to_string()).collect(),
        )
    }

    /// A typical seed migration: a version row, then plan rows whose config is JSON.
    const SEED: &str = "\
-- The first price list. It stays DRAFT until the
-- prices are checked.
INSERT INTO price_lists (version, status, reason)
VALUES (1, 'DRAFT', 'First list; prices unchecked');

INSERT INTO plans (price_list, key, config) VALUES
(1, 'free', '{\"name\":\"Free\",\"seats\":1}'::jsonb),
(1, 'pro', '{\"name\":\"Pro\",\"seats\":5}'::jsonb),
(1, 'team', '{\"name\":\"Team\",\"seats\":25}'::jsonb);
";

    #[test]
    fn seed_rows_are_named_by_their_keys() {
        let chunks = chunk_file("migrations/0002_seed_plans.sql", SEED);
        assert_eq!(chunks.len(), 2, "{chunks:#?}");

        // The comment above the first statement travels with it; the `;` inside the reason
        // string does not end it.
        assert_eq!(range(&chunks[0]), (1, 4));
        assert_eq!(syms(&chunks[0]), vec!["price_lists", "DRAFT"]);
        assert!(chunks[0].text.starts_with("-- The first price list."));

        assert_eq!(range(&chunks[1]), (6, 9));
        assert_eq!(syms(&chunks[1]), vec!["plans", "free", "pro", "team"]);
        assert_eq!(chunks[1].kind, ChunkKind::Item);
        assert!(chunks[1].text.ends_with("::jsonb);"));
    }

    #[test]
    fn statements_that_list_values_are_flagged() {
        let chunks = chunk_file("seed.sql", SEED);
        // One price list row is not a list; three plan rows are.
        assert!(!chunks[0].enumerates);
        assert!(chunks[1].enumerates);
        let flagged = |src: &str| chunk_file("m.sql", src).iter().any(|c| c.enumerates);
        assert!(flagged("CREATE TYPE status AS ENUM ('open', 'closed');"));
        assert!(flagged(
            "CREATE TABLE orgs (state text NOT NULL CHECK (state IN ('ACTIVE', 'LOCKED')));"
        ));
        assert!(flagged(
            "ALTER TABLE orgs ADD CONSTRAINT orgs_state CHECK (state IN ('ACTIVE', 'LOCKED'));"
        ));
        assert!(flagged(
            "CREATE TABLE f (severity Enum8('low' = 1, 'high' = 2)) ENGINE = Log;"
        ));
        assert!(!flagged("CREATE TABLE orgs (seats int CHECK (seats > 0));"));
        assert!(!flagged(
            "CREATE TABLE orgs (id int, note text DEFAULT 'IN (');"
        ));
        assert!(!flagged("INSERT INTO t SELECT * FROM u;"));
    }

    const SCHEMA: &str = "\
CREATE TABLE plans (
    id   bigint PRIMARY KEY,
    key  text NOT NULL,
    note text DEFAULT 'a; b'
);
CREATE UNIQUE INDEX plans_key ON plans (key);
CREATE INDEX ON plans (note);

-- Seats belong to a plan.
CREATE TABLE IF NOT EXISTS public.seats (
    plan_id bigint REFERENCES plans (id)
);
ALTER TABLE ONLY public.seats ADD COLUMN email text;
";

    #[test]
    fn a_table_and_its_indexes_share_a_chunk() {
        let chunks = chunk_file("schema.sql", SCHEMA);
        assert_eq!(chunks.len(), 2, "{chunks:#?}");
        assert_eq!(range(&chunks[0]), (1, 7));
        assert_eq!(syms(&chunks[0]), vec!["plans", "plans_key"]);
        // The comment above the second table starts its chunk.
        assert_eq!(range(&chunks[1]), (9, 13));
        assert_eq!(syms(&chunks[1]), vec!["public.seats"]);
    }

    #[test]
    fn statements_on_other_objects_get_their_own_chunks() {
        let src = "ALTER TABLE a ADD COLUMN x int;\nALTER TABLE b ADD COLUMN y int;\n";
        let chunks = chunk_file("m.sql", src);
        assert_eq!(chunks.len(), 2);
        assert_eq!(syms(&chunks[0]), vec!["a"]);
        assert_eq!(syms(&chunks[1]), vec!["b"]);
    }

    #[test]
    fn statements_on_one_line_share_a_chunk() {
        let chunks = chunk_file(
            "m.sql",
            "BEGIN; CREATE TABLE a (id int); CREATE TABLE b (id int);\n",
        );
        assert_eq!(chunks.len(), 1);
        assert_eq!(syms(&chunks[0]), vec!["a", "b"]);
    }

    #[test]
    fn statements_about_nothing_join_their_neighbours() {
        let src = "BEGIN;\n\nCREATE TABLE a (id int);\n\nCOMMIT;\n";
        let chunks = chunk_file("m.sql", src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(range(&chunks[0]), (1, 5));
        assert_eq!(syms(&chunks[0]), vec!["a"]);
    }

    #[test]
    fn a_file_of_queries_gets_line_chunks_without_symbols() {
        let src = "SELECT 1;\n\nSELECT name FROM plans WHERE key = $1;\n";
        let chunks = chunk_file("queries.sql", src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, ChunkKind::Lines);
        assert!(chunks[0].symbols.is_empty());
        assert_eq!(range(&chunks[0]), (1, 3));
    }

    #[test]
    fn a_group_stops_growing_at_60_lines() {
        let src: String = (0..70)
            .map(|i| format!("ALTER TABLE a ADD COLUMN c{i} int;\n"))
            .collect();
        let chunks = chunk_file("m.sql", &src);
        assert_eq!(chunks.len(), 2);
        assert_eq!(range(&chunks[0]), (1, 60));
        assert_eq!(range(&chunks[1]), (61, 70));
        assert!(chunks.iter().all(|c| syms(c) == vec!["a"]));
    }

    #[test]
    fn a_long_statement_becomes_windows_named_after_its_table_only() {
        let mut src = String::from("INSERT INTO plans (key) VALUES\n");
        for i in 0..200 {
            src.push_str(&format!("('plan{i}'),\n"));
        }
        src.push_str("('last');\n");
        let chunks = chunk_file("seed.sql", &src);
        assert!(chunks.len() > 1);
        assert!(
            chunks.iter().all(|c| syms(c) == vec!["plans"]),
            "{chunks:#?}"
        );
        assert_eq!(range(&chunks[0]), (1, 120));
        assert_covers_all_text(&src, &chunks);
    }

    #[test]
    fn semicolons_in_quotes_comments_and_function_bodies_do_not_split() {
        let src = "\
CREATE OR REPLACE FUNCTION touch() RETURNS trigger AS $$
BEGIN
    NEW.note := 'it''s; done'; -- a comment; with a semicolon
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
/* a block comment; /* nested; */ still a comment; */
CREATE TRIGGER plans_touch BEFORE UPDATE ON plans
    FOR EACH ROW EXECUTE FUNCTION touch();
COMMENT ON COLUMN plans.\"weird;name\" IS 'a; b';
";
        let chunks = chunk_file("m.sql", src);
        assert_eq!(chunks.len(), 2, "{chunks:#?}");
        assert_eq!(range(&chunks[0]), (1, 6));
        assert_eq!(syms(&chunks[0]), vec!["touch"]);
        // The trigger is on `plans`, and so is the column comment.
        assert_eq!(range(&chunks[1]), (7, 10));
        assert_eq!(
            syms(&chunks[1]),
            vec!["plans_touch", "plans", "plans.weird;name"]
        );
    }

    #[test]
    fn clickhouse_tables_are_named() {
        let src = "\
CREATE TABLE IF NOT EXISTS otel.spans ON CLUSTER '{cluster}' (
    ts DateTime64(9),
    name String
) ENGINE = ReplicatedMergeTree ORDER BY ts;
ALTER TABLE otel.spans ON CLUSTER '{cluster}' ADD COLUMN IF NOT EXISTS kind String;
CREATE MATERIALIZED VIEW IF NOT EXISTS otel.spans_by_name TO otel.names AS
SELECT name, count() AS n FROM otel.spans GROUP BY name;
";
        let chunks = chunk_file("ch/0001_spans.sql", src);
        assert_eq!(chunks.len(), 2, "{chunks:#?}");
        assert_eq!(syms(&chunks[0]), vec!["otel.spans"]);
        assert_eq!(range(&chunks[0]), (1, 5));
        assert_eq!(syms(&chunks[1]), vec!["otel.spans_by_name"]);
    }

    #[test]
    fn statement_heads_name_their_objects() {
        let cases = [
            ("CREATE TABLE plans (id int)", named("plans", &["plans"])),
            (
                "create temp table if not exists t (id int)",
                named("t", &["t"]),
            ),
            (
                "CREATE TABLE \"Plans\" (id int)",
                named("Plans", &["Plans"]),
            ),
            (
                "CREATE TABLE `db`.`plans` (id Int8)",
                named("db.plans", &["db.plans"]),
            ),
            (
                "CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS plans_key ON ONLY public.plans (key)",
                named("public.plans", &["plans_key", "public.plans"]),
            ),
            ("CREATE INDEX ON plans (key)", named("plans", &["plans"])),
            (
                "CREATE POLICY own ON plans USING (true)",
                named("plans", &["own", "plans"]),
            ),
            ("CREATE OR REPLACE VIEW v AS SELECT 1", named("v", &["v"])),
            (
                "CREATE MATERIALIZED VIEW mv AS SELECT 1",
                named("mv", &["mv"]),
            ),
            ("CREATE FUNCTION f(a int) RETURNS int", named("f", &["f"])),
            (
                "CREATE TYPE status AS ENUM ('a', 'b')",
                named("status", &["status"]),
            ),
            (
                "CREATE EXTENSION IF NOT EXISTS pgcrypto",
                named("pgcrypto", &["pgcrypto"]),
            ),
            (
                "ALTER TABLE IF EXISTS ONLY plans ADD COLUMN x int",
                named("plans", &["plans"]),
            ),
            (
                "ALTER MATERIALIZED VIEW mv RENAME TO mv2",
                named("mv", &["mv"]),
            ),
            ("DROP TABLE IF EXISTS a, b CASCADE", named("a", &["a", "b"])),
            (
                "DROP INDEX CONCURRENTLY IF EXISTS plans_key",
                named("plans_key", &["plans_key"]),
            ),
            (
                "DROP TRIGGER IF EXISTS plans_touch ON plans",
                named("plans", &["plans_touch", "plans"]),
            ),
            ("UPDATE ONLY plans SET x = 1", named("plans", &["plans"])),
            ("DELETE FROM plans WHERE x = 1", named("plans", &["plans"])),
            ("TRUNCATE TABLE plans", named("plans", &["plans"])),
            (
                "INSERT INTO plans SELECT * FROM old",
                named("plans", &["plans"]),
            ),
            ("COMMENT ON TABLE plans IS 'x'", named("plans", &["plans"])),
            (
                "COMMENT ON COLUMN plans.key IS 'x'",
                named("plans", &["plans.key"]),
            ),
        ];
        for (code, want) in cases {
            assert_eq!(names(code), want, "{code}");
        }
        for code in [
            "BEGIN",
            "SET search_path TO x",
            "SELECT 1",
            "GRANT ALL ON t TO r",
            "DO $$ BEGIN END $$",
            "",
            "(1)",
        ] {
            assert_eq!(about(code), About::default(), "{code}");
        }
    }

    #[test]
    fn row_keys_are_the_first_short_literal_of_each_row() {
        let keys = |code: &str| about(code).keys;
        assert_eq!(
            keys(
                "INSERT INTO t (id, k, v) VALUES (gen_random_uuid(), 'a', 'x'), (now(), 'b', 'y')"
            ),
            vec!["a", "b"]
        );
        // A literal inside a call is not the row's own; JSON and long text are not keys, and a
        // row whose first literal is not a key gives none.
        assert_eq!(
            keys(&format!(
                "INSERT INTO t VALUES (jsonb_build_object('k', 1), 'c'), ('{{}}', 'd'), ('{}', 'e')",
                "x".repeat(65)
            )),
            vec!["c"]
        );
        assert_eq!(
            keys("INSERT INTO t VALUES ('a'), ('a'), ('b') ON CONFLICT (k) DO UPDATE SET v = 'z'"),
            vec!["a", "b"]
        );
        let many: Vec<String> = (0..30).map(|i| format!("('k{i}')")).collect();
        assert_eq!(
            keys(&format!("INSERT INTO t VALUES {}", many.join(", "))).len(),
            MAX_ROW_KEYS
        );
        assert!(keys("INSERT INTO t SELECT 'a'").is_empty());
    }

    #[test]
    fn malformed_input_is_chunked_without_losing_text() {
        let inputs = [
            "SELECT 'unterminated;\nCREATE TABLE a (id int);\n",
            "CREATE FUNCTION f() AS $body$ never closed;\nmore;\n",
            "/* unclosed comment ;\nCREATE TABLE a (id int);\n",
            "SELECT $1, $2 FROM t WHERE x = $3;\nSELECT \"a;\n",
            "-- only a comment\n-- and another\n",
            ";;;\n;\n",
            "\u{feff}é€𝄞; DROP TABLE 𝄞;\r\nINSERT INTO \"t\" VALUES ('\\'', 'b');\r\n",
            "INSERT INTO t VALUES ((((('a'",
            "$",
            "$a",
            "'",
        ];
        for src in inputs {
            let chunks = chunk_file("m.sql", src);
            assert_covers_all_text(src, &chunks);
            for c in &chunks {
                assert!(
                    c.start_line >= 1 && c.start_line <= c.end_line,
                    "{src:?}: {c:?}"
                );
                assert!(c.end_line as usize <= src.lines().count(), "{src:?}: {c:?}");
            }
        }
    }

    #[test]
    fn comments_after_the_last_statement_belong_to_it() {
        let src = "CREATE TABLE a (id int);\n\n-- the end\n\n";
        let chunks = chunk_file("m.sql", src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(range(&chunks[0]), (1, 3));
    }
}
