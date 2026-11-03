//! `lore search`: hybrid search over the key's tenant.

use anyhow::{Context, Result};
use clap::Args;
use serde::Deserialize;
use serde_json::Value;

use super::client::{Auth, Client};

/// Snippet lines printed under each hit.
const SNIPPET_LINES: usize = 3;

#[derive(Args)]
pub struct SearchArgs {
    /// What to look for
    query: String,
    /// How many hits to return (1 to 50)
    #[arg(long)]
    k: Option<u32>,
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

/// The parts of a hit the plain output uses.
#[derive(Deserialize)]
struct Hit {
    kind: String,
    title: String,
    path: Option<String>,
    start_line: Option<i32>,
    end_line: Option<i32>,
    symbols: Vec<String>,
    snippet: String,
    score: f64,
}

#[derive(Deserialize)]
struct SearchResponse {
    hits: Vec<Hit>,
}

pub async fn run_search(client: &Client, args: SearchArgs) -> Result<()> {
    let mut query = vec![("q", args.query)];
    if let Some(k) = args.k {
        query.push(("k", k.to_string()));
    }
    if let Some(kinds) = args.kind.filter(|k| !k.is_empty()) {
        query.push(("kind", kinds.join(",")));
    }
    if let Some(names) = crate::search::source_names(&args.source) {
        query.push(("source", names.join(",")));
    }
    let reply: Value = client
        .get_json_query("/v1/search", Auth::Key, &query)
        .await?;
    if args.json {
        println!("{}", serde_json::to_string(&reply)?);
        return Ok(());
    }
    let reply: SearchResponse =
        serde_json::from_value(reply).context("reading the server's search reply")?;
    if reply.hits.is_empty() {
        eprintln!("no results");
    }
    for hit in &reply.hits {
        print!("{}", format_hit(hit));
    }
    Ok(())
}

/// `path:start-end  (score 0.032)  symbols`, then the first 3 snippet lines indented by 4
/// spaces. Without a path the title takes its place (`session <title>` for a session), and
/// without symbols the last column is left out. Every line ends with a newline.
fn format_hit(hit: &Hit) -> String {
    let place = match (&hit.path, hit.start_line, hit.end_line) {
        (Some(path), Some(start), Some(end)) => format!("{path}:{start}-{end}"),
        (Some(path), _, _) => path.clone(),
        (None, _, _) if hit.kind == "session" => format!("session {}", hit.title),
        (None, _, _) => hit.title.clone(),
    };
    let mut head = format!("{place}  (score {:.3})", hit.score);
    if !hit.symbols.is_empty() {
        head.push_str("  ");
        head.push_str(&hit.symbols.join(", "));
    }
    let mut out = format!("{head}\n");
    for line in hit.snippet.lines().take(SNIPPET_LINES) {
        out.push_str(format!("    {line}").trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit() -> Hit {
        Hit {
            kind: "code".to_string(),
            title: "Standup notes".to_string(),
            path: Some("src/billing/plans.rs".to_string()),
            start_line: Some(7),
            end_line: Some(14),
            symbols: vec!["limits".to_string(), "Plan::limits".to_string()],
            snippet: "/// Returns the limits.\npub fn limits() {\n    body\n    more\n".to_string(),
            score: 0.032_786,
        }
    }

    #[test]
    fn hit_with_a_path() {
        assert_eq!(
            format_hit(&hit()),
            "src/billing/plans.rs:7-14  (score 0.033)  limits, Plan::limits\n    \
             /// Returns the limits.\n    pub fn limits() {\n        body\n"
        );
    }

    #[test]
    fn hit_without_a_path_shows_the_title() {
        let mut h = hit();
        h.path = None;
        h.start_line = None;
        h.end_line = None;
        h.symbols.clear();
        h.snippet = "one\n\nthree".to_string();
        assert_eq!(
            format_hit(&h),
            "Standup notes  (score 0.033)\n    one\n\n    three\n"
        );
    }

    #[test]
    fn hit_with_a_path_but_no_lines() {
        let mut h = hit();
        h.start_line = None;
        h.symbols.clear();
        h.snippet = String::new();
        assert_eq!(format_hit(&h), "src/billing/plans.rs  (score 0.033)\n");
    }

    #[test]
    fn session_hit_is_marked_with_its_kind() {
        let mut h = hit();
        h.kind = "session".to_string();
        h.title = "How many plans do we have?".to_string();
        h.path = None;
        h.start_line = None;
        h.end_line = None;
        h.symbols.clear();
        h.snippet = "user: How many plans do we have?".to_string();
        assert_eq!(
            format_hit(&h),
            "session How many plans do we have?  (score 0.033)\n    user: How many plans do we have?\n"
        );
    }
}
