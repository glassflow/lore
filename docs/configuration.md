# Configuration

lore reads these environment variables (all in `src/config.rs`).

| Variable | Default | Meaning |
|---|---|---|
| `LORE_DATABASE_URL` | `postgres://lore:lore@localhost:5434/lore` | Postgres connection string |
| `LORE_LISTEN_ADDR` | `0.0.0.0:8080` | Address the HTTP server binds |
| `LORE_DATA_DIR` | `./data` | Directory for repos and downloaded models |
| `LORE_ADMIN_TOKEN` | unset | Admin bearer token, for the admin API and the [admin page](usage.md#admin-page) at `/admin`. On the server, an empty value counts as unset and the admin API answers 403. The CLI reads it as the default of `--admin-token` |
| `LORE_LOG` | `info` | Log filter, in `tracing` `EnvFilter` syntax |
| `LORE_LOG_FORMAT` | `text` | `text` or `json`. Anything else is a startup error |
| `LORE_EMBEDDER` | `fastembed` | `fastembed` (bge-small-en-v1.5 on CPU) or `hash` (deterministic, for tests and `make run`). Anything else is a startup error. The first start with `fastembed` downloads about 130 MB into `$LORE_DATA_DIR/models` before the server listens |
| `LORE_EMBED_MODEL` | `bge-small-en-v1.5` | Embedding model. Only the default is supported in v1; any other value is a startup error |
| `LORE_EMBEDDING_CACHE` | `shared` | Who reuses a vector already computed for the same text: `shared` (every tenant, so pushing a repo another tenant has is nearly free, but a push is faster when its text exists elsewhere), `tenant` (each tenant only its own) or `off` (only the chunks a document already holds). Anything else is a startup error. See [the embedding cache](architecture.md#the-embedding-cache-and-tenant-isolation) |
| `LORE_MAX_BUNDLE_BYTES` | `536870912` (512 MiB) | Largest accepted body of `PUT /v1/repos/{id}/bundle`; a bigger upload is a 413. Must be a positive integer, otherwise a startup error |
| `LORE_LLM` | `anthropic` | Model provider: `anthropic`, `openai` (an OpenAI-compatible server such as llama-server, text only) or `fake` (deterministic, for tests and `make run`). Anything else is a startup error |
| `ANTHROPIC_API_KEY` | unset | Anthropic API key. Required when `LORE_LLM=anthropic`: `lore serve` exits at startup with an error naming the variable when it is missing |
| `ANTHROPIC_BASE_URL` | `https://api.anthropic.com` | Base URL of the Anthropic API |
| `LORE_FAST_MODEL` | `claude-opus-5-5` | Model for fast answers |
| `LORE_FAST_EFFORT` | `low` | Effort sent with fast answers. An empty value (`LORE_FAST_EFFORT=`) sends no effort. Claude Haiku 4.5 rejects `effort`, so the escape hatch `LORE_FAST_MODEL=claude-haiku-4-5` needs `LORE_FAST_EFFORT=` (empty) as well |
| `LORE_DEEP_MODEL` | `claude-opus-5-5` | Model for deep answers |
| `LORE_DEEP_EFFORT` | `medium` | Effort sent with deep answers. An empty value (`LORE_DEEP_EFFORT=`) sends no effort, as required for Claude Haiku 4.5 |
| `LORE_OPENAI_BASE_URL` | unset | Base URL of the OpenAI-compatible server, for example `http://localhost:8081/v1` (lore calls `<base>/chat/completions`). Required when `LORE_LLM=openai` |
| `LORE_OPENAI_MODEL` | unset | Model name sent to the OpenAI-compatible server, used for both answer modes with no effort. Required when `LORE_LLM=openai` |
| `LORE_OPENAI_API_KEY` | unset | Bearer token for the OpenAI-compatible server. Optional |
| `LORE_MCP_ALLOWED_HOSTS` | unset | Comma-separated `Host` values the MCP endpoint (`/mcp`) accepts, for example `lore.example.com,lore.lore.svc:8080`. Unset means any host is accepted, which is what you want behind Caddy or a cluster Service; the bearer key is the protection. When set, a request with another `Host` header gets a 403 |
| `LORE_SESSION_QUIET_SECS` | `60` | Seconds a session must go without a new span before its spans are turned into one searchable document. A session that keeps receiving spans is indexed again after each quiet period. `0` indexes on the next worker run |
| `LORE_DEEP_BUDGET_SECS` | `30` | Time budget of one deep-mode question, in seconds. Once more than 80% of it has passed, the model is told to answer with what it has. Must be a positive integer |
| `LORE_DEEP_MAX_ROUNDS` | `12` | Most tool rounds of one deep-mode question. After that the model is told to answer with what it has. Must be a positive integer |
| `LORE_DEEP_CONCURRENCY` | `2` | How many deep-mode questions one tenant may run at once. A further question waits up to 60 seconds for a free slot, then gets a 429. Must be a positive integer |
| `LORE_DEEP_DAILY_LIMIT` | `100` | How many deep-mode questions one tenant may ask per UTC day. The next one gets a 429 ("daily deep-mode limit reached"). The count is kept in memory, so a restart resets it. Must be a positive integer |
| `LORE_VERSION` | the crate version | Version string reported by the server |
| `LORE_URL` | `http://localhost:8080` | CLI only: base URL of the server, the default of the global `--url` flag |
| `LORE_KEY` | unset | CLI only: tenant API key, the default of the global `--key` flag |

