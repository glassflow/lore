# Benchmarks: lore vs Claude Code

lore finds the relevant code once, when you index, so a question skips the grep-and-read loop. These runs compare that with Claude Code exploring the repo on disk, using the same model on both sides (`claude-opus-5-5`).

## Highlights

- **fast** answers in 5 to 8 seconds, 3 to 7 times sooner than Claude Code.
- **deep** matched Claude Code's answers on every question we scored, and was faster in 4 of 5 runs.
- **One index covers every repo you push**, so an answer can connect a service to the client that calls it.
- **Claude Code with lore over MCP** answered the same questions 20% faster than Claude Code alone, and up to 29% faster on the broadest one.

## A 1,816-file service

Two questions about a private service, mostly Go and SQL. Each answer was scored against facts read from the code.

| Question | lore fast | lore deep | Claude Code |
|---|---|---|---|
| "What's included in our entry-level billing plan?" (13 facts) | **4.9 s**, 9 of 13 | **13.3 s**, 13 of 13 | 28.0 s, 13 of 13, $0.27 |
| "How does auth work in our control plane?" (10 facts) | **7.0 s**, 3 of 10 | 28.1 s, 10 of 10 | 22.3 s, 10 of 10, $0.32 |

fast gives a quick, cited summary built from the 12 passages search ranks highest. For the billing question it answered from test files that carry the plan's values. For the auth question it covered both credential kinds and the shared service token, and added how a second indexed repo, an MCP client, handles the control plane's 401. deep went on searching until it had the full picture: it found the plan's seed migration and every token type, with 4 and 12 tool calls.

Claude Code ran in a copy of the repo on disk, with effort `medium` for the billing question and `low` for the auth question. For the billing question that copy was a clone at the commit lore indexed. For the auth question it was a working copy on another branch, so Claude Code read `origin/main` through git instead: 21 commits newer than lore's index, with no change to the auth code.

## A 54-file service

Three questions about a private Python service (an MCP server). "Retrieval only" is lore's search on its own, with no model call.

| Question | Retrieval only | lore fast | lore deep | Claude Code |
|---|---|---|---|---|
| "How does auth work?" | 18 ms | **8.1 s** | 16.1 s | 52.5 s, $0.38 |
| List a feature's tools and what each does | 19 ms | **5.9 s** | 15.8 s | 38.7 s, $0.32 |
| Trace one code path end to end, with its test | 36 ms | **8.2 s** | 19.5 s | 36.9 s, $0.33 |

All three named all 5 tools for the second question. For the third, all three had all 5 facts checked: the HTTP method and path, the error message, the helper that marks the call as failed, the test name and the lifecycle constant.

Claude Code sent 164k to 211k input tokens per question, most of them read from its prompt cache.

## lore as an MCP server for Claude Code

Can lore make Claude Code itself faster? The same five questions, asked of Claude Code twice: once on its own, and once with lore connected over MCP.

| Question | Claude Code alone | Claude Code + lore | Faster by |
|---|---|---|---|
| "What's included in our entry-level billing plan?" (1,816 files) | 23.7 s, $0.14 | **21.0 s**, $0.17 | 11% |
| "How does auth work in our control plane?" (1,816 files) | 68.9 s, $0.57 | **48.7 s**, **$0.34** | 29% |
| "How does auth work?" (54 files) | 31.2 s, $0.17 | **26.3 s**, $0.23 | 16% |
| List a feature's tools and what each does (54 files) | 24.0 s, $0.16 | **21.7 s**, $0.20 | 10% |
| Trace one code path end to end, with its test (54 files) | 30.0 s, $0.19 | **24.8 s**, $0.19 | 17% |
| **All five** | 178 s, $1.23 | **143 s**, **$1.13** | **20%** |

- Both setups got every checked fact right, on every question (13, 10, 6, 5 and 5 facts).
- The gain is largest on broad questions in a big repo. For the control-plane question Claude made 13 tool calls instead of 21, and the run cost 40% less.
- On focused questions lore saved 10 to 17% of the time. The cost stayed about the same, because lore's search results add some input.
- Claude used lore as a map: one `search` to find the right files, then it read them.

### Tell Claude to start with lore

With lore connected but only mentioned in passing, Claude kept to its own tools and never called lore. Add a line like this to your `CLAUDE.md`:

> This repository is indexed in the lore MCP server. Start with lore (search, or ask for an overview) to find the relevant code, then read the files it points to. Use Grep only for what lore does not find.

The runs below used that line. Asked this way, Claude often chose `ask`, which runs lore's own model and is slower than reading the code. lore's tools have since been reworded for agents: `search` shows short excerpts, and the descriptions point agents to `search` when they can read the code. The line in [Use lore from agents](usage.md#use-lore-from-agents) names `search` directly.

### Setup

- Claude Code 2.1.304 with `claude-opus-5-5` at effort `medium`, run as `claude -p` from a clone at the commit lore had indexed. Each question ended with "Answer from this repository's code and cite file:line."
- Both setups had only `Read`, `Grep` and `Glob` (`--tools`) and no other MCP servers (`--strict-mcp-config`).
- The lore setup added lore's MCP server (`--mcp-config`) and passed the `CLAUDE.md` line above with `--append-system-prompt`.
- The two setups ran side by side, one question at a time. Each cell is one run, taken on 2026-10-22.

## Indexing

| Repo | Files | Chunks | Time | Storage |
|---|---|---|---|---|
| 54-file Python service | 54 | 430 | 23 s | about 1.5 MB |
| 1,816-file Go service | 1,801 indexed | 22,796 | 35 min | 140 MB in Postgres, plus a 35 MB copy of the repo |

Indexing ran on an Apple M2 Pro (10 cores, 16 GB). Nearly all of the time goes to the embedding model on the CPU; lore used up to 7 cores and 3.7 GB of memory on the large repo. After the first push, lore embeds only the files that changed. On a large first push `lore repo push` stops waiting after 15 minutes, and the server finishes the push on its own; `lore repo list` shows the new commit when it is done.

## How these were measured

- lore and Postgres ran on the same laptop. lore used its defaults: effort `low` for fast, `medium` for deep, 12 passages for a fast answer.
- Claude Code 2.1.304 ran as `claude -p` from the repo's directory, reading the files on disk. It had its default tools, including Bash, Read and Grep. For the 54-file service it ran with no MCP servers (`--strict-mcp-config`), so it could not use lore.
- Times are wall clock as each tool reports them: `latency_ms` from `/v1/ask`, `duration_ms` from Claude Code. Retrieval only is the median of 5 `GET /v1/search` calls.
- Each cell is one run, taken on 2026-10-22.
- Claude Code's cost is the `total_cost_usd` it reports. lore does not report its cost yet: `/v1/ask` returns only uncached input tokens, and a fast answer sends at most 60,000 characters of passages in one call.

## Run it on your repo

Put one question per line in a file and run:

```bash
export LORE_URL=http://localhost:8080 LORE_KEY=lk_...
scripts/bench_vs_claude_code.sh ~/code/app questions.txt bench-out
```

The script pushes the repo (`BENCH_PUSH=0` skips that), times 5 searches per question, then runs a fast answer, a deep answer and Claude Code for each question. It saves every response in the output directory and prints a table like the ones above.

It needs `curl`, `jq`, `claude` and `lore` on the path, and a key with the `read` scope (plus `ingest` for the push) of a tenant that holds only this repo. `BENCH_MODEL` sets Claude Code's model (default `claude-opus-5-5`); run the lore server with the same `LORE_FAST_MODEL` and `LORE_DEEP_MODEL`.
