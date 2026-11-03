//! Sources mode: the hybrid search fast mode runs, and no model call. The answer lists the hits as
//! bullets, best first: where each one is, its symbols, and the line that best matches the
//! question. It answers in the time of a search and costs nothing.

use uuid::Uuid;

use super::fast::{DEFAULT_K, NOTHING_FOUND};
use super::{AskRequest, AskResponse, Citation, Mode, UsageOut};
use crate::error::AppError;
use crate::http::AppState;
use crate::search::{Hit, SearchQuery, query_terms, search};

/// What `model` says when no model was called.
const NO_MODEL: &str = "none";
/// A bullet names at most this many symbols.
const BULLET_SYMBOLS: usize = 6;
/// The quoted line is cut to this many characters.
const LINE_CHARS: usize = 120;

pub async fn answer(
    state: &AppState,
    tenant_id: Uuid,
    req: &AskRequest,
) -> Result<AskResponse, AppError> {
    let question = req.question.trim();
    let hits = search(
        &state.pool,
        state.embedder.clone(),
        SearchQuery {
            tenant_id,
            q: question.to_string(),
            k: req.k.unwrap_or(DEFAULT_K),
            kinds: req.kind.clone(),
            source: req.source.clone().filter(|s| !s.trim().is_empty()),
        },
    )
    .await?;
    Ok(response(question, &hits))
}

/// The answer for `hits`: one bullet and one citation per hit, labelled 1, 2, ... in rank order.
fn response(question: &str, hits: &[Hit]) -> AskResponse {
    let answer = if hits.is_empty() {
        NOTHING_FOUND.to_string()
    } else {
        let terms = query_terms(question);
        let bullets: Vec<String> = hits
            .iter()
            .enumerate()
            .map(|(i, h)| bullet(i + 1, h, &terms))
            .collect();
        bullets.join("\n")
    };
    AskResponse {
        answer,
        mode: Mode::Sources,
        model: NO_MODEL.to_string(),
        citations: hits
            .iter()
            .enumerate()
            .map(|(i, h)| citation(i + 1, h))
            .collect(),
        sources_considered: hits.len(),
        tool_calls: 0,
        usage: UsageOut::default(),
        latency_ms: 0,
    }
}

/// ``- [1] `src/billing/plans.rs:3-14` (sample): Plan, Plan::limits — `pub enum Plan {`` with the
/// title in place of the path for documents without one. Backticks inside the place or the line
/// become `'`, so they cannot end the code span early.
fn bullet(n: usize, h: &Hit, terms: &[String]) -> String {
    let place = match (&h.path, h.start_line, h.end_line) {
        (Some(path), Some(start), Some(end)) => format!("{path}:{start}-{end}"),
        (Some(path), _, _) => path.clone(),
        (None, _, _) => h.title.clone(),
    };
    let mut out = format!("- [{n}] `{}` ({})", place.replace('`', "'"), h.source);
    if !h.symbols.is_empty() {
        let mut symbols = h.symbols[..h.symbols.len().min(BULLET_SYMBOLS)].join(", ");
        if h.symbols.len() > BULLET_SYMBOLS {
            symbols.push_str(", …");
        }
        out.push_str(": ");
        out.push_str(&symbols);
    }
    if let Some(line) = best_line(&h.text, terms) {
        out.push_str(&format!(" — `{}`", line.replace('`', "'")));
    }
    out
}

/// The line of `text` with the most query words (the first of equals), or the first non-blank
/// line when none has any. Trimmed and cut to [`LINE_CHARS`] characters.
fn best_line(text: &str, terms: &[String]) -> Option<String> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let mut best: Option<(&str, usize)> = None;
    for line in &lines {
        let lower = line.to_lowercase();
        let matches = terms.iter().filter(|t| lower.contains(t.as_str())).count();
        if matches > 0 && best.is_none_or(|(_, most)| matches > most) {
            best = Some((line, matches));
        }
    }
    let line = best.map(|(l, _)| l).or_else(|| lines.first().copied())?;
    Some(match line.char_indices().nth(LINE_CHARS) {
        Some(_) => format!("{}…", line.chars().take(LINE_CHARS - 1).collect::<String>()),
        None => line.to_string(),
    })
}

fn citation(n: usize, h: &Hit) -> Citation {
    Citation {
        label: n.to_string(),
        document_id: Some(h.document_id),
        kind: Some(h.kind.clone()),
        title: Some(h.title.clone()),
        path: h.path.clone(),
        start_line: h.start_line,
        end_line: h.end_line,
        url: h.uri.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(path: Option<&str>, symbols: &[&str], text: &str) -> Hit {
        Hit {
            chunk_id: 1,
            document_id: Uuid::nil(),
            source: "sample".to_string(),
            kind: "code".to_string(),
            title: "Standup notes".to_string(),
            uri: Some("https://git.example.com/x".to_string()),
            path: path.map(str::to_string),
            external_id: None,
            start_line: path.map(|_| 3),
            end_line: path.map(|_| 14),
            symbols: symbols.iter().map(|s| s.to_string()).collect(),
            text: text.to_string(),
            score: 0.03,
        }
    }

    fn terms(q: &str) -> Vec<String> {
        query_terms(q)
    }

    #[test]
    fn a_bullet_has_the_place_source_symbols_and_best_line() {
        let h = hit(
            Some("src/billing/plans.rs"),
            &["Plan", "Plan::limits"],
            "/// Plans.\npub enum Plan { Free, Pro, Team }\nfn limits() {}",
        );
        assert_eq!(
            bullet(1, &h, &terms("which plans are there")),
            "- [1] `src/billing/plans.rs:3-14` (sample): Plan, Plan::limits — `/// Plans.`"
        );
        assert_eq!(
            bullet(2, &h, &terms("the limits")),
            "- [2] `src/billing/plans.rs:3-14` (sample): Plan, Plan::limits — `fn limits() {}`"
        );
    }

    #[test]
    fn a_bullet_without_a_path_names_the_title_and_skips_empty_parts() {
        let h = hit(None, &[], "");
        assert_eq!(bullet(1, &h, &[]), "- [1] `Standup notes` (sample)");
    }

    #[test]
    fn many_symbols_are_cut_and_backticks_cannot_end_the_code_span() {
        let many: Vec<String> = (1..=8).map(|i| format!("s{i}")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        let h = hit(Some("a`b.rs"), &many, "let s = `raw`;");
        assert_eq!(
            bullet(1, &h, &[]),
            "- [1] `a'b.rs:3-14` (sample): s1, s2, s3, s4, s5, s6, … — `let s = 'raw';`"
        );
    }

    #[test]
    fn the_best_line_is_the_one_with_most_query_words_else_the_first() {
        let text = "\n  fn a() {}\n  // billing\n  // billing plan\n  // plan billing again\n";
        assert_eq!(
            best_line(text, &terms("billing plan")).as_deref(),
            Some("// billing plan")
        );
        assert_eq!(
            best_line(text, &terms("nothing")).as_deref(),
            Some("fn a() {}")
        );
        assert_eq!(best_line("\n \n", &[]), None);
        let long = "x".repeat(300);
        let cut = best_line(&long, &[]).unwrap();
        assert_eq!(cut.chars().count(), LINE_CHARS);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn the_response_cites_every_hit_in_order_and_calls_no_model() {
        let hits = vec![hit(Some("a.rs"), &[], "a"), hit(Some("b.rs"), &[], "b")];
        let res = response("q", &hits);
        assert_eq!(res.mode, Mode::Sources);
        assert_eq!(res.model, "none");
        assert_eq!(res.usage, UsageOut::default());
        assert_eq!(res.sources_considered, 2);
        let labels: Vec<&str> = res.citations.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["1", "2"]);
        assert_eq!(res.citations[1].path.as_deref(), Some("b.rs"));
        assert_eq!(res.answer.lines().count(), 2);
        assert!(res.answer.starts_with("- [1] `a.rs:3-14`"));
    }

    #[test]
    fn no_hits_say_so() {
        let res = response("q", &[]);
        assert_eq!(res.answer, NOTHING_FOUND);
        assert!(res.citations.is_empty());
    }
}
