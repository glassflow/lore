//! A deterministic model for tests, `make run`, the kind deployment and acceptance checks. It
//! never makes a network call.

use std::sync::LazyLock;

use anyhow::Result;
use async_trait::async_trait;
use regex::Regex;
use serde_json::json;

use super::{Block, Llm, LlmRequest, LlmResponse, Message, Role, ToolChoice, Usage};

/// The most citations a fake answer carries.
const MAX_CITATIONS: usize = 5;

/// `path:start-end`, the form deep-mode tool results use for a passage.
static PASSAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"([A-Za-z0-9_./-]+):(\d+)-(\d+)").expect("the passage pattern is valid")
});

/// A line that starts with `[<n>]`, the form fast mode numbers its sources in.
static SOURCE_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[(\d+)\]").expect("the source pattern is valid"));

#[derive(Debug, Clone, Copy, Default)]
pub struct FakeLlm {
    always_tools: bool,
}

impl FakeLlm {
    pub fn new() -> Self {
        Self::default()
    }

    /// A variant that returns a search tool call on every call while `tool_choice` is `Auto`,
    /// whatever the history holds. With `ToolChoice::None` it answers with text like the base
    /// fake. Tests use it to exhaust a tool-call budget.
    pub fn always_tools() -> Self {
        Self { always_tools: true }
    }
}

fn texts(m: &Message) -> String {
    m.content
        .iter()
        .filter_map(|b| match b {
            Block::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn has_tool_result(req: &LlmRequest) -> bool {
    req.messages
        .iter()
        .flat_map(|m| &m.content)
        .any(|b| matches!(b, Block::ToolResult { .. }))
}

fn last_tool_result(req: &LlmRequest) -> Option<&str> {
    req.messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            Block::ToolResult { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .next_back()
}

/// The characters of everything the request carries: system prompt, message text, tool calls
/// and results.
fn request_chars(req: &LlmRequest) -> u64 {
    let mut n = req.system.chars().count();
    for b in req.messages.iter().flat_map(|m| &m.content) {
        n += match b {
            Block::Text { text } => text.chars().count(),
            Block::ToolUse { name, input, .. } => {
                name.chars().count() + input.to_string().chars().count()
            }
            Block::ToolResult { content, .. } => content.chars().count(),
            Block::Raw(v) => v.to_string().chars().count(),
        };
    }
    n as u64
}

fn usage(req: &LlmRequest, output_chars: usize) -> Usage {
    Usage {
        input_tokens: request_chars(req) / 4,
        output_tokens: output_chars as u64 / 4,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
    }
}

fn text_response(req: &LlmRequest, answer: String) -> LlmResponse {
    let usage = usage(req, answer.chars().count());
    LlmResponse {
        content: vec![Block::Text { text: answer }],
        stop_reason: "end_turn".to_string(),
        usage,
        model: "fake".to_string(),
    }
}

fn tool_call_response(req: &LlmRequest) -> LlmResponse {
    let query = req
        .messages
        .iter()
        .find(|m| m.role == Role::User)
        .map(texts)
        .unwrap_or_default();
    let (name, input) = match req.tools.iter().find(|t| t.name == "search") {
        Some(t) => (t.name.clone(), json!({ "query": query })),
        None => (
            req.tools
                .first()
                .map(|t| t.name.clone())
                .unwrap_or_default(),
            json!({}),
        ),
    };
    let usage = usage(req, input.to_string().chars().count());
    LlmResponse {
        content: vec![Block::ToolUse {
            id: "fake-1".to_string(),
            name,
            input,
        }],
        stop_reason: "tool_use".to_string(),
        usage,
        model: "fake".to_string(),
    }
}

/// Deep mode's final answer: cites the first five distinct `path:start-end` passages of the
/// last tool result.
fn deep_answer(result: &str) -> String {
    let mut cited: Vec<&str> = Vec::new();
    for m in PASSAGE.find_iter(result) {
        if !cited.contains(&m.as_str()) {
            cited.push(m.as_str());
        }
        if cited.len() == MAX_CITATIONS {
            break;
        }
    }
    let mut answer = "Fake deep answer.".to_string();
    for c in cited {
        answer.push_str(&format!(" [{c}]"));
    }
    answer
}

/// Fast mode's answer: cites the first five of the numbered sources in the last user message.
fn fast_answer(req: &LlmRequest) -> String {
    let prompt = req
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .map(texts)
        .unwrap_or_default();
    let mut numbers: Vec<&str> = Vec::new();
    for line in prompt.lines() {
        if let Some(c) = SOURCE_LINE.captures(line) {
            let n = c.get(1).map_or("", |m| m.as_str());
            if !numbers.contains(&n) {
                numbers.push(n);
            }
        }
    }
    if numbers.is_empty() {
        return "Fake answer: no sources.".to_string();
    }
    let mut answer = format!("Fake answer from {} sources.", numbers.len());
    for n in numbers.iter().take(MAX_CITATIONS) {
        answer.push_str(&format!(" [{n}]"));
    }
    answer
}

#[async_trait]
impl Llm for FakeLlm {
    fn name(&self) -> &str {
        "fake"
    }

    fn supports_tools(&self) -> bool {
        true
    }

    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse> {
        let tools_on = !req.tools.is_empty() && req.tool_choice == ToolChoice::Auto;
        if tools_on && (self.always_tools || !has_tool_result(&req)) {
            return Ok(tool_call_response(&req));
        }
        // Tool use is over: a tool result came back, or the tools were turned off.
        if has_tool_result(&req) || (!req.tools.is_empty() && req.tool_choice == ToolChoice::None) {
            let answer = deep_answer(last_tool_result(&req).unwrap_or(""));
            return Ok(text_response(&req, answer));
        }
        let answer = fast_answer(&req);
        Ok(text_response(&req, answer))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolDef;

    fn user(text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![Block::Text { text: text.into() }],
        }
    }

    fn tool(name: &str) -> ToolDef {
        ToolDef {
            name: name.into(),
            description: "d".into(),
            input_schema: json!({"type": "object"}),
        }
    }

    fn request(messages: Vec<Message>, tools: Vec<ToolDef>, tool_choice: ToolChoice) -> LlmRequest {
        LlmRequest {
            model: "fake".into(),
            system: "sys".into(),
            messages,
            tools,
            tool_choice,
            max_tokens: 100,
            effort: None,
        }
    }

    fn text(res: &LlmResponse) -> &str {
        match res.content.as_slice() {
            [Block::Text { text }] => text,
            other => panic!("expected one text block, got {other:?}"),
        }
    }

    fn tool_result(content: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![Block::ToolResult {
                tool_use_id: "fake-1".into(),
                content: content.into(),
                is_error: false,
            }],
        }
    }

    fn call_turn() -> Message {
        Message {
            role: Role::Assistant,
            content: vec![Block::ToolUse {
                id: "fake-1".into(),
                name: "search".into(),
                input: json!({"query": "q"}),
            }],
        }
    }

    #[test]
    fn identity() {
        let fake = FakeLlm::new();
        assert_eq!(fake.name(), "fake");
        assert!(fake.supports_tools());
    }

    #[tokio::test]
    async fn first_call_with_tools_searches_for_the_first_user_message() {
        let req = request(
            vec![user("how do plan limits work?")],
            vec![tool("read_file"), tool("search")],
            ToolChoice::Auto,
        );
        let res = FakeLlm::new().complete(req).await.unwrap();
        assert_eq!(res.stop_reason, "tool_use");
        assert_eq!(res.model, "fake");
        assert_eq!(
            res.content,
            vec![Block::ToolUse {
                id: "fake-1".into(),
                name: "search".into(),
                input: json!({"query": "how do plan limits work?"}),
            }]
        );
    }

    #[tokio::test]
    async fn without_a_search_tool_the_first_tool_gets_empty_input() {
        let req = request(
            vec![user("q")],
            vec![tool("read_file"), tool("list")],
            ToolChoice::Auto,
        );
        let res = FakeLlm::new().complete(req).await.unwrap();
        assert_eq!(
            res.content,
            vec![Block::ToolUse {
                id: "fake-1".into(),
                name: "read_file".into(),
                input: json!({}),
            }]
        );
    }

    #[tokio::test]
    async fn a_tool_result_gets_a_deep_answer_with_five_distinct_passages() {
        let result = "src/a.rs:1-10 text\nsrc/b.rs:5-9\nsrc/a.rs:1-10 again\n\
                      docs/c.md:2-3 x/d.py:7-8 e.go:1-2 f.ts:3-4 g.rs:9-9";
        let req = request(
            vec![user("q"), call_turn(), tool_result(result)],
            vec![tool("search")],
            ToolChoice::Auto,
        );
        let res = FakeLlm::new().complete(req.clone()).await.unwrap();
        assert_eq!(res.stop_reason, "end_turn");
        assert_eq!(
            text(&res),
            "Fake deep answer. [src/a.rs:1-10] [src/b.rs:5-9] [docs/c.md:2-3] [x/d.py:7-8] [e.go:1-2]"
        );
        // always_tools still answers once the tools are turned off.
        let mut off = req;
        off.tool_choice = ToolChoice::None;
        let res = FakeLlm::always_tools().complete(off).await.unwrap();
        assert!(text(&res).starts_with("Fake deep answer. [src/a.rs:1-10]"));
    }

    #[tokio::test]
    async fn only_the_last_tool_result_is_cited() {
        let req = request(
            vec![
                user("q"),
                call_turn(),
                tool_result("old.rs:1-2"),
                call_turn(),
                tool_result("new.rs:3-4"),
            ],
            vec![tool("search")],
            ToolChoice::Auto,
        );
        let res = FakeLlm::new().complete(req).await.unwrap();
        assert_eq!(text(&res), "Fake deep answer. [new.rs:3-4]");
    }

    #[tokio::test]
    async fn tool_choice_none_answers_with_text_even_without_matches() {
        let req = request(vec![user("q")], vec![tool("search")], ToolChoice::None);
        let res = FakeLlm::new().complete(req).await.unwrap();
        assert_eq!(res.stop_reason, "end_turn");
        assert_eq!(text(&res), "Fake deep answer.");
    }

    #[tokio::test]
    async fn always_tools_calls_search_on_every_auto_call() {
        let fake = FakeLlm::always_tools();
        let req = request(
            vec![user("q"), call_turn(), tool_result("a.rs:1-2")],
            vec![tool("search")],
            ToolChoice::Auto,
        );
        for _ in 0..3 {
            let res = fake.complete(req.clone()).await.unwrap();
            assert_eq!(res.stop_reason, "tool_use");
        }
        // The plain fake would answer here.
        let res = FakeLlm::new().complete(req).await.unwrap();
        assert_eq!(res.stop_reason, "end_turn");
    }

    #[tokio::test]
    async fn without_tools_the_answer_cites_the_numbered_sources() {
        let prompt = "Question: what?\n\nSources:\n[1] src/a.rs:1-5\nbody\n[2] docs/b.md:2-9\n\
                      [3] c\n[4] d\n[5] e\n[6] f\n [7] indented, not a source line";
        let req = request(vec![user(prompt)], vec![], ToolChoice::Auto);
        let res = FakeLlm::new().complete(req).await.unwrap();
        assert_eq!(res.stop_reason, "end_turn");
        assert_eq!(
            text(&res),
            "Fake answer from 6 sources. [1] [2] [3] [4] [5]"
        );

        let two = request(vec![user("[1] a\n[2] b")], vec![], ToolChoice::Auto);
        let res = FakeLlm::new().complete(two).await.unwrap();
        assert_eq!(text(&res), "Fake answer from 2 sources. [1] [2]");

        let none = request(
            vec![user("no numbered lines here")],
            vec![],
            ToolChoice::Auto,
        );
        let res = FakeLlm::new().complete(none).await.unwrap();
        assert_eq!(text(&res), "Fake answer: no sources.");
    }

    #[tokio::test]
    async fn the_last_user_message_is_the_one_read() {
        let req = request(
            vec![
                user("[1] old"),
                Message {
                    role: Role::Assistant,
                    content: vec![Block::Text {
                        text: "[9] x".into(),
                    }],
                },
                user("[4] new"),
            ],
            vec![],
            ToolChoice::Auto,
        );
        let res = FakeLlm::new().complete(req).await.unwrap();
        assert_eq!(text(&res), "Fake answer from 1 sources. [4]");
    }

    #[tokio::test]
    async fn usage_is_characters_divided_by_four() {
        // 3 (system) + 16 (the user message) = 19 characters, 4 tokens.
        let mut req = request(vec![user("[1] 0123456789ab")], vec![], ToolChoice::Auto);
        req.system = "sys".into();
        let res = FakeLlm::new().complete(req).await.unwrap();
        assert_eq!(text(&res), "Fake answer from 1 sources. [1]");
        assert_eq!(res.usage.input_tokens, 4);
        assert_eq!(res.usage.output_tokens, 31 / 4);
        assert_eq!(res.usage.cache_read_input_tokens, 0);
    }
}
