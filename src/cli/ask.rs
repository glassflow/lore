//! `lore ask`: a cited answer to a question about the key's tenant.

use std::io::IsTerminal;

use anyhow::{Context, Result};
use clap::Args;
use serde_json::Value;

use super::client::{Auth, Client};
use crate::answer::{AskRequest, AskResponse, Citation, Mode};

#[derive(Args)]
pub struct AskArgs {
    /// The question
    question: String,
    /// fast: one search and one model call. sources: the same search and no model call, the
    /// sources as bullets. deep: an agent loop with tools and a time budget
    #[arg(long, value_enum, default_value_t = Mode::Fast)]
    mode: Mode,
    /// How many sources to retrieve (1 to 50)
    #[arg(long)]
    k: Option<usize>,
    /// Only these document kinds, comma-separated (code, doc, session, note)
    #[arg(long, value_delimiter = ',')]
    kind: Option<Vec<String>>,
    /// Only the sources with these names, comma-separated
    #[arg(long, value_delimiter = ',')]
    source: Vec<String>,
    /// Print the server's JSON on one line
    #[arg(long)]
    json: bool,
}

pub async fn run_ask(client: &Client, args: AskArgs) -> Result<()> {
    let body = AskRequest {
        question: args.question,
        mode: args.mode,
        k: args.k,
        kind: args.kind.filter(|k| !k.is_empty()),
        source: None,
        sources: crate::search::source_names(&args.source),
    };
    let reply: Value = client.post_json("/v1/ask", Auth::Key, &body).await?;
    if args.json {
        println!("{}", serde_json::to_string(&reply)?);
        return Ok(());
    }
    let reply: AskResponse =
        serde_json::from_value(reply).context("reading the server's ask reply")?;
    print!("{}", format_answer(&reply, std::io::stdout().is_terminal()));
    Ok(())
}

/// `path:start-end` for a citation, else its path, else its title.
fn place(c: &Citation) -> String {
    match (&c.path, c.start_line, c.end_line) {
        (Some(path), Some(start), Some(end)) => format!("{path}:{start}-{end}"),
        (Some(path), _, _) => path.clone(),
        (None, _, _) => c.title.clone().unwrap_or_default(),
    }
}

/// The answer, a blank line, the `Sources:` list (left out when nothing is cited), then a line
/// with the model, latency and token counts (`no model` and the latency in sources mode), dimmed
/// when `dim` is set. A sources answer already names each source on its bullet, so its links go
/// under the bullets instead of in a list. Every line ends with a newline.
fn format_answer(res: &AskResponse, dim: bool) -> String {
    let bullets: Vec<&str> = res.answer.trim_end().lines().collect();
    let mut out = String::new();
    if res.mode == Mode::Sources && bullets.len() == res.citations.len() {
        for (bullet, c) in bullets.iter().zip(&res.citations) {
            out.push_str(bullet);
            out.push('\n');
            if let Some(url) = &c.url {
                out.push_str(&format!("  {url}\n"));
            }
        }
        out.push('\n');
    } else {
        out.push_str(&format!("{}\n\n", res.answer.trim_end()));
    }
    if res.mode != Mode::Sources && !res.citations.is_empty() {
        out.push_str("Sources:\n");
        for c in &res.citations {
            out.push_str(format!("[{}] {}", c.label, place(c)).trim_end());
            out.push('\n');
            if let Some(url) = &c.url {
                out.push_str(&format!("  {url}\n"));
            }
        }
    }
    let stats = match res.mode {
        Mode::Sources => format!("no model \u{b7} {} ms", res.latency_ms),
        Mode::Fast | Mode::Deep => format!(
            "{} \u{b7} {} ms \u{b7} {} in / {} out",
            res.model, res.latency_ms, res.usage.input_tokens, res.usage.output_tokens
        ),
    };
    if dim {
        out.push_str(&format!("\x1b[2m{stats}\x1b[0m\n"));
    } else {
        out.push_str(&format!("{stats}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::answer::UsageOut;

    fn citation(label: &str) -> Citation {
        Citation {
            label: label.to_string(),
            document_id: None,
            kind: Some("code".to_string()),
            title: Some("Pricing".to_string()),
            path: Some("src/billing/plans.rs".to_string()),
            start_line: Some(7),
            end_line: Some(14),
            url: Some("https://example.test/plans.rs#L7-L14".to_string()),
        }
    }

    fn response(citations: Vec<Citation>) -> AskResponse {
        AskResponse {
            answer: "There are 3 plans [1].\n".to_string(),
            mode: Mode::Fast,
            model: "fake".to_string(),
            citations,
            sources_considered: 2,
            tool_calls: 0,
            usage: UsageOut {
                input_tokens: 120,
                output_tokens: 15,
            },
            latency_ms: 42,
        }
    }

    #[test]
    fn answer_sources_and_stats() {
        let mut second = citation("2");
        second.path = None;
        second.url = None;
        let out = format_answer(&response(vec![citation("1"), second]), false);
        assert_eq!(
            out,
            "There are 3 plans [1].\n\n\
             Sources:\n\
             [1] src/billing/plans.rs:7-14\n  https://example.test/plans.rs#L7-L14\n\
             [2] Pricing\n\
             fake \u{b7} 42 ms \u{b7} 120 in / 15 out\n"
        );
    }

    #[test]
    fn no_citations_means_no_sources_list() {
        let out = format_answer(&response(Vec::new()), false);
        assert_eq!(
            out,
            "There are 3 plans [1].\n\nfake \u{b7} 42 ms \u{b7} 120 in / 15 out\n"
        );
    }

    #[test]
    fn the_stats_line_is_dimmed_on_a_terminal() {
        let out = format_answer(&response(Vec::new()), true);
        assert!(
            out.ends_with("\x1b[2mfake \u{b7} 42 ms \u{b7} 120 in / 15 out\x1b[0m\n"),
            "{out:?}"
        );
    }

    #[test]
    fn sources_mode_puts_each_link_under_its_bullet_and_says_no_model() {
        let mut second = citation("2");
        second.url = None;
        let mut res = response(vec![citation("1"), second]);
        res.mode = Mode::Sources;
        res.answer =
            "- [1] `src/billing/plans.rs:7-14` (sample): Plan\n- [2] `b.rs:1-2` (sample)\n"
                .to_string();
        res.model = "none".to_string();
        res.usage = UsageOut::default();
        assert_eq!(
            format_answer(&res, false),
            "- [1] `src/billing/plans.rs:7-14` (sample): Plan\n  https://example.test/plans.rs#L7-L14\n\
             - [2] `b.rs:1-2` (sample)\n\n\
             no model \u{b7} 42 ms\n"
        );

        // Nothing found: the message, and no list.
        res.answer = "I found nothing indexed that matches this question.".to_string();
        res.citations = Vec::new();
        assert_eq!(
            format_answer(&res, false),
            "I found nothing indexed that matches this question.\n\nno model \u{b7} 42 ms\n"
        );
    }

    #[test]
    fn a_path_without_lines_shows_the_path_alone() {
        let mut c = citation("1");
        c.start_line = None;
        assert_eq!(place(&c), "src/billing/plans.rs");
    }
}
