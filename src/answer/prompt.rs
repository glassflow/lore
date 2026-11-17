//! The prompt of fast mode: a fixed system prompt, and the retrieved passages as numbered
//! sources followed by the question.

use crate::decision;
use crate::search::Hit;

pub const FAST_SYSTEM: &str = "You answer questions about a team's code, documents and agent sessions. \
Use only the numbered sources. Cite every claim with its source number in square brackets, like [2]. \
A source marked decision says who decided and when: when you answer from one, say who decided and on what date. \
If the sources do not contain the answer, say so and say what is missing. \
Answer in at most 8 short sentences or a short list. Count carefully when asked how many.";

/// The system prompt of deep mode. The model finds its own evidence with the tools and cites
/// code as `[path:start-end]` (the path without the source prefix) and sessions as `[session:<id>]`.
pub const DEEP_SYSTEM: &str = "You answer questions about a team's code, documents and agent sessions. \
Use the tools to find evidence before answering: search for relevant passages, grep for exact names, read_file to check details, get_session to read an agent session. \
For 'how many' or 'list all' questions, grep for every occurrence instead of trusting search alone. \
Cite every claim as [path:start-end] for code, using the path without the source prefix, [session:<id>] for sessions, or [doc:<id>] for a hit shown as doc:<id>. \
A decision hit names who decided and when: say so when you answer from it. \
Be concise.";

/// The most lines of a passage that go into the prompt.
const MAX_LINES: usize = 120;

const PREFIX: &str = "Sources:\n\n";

/// One numbered source: a header line, then the text in a fence cut to its first 120 lines.
fn render_source(n: usize, hit: &Hit) -> String {
    // A decision names who decided and when where the other kinds name the kind.
    let kind = match &hit.decision {
        Some(decision) if hit.kind == decision::KIND => decision.label(),
        _ => hit.kind.clone(),
    };
    let header = match (&hit.path, hit.start_line, hit.end_line) {
        (Some(path), Some(start), Some(end)) => format!("[{n}] {path}:{start}-{end} ({kind})"),
        (Some(path), _, _) => format!("[{n}] {path} ({kind})"),
        (None, _, _) => format!("[{n}] {} ({kind})", hit.title),
    };
    let text = hit
        .text
        .lines()
        .take(MAX_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    format!("{header}\n```\n{text}\n```")
}

/// The user message: `Sources:`, the hits numbered from 1 in the order given, then the question.
/// Hits are added while the whole message stays within `max_chars`. Returns the message and how
/// many sources it holds.
pub fn fast_user_message(question: &str, hits: &[Hit], max_chars: usize) -> (String, usize) {
    let suffix = format!("\n\nQuestion: {question}");
    let mut used = PREFIX.chars().count() + suffix.chars().count();
    let mut sources: Vec<String> = Vec::new();
    for (i, hit) in hits.iter().enumerate() {
        let source = render_source(i + 1, hit);
        // Sources are joined by a blank line.
        let sep = if sources.is_empty() { 0 } else { 2 };
        let cost = sep + source.chars().count();
        if used + cost > max_chars {
            break;
        }
        used += cost;
        sources.push(source);
    }
    let message = format!("{PREFIX}{}{suffix}", sources.join("\n\n"));
    (message, sources.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decision::Decision;
    use uuid::Uuid;

    fn hit(path: Option<&str>, text: &str) -> Hit {
        Hit {
            chunk_id: 1,
            document_id: Uuid::nil(),
            source: "sample".to_string(),
            kind: "code".to_string(),
            title: "Standup notes".to_string(),
            uri: None,
            path: path.map(str::to_string),
            external_id: None,
            start_line: path.map(|_| 7),
            end_line: path.map(|_| 14),
            symbols: Vec::new(),
            decision: None,
            text: text.to_string(),
            score: 0.03,
        }
    }

    #[test]
    fn sources_are_numbered_from_one() {
        let hits = vec![
            hit(Some("src/a.rs"), "fn a() {}"),
            hit(None, "meeting notes"),
        ];
        let (msg, n) = fast_user_message("what is a?", &hits, 60_000);
        assert_eq!(n, 2);
        assert_eq!(
            msg,
            "Sources:\n\n\
             [1] src/a.rs:7-14 (code)\n```\nfn a() {}\n```\n\n\
             [2] Standup notes (code)\n```\nmeeting notes\n```\n\n\
             Question: what is a?"
        );
    }

    #[test]
    fn a_session_hit_is_listed_by_its_title_and_kind() {
        let mut h = hit(None, "user: How many plans do we have?");
        h.kind = "session".to_string();
        h.title = "How many plans do we have?".to_string();
        h.start_line = None;
        h.end_line = None;
        h.external_id = Some("sess-1".to_string());
        let (msg, n) = fast_user_message("q", &[h], 60_000);
        assert_eq!(n, 1);
        assert!(
            msg.contains("[1] How many plans do we have? (session)\n```\n"),
            "{msg}"
        );
    }

    fn decision_hit(metadata: serde_json::Value) -> Hit {
        let mut h = hit(None, "We use Postgres for the queue.");
        h.kind = "decision".to_string();
        h.title = "Queue storage".to_string();
        h.start_line = None;
        h.end_line = None;
        h.decision = Decision::from_metadata(&metadata);
        h
    }

    #[test]
    fn a_decision_source_says_who_decided_and_when() {
        let h = decision_hit(serde_json::json!({
            "decided_by": ["Ashish Bagri"],
            "decided_at": "2026-08-03"
        }));
        let (msg, _) = fast_user_message("q", &[h], 60_000);
        assert!(
            msg.contains(
                "[1] Queue storage (decision, decided by Ashish Bagri on 3 August 2026)\n```\n"
            ),
            "{msg}"
        );
    }

    #[test]
    fn a_decision_with_missing_or_empty_values_prints_no_placeholder() {
        for (metadata, header) in [
            (
                serde_json::json!({ "decided_by": ["Ana", "Bo"] }),
                "[1] Queue storage (decision, decided by Ana and Bo)",
            ),
            (
                serde_json::json!({ "decided_at": "2026-08-03", "decided_by": [] }),
                "[1] Queue storage (decision, decided on 3 August 2026)",
            ),
            (
                serde_json::json!({ "decided_by": [" ", null, 3], "decided_at": "" }),
                "[1] Queue storage (decision)",
            ),
            (serde_json::json!({}), "[1] Queue storage (decision)"),
            (serde_json::json!(null), "[1] Queue storage (decision)"),
        ] {
            let h = decision_hit(metadata.clone());
            let (msg, n) = fast_user_message("q", &[h], 60_000);
            assert_eq!(n, 1);
            assert!(msg.contains(header), "{metadata}: {msg}");
            assert!(!msg.contains("None") && !msg.contains("null"), "{msg}");
        }
    }

    #[test]
    fn a_decision_hit_that_lost_its_metadata_falls_back_to_its_kind() {
        let mut h = decision_hit(serde_json::json!({}));
        h.decision = None;
        let (msg, _) = fast_user_message("q", &[h], 60_000);
        assert!(msg.contains("[1] Queue storage (decision)\n"), "{msg}");
    }

    #[test]
    fn stops_before_max_chars() {
        let hits = vec![
            hit(Some("src/a.rs"), &"x".repeat(100)),
            hit(Some("src/b.rs"), &"y".repeat(100)),
            hit(Some("src/c.rs"), &"z".repeat(100)),
        ];
        let (full, n) = fast_user_message("q", &hits, usize::MAX);
        assert_eq!(n, 3);
        let one = fast_user_message("q", &hits[..1], usize::MAX)
            .0
            .chars()
            .count();
        let two = fast_user_message("q", &hits[..2], usize::MAX)
            .0
            .chars()
            .count();
        assert!(one < two && two < full.chars().count());

        // Exactly enough room for two sources: the third is left out.
        let (msg, n) = fast_user_message("q", &hits, two);
        assert_eq!(n, 2);
        assert_eq!(msg.chars().count(), two);
        assert!(!msg.contains("src/c.rs"));
        // One char less than that and only the first fits.
        let (msg, n) = fast_user_message("q", &hits, two - 1);
        assert_eq!(n, 1);
        assert!(msg.ends_with("\n\nQuestion: q"));
        // Nothing fits.
        let (msg, n) = fast_user_message("q", &hits, one - 1);
        assert_eq!(n, 0);
        assert_eq!(msg, "Sources:\n\n\n\nQuestion: q");
    }

    #[test]
    fn each_source_is_cut_to_120_lines() {
        let text: String = (1..=200).map(|i| format!("line {i}\n")).collect();
        let (msg, n) = fast_user_message("q", &[hit(Some("src/a.rs"), &text)], 60_000);
        assert_eq!(n, 1);
        assert!(msg.contains("line 120\n```"));
        assert!(!msg.contains("line 121"));
    }

    #[test]
    fn a_path_without_lines_shows_the_path_alone() {
        let mut h = hit(Some("README.md"), "hello");
        h.start_line = None;
        h.end_line = None;
        let (msg, _) = fast_user_message("q", &[h], 60_000);
        assert!(msg.contains("[1] README.md (code)\n"), "{msg}");
    }
}
