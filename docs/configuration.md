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
| `LORE_EMBEDDER` | `fastembed` | `fastembed` (bge-small-en-v1.5 on CPU) or `hash` (deterministic, for tests and `make run`). Anything else is a startup error. The first start with `fastembed` downloads about 130 MB into `$LORE_DATA_DIR/models` before the server listens; see [the embedding model download](#the-embedding-model-download) |
| `LORE_EMBED_MODEL` | `bge-small-en-v1.5` | Embedding model. Only the default is supported in v1; any other value is a startup error |
| `LORE_EMBEDDING_CACHE` | `shared` | Who reuses a vector already computed for the same text: `shared` (every tenant, so pushing a repo another tenant has is nearly free, but a push is faster when its text exists elsewhere), `tenant` (each tenant only its own) or `off` (only the chunks a document already holds). Anything else is a startup error. See [the embedding cache](architecture.md#the-embedding-cache-and-tenant-isolation) |
| `LORE_MODEL_DOWNLOAD_TIMEOUT_SECS` | `300` | Seconds the first-start download of the embedding model may take, retries included. When they run out, `lore serve` exits with an error naming the URL and the model directory. Must be a positive integer |
| `HF_ENDPOINT` | `https://huggingface.co` | Where the embedding model is downloaded from, for example a Hugging Face mirror |
| `HF_HOME` | unset | When set, the embedding model is kept in this directory instead of `$LORE_DATA_DIR/models` |
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
| `LORE_SYNC_INTERVAL_SECS` | unset | Seconds between two [syncs](usage.md#sync-a-repo-from-its-remote) of every repo that has a `remote_url`. Unset or `0` turns the poller off. Must be a non-negative integer |
| `LORE_GITHUB_WEBHOOK_SECRET` | unset | Secret of the GitHub webhook at `POST /v1/webhooks/github`. Unset or empty turns the route off (404) |
| `LORE_GIT_TOKEN_<TENANT>` | unset | Token for the private remotes of one tenant, for example `LORE_GIT_TOKEN_ACME_WEB` for tenant `acme-web`. Sent as HTTP basic auth to that tenant's `https://` remotes only |
| `LORE_SYNC_ALLOW_FILE_REMOTES` | `false` | Also accept `file://` remote URLs. For tests and local experiments only: with it on, any `ingest` key can make the server index a git repo on its own disk |
| `LORE_VERSION` | the release tag the binary was built for, else the crate version | Version string reported by the server |
| `LORE_URL` | `http://localhost:8080` | CLI only: base URL of the server, the default of the global `--url` flag |
| `LORE_KEY` | unset | CLI only: tenant API key, the default of the global `--key` flag |

## The embedding model download

With `LORE_EMBEDDER=fastembed`, the first start downloads bge-small-en-v1.5 (about 130 MB, five files of the `Xenova/bge-small-en-v1.5` repo) from `$HF_ENDPOINT` into `$LORE_DATA_DIR/models` before the server listens. Later starts find the files there and do not use the network.

- The log shows where the download goes when it starts, then a line for every 10 MB of the model file.
- A connection that sends nothing for 30 seconds is dropped. Each file is tried up to 5 times, with waits of 1, 2, 4 and 8 seconds in between, and a retry continues a partly downloaded file where it stopped.
- The whole download, retries included, must finish within `LORE_MODEL_DOWNLOAD_TIMEOUT_SECS` (5 minutes by default). Otherwise `lore serve` exits with an error that names the URL it was downloading and the model directory.

The model directory uses the Hugging Face cache layout (`models--Xenova--bge-small-en-v1.5/` with `refs/main` and `snapshots/<commit>/`). To start a machine that cannot reach Hugging Face, or to skip the download, copy that directory from another install before the first start:

```sh
# from a machine where lore has started once
tar -C "$LORE_DATA_DIR/models" -czf lore-models.tgz models--Xenova--bge-small-en-v1.5
# on the new machine
mkdir -p "$LORE_DATA_DIR/models"
tar -C "$LORE_DATA_DIR/models" -xzf lore-models.tgz
```

`tar` keeps the symlinks of `snapshots/` that point into `blobs/`. For the Docker image, unpack it into the data volume, which is mounted at `/var/lib/lore`, so the directory is `/var/lib/lore/models`. The files must be readable by the `lore` user (uid 10001).
