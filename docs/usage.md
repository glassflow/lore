# Using lore

Tenants and keys, indexing, search, traces, documents, answers, the web pages and MCP. Most endpoints are also reachable from the `lore` CLI.

## Tenants and keys

lore is multi-tenant. Each tenant has API keys with the scopes `ingest` and `read`. Keys look like `lk_` followed by 40 hex characters, and only their SHA-256 hash is stored, so a key is shown once, when it is created. An admin token (`LORE_ADMIN_TOKEN` on the server) creates and deletes tenants and keys, and deletes sources. Without it the admin API answers 403.

The CLI talks to the server over HTTP. `--url`, `--key` and `--admin-token` are global flags (env `LORE_URL`, `LORE_KEY`, `LORE_ADMIN_TOKEN`), and the `admin` and `me` subcommands print the server's JSON.

```bash
export LORE_ADMIN_TOKEN=dev-admin               # with `make run` going in another shell
lore admin tenant create --name demo            # {"id": "...", "name": "demo"}
lore admin tenant list
lore admin key create --tenant <id>             # {"id": "...", "key": "lk_...", ...}; add --scopes read to limit it
lore admin key list --tenant <id>               # prefixes only, never the key or its hash
lore admin key revoke <key-id>                  # the key stops working at once
lore admin source delete <source-id>            # {"id": "...", "deleted": true}
lore admin tenant delete <tenant-id>            # {"id": "...", "deleted": true}
lore --key lk_... me                            # {"tenant_id": "...", "tenant_name": "demo", "scopes": [...]}
```

Tenant names are 1 to 64 characters of `a-z`, `0-9` and `-`. Send keys as `Authorization: Bearer lk_...`.

### Deleting tenants and sources

`DELETE /v1/admin/tenants/{id}` deletes a tenant with everything it holds: its sources, documents, chunks, trace spans, queued sessions and keys, in one transaction. `DELETE /v1/admin/sources/{id}` deletes one source with its documents and chunks. Both answer 204, or 404 when the id is unknown, and then remove the repo mirror and checkout of each deleted repo source (`$LORE_DATA_DIR/repos/<source id>`). While a push to the source, or to any source of the tenant, has a live index run (not ended, updated in the last 90 seconds), the delete answers 409 and changes nothing; try again when the push ends. The run may also be one that stopped less than 90 seconds ago (its process was killed), which counts as live until then. Deleting the `traces` source (the session transcripts) does not delete the tenant's spans, so a session that gets new spans is indexed again.

Postgres reuses the space of the deleted rows after `VACUUM` (autovacuum runs it on its own), but it does not give it back to the file system, so the database size on the admin page does not shrink. `VACUUM FULL` rewrites the tables to shrink them, and locks them while it runs.

## Repos

A repo source is pushed as a git bundle. The server keeps a bare mirror and a checkout under `$LORE_DATA_DIR/repos/<source id>/`, and needs `git` on its path (the Docker image installs it). Both routes need an `ingest` key; listing needs `read`.

```bash
curl -X POST localhost:8080/v1/repos -H "Authorization: Bearer $LORE_KEY" \
  -H 'content-type: application/json' -d '{"name": "sample"}'      # {"id": "...", "name": "sample", ...}
curl localhost:8080/v1/repos -H "Authorization: Bearer $LORE_KEY"
git bundle create /tmp/repo.bundle HEAD
curl -X PUT "localhost:8080/v1/repos/<id>/bundle?ref=$(git rev-parse HEAD)&full=true" \
  -H "Authorization: Bearer $LORE_KEY" --data-binary @/tmp/repo.bundle
```

`ref` is the full 40 character lowercase sha of the bundle's `HEAD`. With `full=true` the bundle holds the whole history and the tree is re-indexed, deleting documents of files that are gone. With `full=false` the bundle may be incremental (`git bundle create out.bundle <last-sha>..HEAD`) and only changed files are indexed. If the server no longer has the last indexed commit, or the bundle needs commits it lacks, the answer is 409 (`need_full_bundle`) and the client sends a full bundle. The answer is a report with `indexed_commit`, `files_indexed`, `files_unchanged`, `files_skipped`, `files_deleted` and `chunks`.

`DELETE /v1/repos/{id}` needs an `ingest` key and deletes one of the tenant's repos with everything the admin delete of a source removes: the source, its documents and chunks, its mirror and its checkout on the server. The answer is 204. An unknown id, an id of another tenant and an id that is not a repo are all a 404. While a run of the repo is going (a push, a sync, or a `lore index` Job whose run was written in the last 90 seconds) the answer is 409, and nothing is deleted; try again when the run ends. `curl -X DELETE localhost:8080/v1/repos/<id> -H "Authorization: Bearer $LORE_KEY"`.

`GET /v1/repos/{id}/progress` needs a `read` key and answers with the latest index run of that repo, whichever process ran it (while a run is queued behind another, the queued run is the latest, so the one indexing is not shown until the queued one starts): `{"id", "stage", "commit", "full", "files_done", "files_total", "chunks", "chunks_embedded", "embed_ms", "started_at", "updated_at", "finished_at", "error", "stale"}`. `stage` is `queued` (waiting for another run on the same repo), `cloning` (fetching the commit), `indexing`, `done` or `failed`, and `error` says why a run failed. `files_total` is null until the files to index are known. A run that has not ended and has not been updated for 90 seconds is shown with `"stale": true`: its process most likely died. The stage is left as it was, and the run stays that way until the next run starts. A repo that has no run yet answers 404 `no_index_run`; a repo of another tenant answers the usual 404.

Progress is kept in the Postgres table `index_runs`, one row per run (source, tenant, stage, files done and total, chunks, start and finish times, error), so it survives a restart and a process other than the server can report through it. An indexer writes its counts at most once a second, writes a change of stage or the end of a run at once, and refreshes `updated_at` every 5 seconds while it is alive. If a write fails, the failure is logged and the index carries on. The rows go with their source or tenant when it is deleted.

Repo indexing publishes atomically. While a push runs, search sees the previous complete index
(or no repo chunks on its first push). Changed, new and deleted files and `indexed_commit`
become visible together after success. A failed or cancelled push leaves the previous index
visible. Unchanged files keep their existing chunks and embeddings.

Each search uses one database snapshot for lexical ranking, vector ranking and returned text,
even if a push finishes between those steps. Stored document reads likewise keep metadata and
chunks on one snapshot. Separate requests can observe different completed pushes. This does
not pin deep-mode or MCP `grep` and `read_file`: those tools read the live checkout, which may
already contain the incoming commit while indexing is still running or after a failed push.

The atomic write uses one database transaction and connection for the duration of indexing,
including embedding. It does not buffer repo text or copy unchanged chunks. With the embedding
cache enabled, it stages each fresh vector and its exact cache key until commit: a 384-float
vector takes 1,536 bytes of payload, plus keys, allocation and publication overhead. This memory
grows with newly embedded chunks, and concurrent cold pushes add their staging requirements.
Only after commit releases the connection are cache entries published in sorted, short inserts;
a cache error is logged but does not fail an already-published index. Failed or cancelled pushes
discard their staging. Standalone documents use the same best-effort post-commit cache policy.
A large update also retains old row versions until publication and until older readers finish.
Concurrent pushes to different sources each use a connection; when the pool is full, other
requests wait for one. Pushes to the same source remain serialized.

Symlinks, submodules, binary or non-UTF-8 files, files over 256 KiB, and lock files, build output and vendored directories are skipped and counted in `files_skipped`.

## Index a repo

`lore repo push` indexes the committed HEAD of a local git repo. It needs `git` on the path and an `ingest` key.

```bash
export LORE_URL=http://localhost:8080 LORE_KEY=lk_...
lore repo push . --name sample          # indexed sample at 1a2b3c4: 6 indexed, 0 unchanged, 0 deleted, 1 skipped, 31 chunks
lore repo push ~/code/app --json        # the report as one line of JSON
lore repo list                          # name, short indexed commit and web URL template, one repo per line
```

`PATH` defaults to `.` and can be any directory inside the repo. `--name` defaults to the name of the repo's top level directory.
The first push sends the whole history. Later pushes send only the commits the server lacks, and when the server already has HEAD nothing is sent and the report is all zeros.
If the server lost its copy, or the history was rewritten (for example after a force push), the CLI falls back to a full bundle by itself. `--full` forces one, and also chunks again every file that an older version of lore chunked differently, which a push otherwise skips as unchanged. Run it once after upgrading lore.
Uncommitted changes are not indexed, and the CLI says so on stderr.
`--web-url <template>` stores a URL template for links to files, for example `https://github.com/acme/app/blob/{commit}/{path}`. `repo list --json` prints the server's JSON on one line.

To index on every push to main, run it from CI. A GitHub Actions step (the checkout needs the full history on every push, so keep `fetch-depth: 0`: a shallow clone that lacks the commits the server has makes the push fail):

```yaml
on:
  push:
    branches: [main]
jobs:
  lore:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - name: Index the repo in lore
        run: lore --url $LORE_URL --key ${{ secrets.LORE_KEY }} repo push . --name <repo>
        env:
          LORE_URL: ${{ vars.LORE_URL }}
```

The `lore` binary has to be on the runner (build it with `cargo install --path .` or copy a release binary).

## Sync a repo from its remote

Instead of pushing from CI, the server can fetch a repo itself. Give the repo a `remote_url` when you create it (creating it again changes it, `""` removes it, leaving it out keeps it):

```bash
curl -X POST localhost:8080/v1/repos -H "Authorization: Bearer $LORE_KEY" \
  -H 'content-type: application/json' \
  -d '{"name": "app", "remote_url": "https://github.com/acme/app.git"}'
curl -X POST localhost:8080/v1/repos/<id>/sync -H "Authorization: Bearer $LORE_KEY"
```

`POST /v1/repos/{id}/sync` needs an `ingest` key and git 2.36 or newer on the server. It fetches the remote's default branch (its `HEAD`) into the repo's mirror and indexes the files that changed since the last indexed commit, exactly like a push, under the same per-repo lock. If history was rewritten or the mirror lost the last indexed commit, it fetches all objects again (`git fetch --refetch`) and indexes the whole tree. The answer is the push report plus `up_to_date` (the remote's head was already indexed, the counts are zeros), `full` (the whole tree was indexed, including the first sync) and `adopted` (the repo was already indexed, for example by [`lore index`](#index-a-repo-from-a-job), but this server held no mirror or no checkout for it, so the sync built them; a sync that ends as a full index is a reindex and reports `adopted: false`). A repo without a remote is a 409; a remote that cannot be fetched (unreachable, missing, or a fetch over 10 minutes) is a 502 `could not fetch the remote`, and git's message goes to the server log and the [admin page](#admin-page). A remote that refuses the credentials, or wants some and gets none, is a 502 that says `the remote refused authentication`. Pushes and syncs can be mixed on one repo.

`remote_url` must be an `https://` URL without a user or password. For a private remote, the operator sets `LORE_GIT_TOKEN_<TENANT>` on the server (the tenant name upper-cased, `-` written `_`: tenant `acme-web` reads `LORE_GIT_TOKEN_ACME_WEB`). It is sent as HTTP basic auth (`x-access-token:<token>`), which GitHub, GitLab and Gitea accept for a personal access token, to every remote of that tenant and no other. Since a tenant's keys choose the URL, give each tenant a token that reads only its own repos.

A token can also be passed per call, which is how a private repo is synced without a token on the server:

```bash
curl -X POST localhost:8080/v1/repos/<id>/sync -H "Authorization: Bearer $LORE_KEY" \
  -H 'content-type: application/json' -d "{\"token\": \"$GITHUB_TOKEN\"}"
```

The body is optional (an empty body syncs as before); anything else must be `{"token": "..."}`, otherwise it is a 400. The token is used for that fetch only and wins over `LORE_GIT_TOKEN_<TENANT>` for that call. It is never stored, and it is scrubbed (also in its basic-auth form) from what git says before that text reaches the server log, the admin page's `last_sync_error` or the database. While the fetch runs, the token is in the environment of the git child process, so anyone who can read that process's `/proc/<pid>/environ` (the same user, or root, on the host) can see it. The poller and the webhook never have a per-call token: they use `LORE_GIT_TOKEN_<TENANT>` or, without it, fetch anonymously.

Two ways to sync without anyone calling the route:

- **Poller.** With `LORE_SYNC_INTERVAL_SECS` set, the server checks every repo that has a remote at start and then every interval. Repos sync independently, so an unreachable remote cannot hold up the others. Each repo has at most one running poll; a repo already pushing or syncing skips that tick instead of queuing duplicate work. A default-branch push is picked up on the next available tick, plus fetch and indexing time.
- **GitHub webhook.** Set `LORE_GITHUB_WEBHOOK_SECRET`, then in the GitHub repository's settings add a webhook with payload URL `<LORE_URL>/v1/webhooks/github`, content type `application/json`, the same secret, and the `push` event. A push to the default branch syncs every repo, of any tenant, whose `remote_url` is that repository (`clone_url` or `html_url`, compared without case, a trailing `/` or `.git`); the answer is 202 `{"queued": n}` and the sync runs in the background, so the change is searchable seconds later. The route needs no API key: a delivery whose `X-Hub-Signature-256` is not the HMAC-SHA256 of the body under the secret is a 401, and a valid one can only start fetches from remotes tenants configured. Pushes to other branches, deleted branches and other events are accepted and ignored, `ping` answers 200. Without the secret the route is a 404.

## Index a repo from a job

`lore index` fetches one repo and indexes it straight into Postgres, then exits. It needs no running server, which makes it fit for a Kubernetes Job or a cron run. It indexes the whole tree of the remote's default branch (`HEAD`) exactly as `lore repo push --full` would for that commit: the same chunks, document hashes, search results, embedding cache behaviour and `indexed_commit`, and the same checkout on its data dir for deep answers. It is not incremental: it reads the whole tree every time and only embeds text it has no vector for.

```bash
export LORE_DATABASE_URL=postgres://lore:...@db/lore
export LORE_INDEX_TOKEN=...        # only for a private remote
lore index --tenant <tenant id> --repo <source id> --remote https://github.com/acme/app.git
# {"commit":"3f2a...","files_indexed":212,"files_unchanged":0,"files_skipped":4,"files_deleted":0,"chunks":1480,"chunks_embedded":1480,"embed_ms":61234,"full":true}
```

| Flag | Meaning |
|---|---|
| `--tenant` | The tenant id (uuid) that owns the repo |
| `--repo` | The repo source id (uuid), made earlier with `POST /v1/repos` |
| `--remote` | The `https://` remote, without a user or password (the same rule as `remote_url`) |

| Variable | Meaning |
|---|---|
| `LORE_DATABASE_URL` | The Postgres DSN (required). Migrations are applied first, as `lore serve` does (see below) |
| `LORE_INDEX_TOKEN` | A git token for a private remote, sent as basic auth (`x-access-token:<token>`). Only ever read from the environment: there is no flag for it, it is not put in the URL or git's arguments, and it is scrubbed from every message the command prints, logs or records. Git receives it as a basic-auth header in its environment, so while the fetch runs it is readable from that git process's `/proc/<pid>/environ` by the same user (or root); `LORE_INDEX_TOKEN` and `LORE_DATABASE_URL` themselves are removed from git's environment |
| `LORE_EMBEDDER`, `LORE_EMBEDDING_CACHE`, `LORE_DATA_DIR`, ... | Read as the server reads them, see [the configuration](configuration.md). `lore index` makes no LLM or other model calls except the local embedder |
| `LORE_EMBED_THREADS` | ONNX threads of the embedder. Unset, it follows the container CPU limit (cgroup v2 `cpu.max`, else cgroup v1 CFS quota and period), rounded up and at least 1, and without a limit the available cores |

`lore index` runs the database migrations when it starts, as `lore serve` does. A Job image newer than the running server therefore migrates the schema under that server; run matching versions of the two.

A Job index writes its mirror and checkout under its own `LORE_DATA_DIR`, so it does not feed the server's deep mode (the checkout) itself. Pointing the Job at the server's volume is not safe: the mirror and checkout updates are only guarded in-process (TR-1196 covers this).

Instead the server adopts what the Job indexed. Give the repo a `remote_url` and call `POST /v1/repos/{id}/sync` (or let the poller or a webhook do it). The server finds the repo already indexed but has no mirror or checkout for it, so it fetches the remote into its own data dir and builds the checkout, and it embeds and indexes nothing: the answer is `{"up_to_date": true, "adopted": true, "files_indexed": 0, "chunks": 0, ...}`, and the chunks, vectors and `indexed_commit` stay exactly as the Job wrote them. If the remote moved on after the Job, the sync indexes only the files that changed since the Job's commit, like any sync, and reports `up_to_date: false, full: false, adopted: true`. From then on `grep` and `read_file` in deep answers work on that repo, and later syncs report `adopted: false`. If a Job indexes the repo again later, the next sync finds the server's checkout behind the index (the mirror's `refs/lore/checkout` ref names the commit the checkout is at, and it is written only after a rebuild succeeds), rebuilds the checkout without embedding anything, and reports `up_to_date: true, adopted: true`. A retry after a failed attempt can report `adopted: false` once the first attempt built the mirror and checkout. The server logs one line per adoption (source id, commit, files indexed).

The repo must belong to the tenant: a source of another tenant, a source that does not exist and a tenant that does not exist are all the same error and nothing is written. Only that tenant's rows are touched.

The run shows in `index_runs` like any index run, so `GET /v1/repos/{id}/progress` and the admin page show its stage and counts while it works and `done` or `failed` after. A process that is killed leaves a run that goes stale, as for the server.

One `lore index` per repo at a time: it holds a Postgres advisory lock on the repo for the whole run, on a connection of its own. A second one for the same repo does not wait; it exits at once with code 3 (`another index of source <id> is running`). The lock is released when the process ends, however it ends. Other repos are not affected, and the server's own pushes and syncs of that repo still serialise with it on the row lock the index takes while it writes. A server incremental push or sync that waited on that row lock behind an index that moved the indexed commit is answered with the full-index path (409 `need_full_bundle` for a push, a full index for a sync) instead of applying a stale diff. The lock connection is pinged every 30 seconds so an idle-connection timeout of a NAT or load balancer does not drop it; point `LORE_DATABASE_URL` at Postgres or a session-mode pooler, not a transaction-mode one (PgBouncer `pool_mode=transaction`), which cannot hold a session lock.

| Exit code | Meaning |
|---|---|
| 0 | Done. One JSON line on stdout |
| 1 | Failed (bad remote, unknown repo or tenant, fetch or index error). The error is on stderr and the run row is marked `failed` with the scrubbed error. Nothing on stdout |
| 3 | Another `lore index` of the same repo holds the lock |

Logs go to stderr, so stdout is always either one JSON line or empty.

`lore repo push` keeps working for local repos and CI.

## Search

`GET /v1/search` searches the chunks of the key's tenant. It needs a `read` key, and another tenant's documents are never visible: a source name from another tenant just matches nothing.

```bash
curl -G localhost:8080/v1/search -H "Authorization: Bearer $LORE_KEY" \
  --data-urlencode 'q=plan limits' --data-urlencode 'k=5' --data-urlencode 'kind=code,doc'
lore search "plan limits"                       # the same, from the CLI
lore search "pricing plans" --kind doc --k 3
lore search "plan limits" --source sample --json
```

| Parameter | Meaning |
|---|---|
| `q` | The query. Required: a missing or blank `q` is a 400. At most 2000 characters, otherwise a 400. It cannot contain a NUL byte |
| `k` | How many hits, default 10. Any integer is accepted and clamped to 1 to 50. Something that is not an integer is a 400 |
| `kind` | Comma-separated document kinds: `code`, `doc`, `session`, `note`. Another name is a 400 that lists the kinds. Empty means all |
| `source` | Only the source with this name, whatever its kind (`repo`, `traces`, `documents`). Empty means all. A name that matches nothing gives no hits |

The answer is `{"hits": [...]}`, best first. A hit has `chunk_id`, `document_id`, `source`, `kind`, `title`, `uri`, `path`, `start_line`, `end_line` (1-based, inclusive), `symbols`, `snippet` (the first 40 lines of the chunk) and `score`. `path` is set for repo files only. `uri` is the source's `web_url_template` filled in for repo files (see below), or else the document's own URI, or null.

The `web_url_template` of a repo source can use `{path}`, `{commit}`, `{start}` and `{end}`, for example `https://github.com/acme/app/blob/{commit}/{path}#L{start}-L{end}`. `{path}` is percent-encoded segment by segment (the `/` between segments stays), so a space, `#`, `?` or `%` in a file name cannot change the link. Other `{...}` text is left as it is.

How it ranks: the query is run twice, once against the Postgres full-text index (any query word may match, ranked with `ts_rank_cd`) and once against the pgvector embeddings (cosine distance). Each run keeps its best 50 chunks. A third list holds the first 3 of the vector hits that define a list of values (enums, constant lists, SQL seed rows, `CHECK ... IN` lists), so "which X are there" finds the definition of X rather than only the code that uses it. The lists are merged with Reciprocal Rank Fusion: a chunk scores the sum of `1 / (60 + rank)` over the lists it is in, so the score is at most about 0.049 and is only good for comparing hits of one search. Ties go to the better full-text rank, then to the lower chunk id. A query with only stop words (`the and of`) has no full-text part and is answered from the vectors alone. Identifiers such as `MaxSeatsPerPlan` are also indexed as the words `max seats per plan`, so a search for "seats" finds them.

The CLI prints, for each hit, `path:start-end  (score 0.032)  symbols` (the title when there is no path), then the first 3 snippet lines indented by 4 spaces. `--json` prints the server's JSON on one line. No hits prints `no results` on stderr and exits 0.

## Traces

`POST /v1/traces` takes OpenTelemetry traces over OTLP/HTTP, so an agent or a collector can export its sessions to lore. It needs an `ingest` key. Send `Content-Type: application/json` or `application/x-protobuf`, optionally with `Content-Encoding: gzip`. Anything else is a 415. A bad body is a 400. A body over 16 MiB, one that unzips to more than 64 MiB, or one whose spans would store more than 64 MiB of row data (the resource attributes count once per span) is a 413. A span whose trace id is not 16 bytes, whose span id is not 8 bytes, or whose parent span id is not empty or 8 bytes is not stored; the answer is still a 200 and carries `partialSuccess` with `rejectedSpans` and an `errorMessage`. A session id over 512 bytes is shortened to its first part plus a hash, so two long ids stay distinct. With nothing rejected the answer is an empty `ExportTraceServiceResponse` in the format of the request.

Spans are stored per tenant, keyed by trace id and span id, so sending a span again updates it. A span's session is its `session.id` attribute, else `gen_ai.conversation.id`, else the resource's `session.id`, else its trace id. Each session is queued for indexing. Strings longer than 64 KiB are cut and end with `…[truncated]`.

```bash
lore otlp send tests/fixtures/otlp/claude_code_session.json          # JSON, needs --url and --key
lore otlp send tests/fixtures/otlp/otel_genai_session.json --protobuf --gzip
```

`lore otlp send <FILE.json>` posts an OTLP JSON file to `/v1/traces` with the global `--url` and `--key`. `--protobuf` converts it to protobuf first, and `--gzip` compresses the body. It prints `HTTP <status>` and exits non-zero when the server does not answer 2xx.

## Documents

`POST /v1/documents` stores notes and documents that are not in a repo or a trace, and indexes them so search and ask find them. It needs an `ingest` key.

```bash
curl -X POST "$LORE_URL/v1/documents" \
  -H "Authorization: Bearer $LORE_KEY" -H "Content-Type: application/json" \
  -d '{"source": "notes", "documents": [
        {"external_id": "n1", "title": "Review", "text": "The quarterly zorblax review moved to Thursdays."}
      ]}'
# {"upserted":1,"unchanged":0,"chunks":1}
```

`source` names a group of documents (1 to 100 characters of `A-Z a-z 0-9 . _ -`, but not `.` or `..`; it is created on first use). `documents` has 1 to 100 entries. Each has an `external_id` (1 to 512 bytes, unique within the source, with no `.` or `..` between slashes, so `a.b` and `a/.b` are fine but `..` and `a/../b` are not), a `title` of at most 1 KiB, a `text` of at most 1 MiB, and optionally `kind` (`note`, the default, or `doc`), `uri` (shown with search hits, at most 2,048 bytes) and `metadata` (a JSON object of at most 64 KiB once serialized). A text that makes more than 1000 chunks is refused too. Anything invalid is a 400 and nothing of that request is written. The body may be up to 128 MiB; more is a 413.

Putting an `external_id` again replaces the document. When its kind, title, text, uri and metadata are all the same as stored, it is skipped and counted in `unchanged`, so repeating a sync is cheap. If one id appears twice in a request, the documents are handled in order and the last one wins. The text is split into sections at markdown headings, and long sections into overlapping windows of lines. All of the text is searchable: a window is kept within 8,000 characters, and a single line longer than that is split into several chunks (at a space when there is one close to the end) instead of being cut.

```bash
lore doc put --source notes --id n1 --title "Review" --text "The quarterly zorblax review moved to Thursdays."
lore doc put --source wiki --id guide --title "Guide" --file guide.md --kind doc --uri https://example.com/guide
lore doc get <DOCUMENT_ID>                      # prints the text
lore doc rm --source notes --id n1
```

`GET /v1/documents/{id}` (a `read` key) returns `{"id", "kind", "title", "uri", "metadata", "text"}` for any document of the tenant, using the `document_id` of a search hit. For a document put through the API the text is exactly what was sent (line endings, blank lines and the final newline included), cut at 100,000 characters; for repo files and sessions it is rebuilt from the indexed chunks. `DELETE /v1/documents/{source}/{external_id}` (an `ingest` key) answers 204, or 404 when the tenant has no such document; an `external_id` may contain `/`, and other special characters must be percent-encoded. `lore doc put` accepts either `--file` or `--text`, and prints the server's JSON.

## Ask

`POST /v1/ask` answers a question from the key's tenant, with numbered citations. It needs a `read` key. Fast mode runs one hybrid search (the same as `/v1/search`), puts the best passages in a prompt as numbered sources, and makes one model call.

```bash
curl localhost:8080/v1/ask -H "Authorization: Bearer $LORE_KEY" -H 'content-type: application/json' \
  -d '{"question": "how many plans are there and what are their limits?"}'
lore ask "how many plans are there and what are their limits?"     # the same, from the CLI
lore ask "who owns billing?" --kind doc,note --k 8 --source sample
lore ask "how many plans are there?" --json                        # the server's JSON on one line
lore ask "how many plans are there?" --mode sources                # the sources as bullets, no model call
```

| Field | Meaning |
|---|---|
| `question` | Required. A missing or blank question is a 400. At most 2000 characters, otherwise a 400 |
| `mode` | `fast` (the default), `sources` or `deep`. `sources` calls no model (see below). `deep` runs an agent loop (see below) and is a 400 ("deep mode needs a tool-capable model provider (LORE_LLM=anthropic)") with `LORE_LLM=openai` |
| `k` | Fast and sources mode: how many sources to retrieve, default 12, clamped to 1 to 50 |
| `kind` | Fast and sources mode: a list of document kinds: `code`, `doc`, `session`, `note`. Another name is a 400 |
| `source` | Fast and sources mode: only the source with this name |

The answer is `{"answer", "mode", "model", "citations", "sources_considered", "tool_calls", "usage", "latency_ms"}`. `citations` lists the sources the answer cites with `[n]`, once each, in the order they first appear. A citation has `label` (the source number, as text), `document_id`, `kind`, `title`, `path`, `start_line`, `end_line` and `url` (the same link `/v1/search` gives as `uri`). `sources_considered` is the number of passages that went into the prompt (at most 60,000 characters of them, each cut to its first 120 lines), `tool_calls` is 0 in fast mode, and `usage` has `input_tokens`, `output_tokens`, `cache_read_input_tokens` and `cache_creation_input_tokens`. `input_tokens` is only the part of the prompt the model's prompt cache did not cover: the whole prompt is the sum of the three input counts.

When nothing indexed matches, the answer is "I found nothing indexed that matches this question.", `model` is `"none"` and the model is not called. If the model declines to answer, the response is a 422 `{"error": "the model declined to answer"}`.

Sources mode (`"mode": "sources"`) runs the same search as fast mode and calls no model, so it answers in the time of a search and costs nothing, and it works without a model provider. The answer is one bullet per source, best first: ``- [1] `src/billing/plans.rs:3-14` (sample): Plan, Plan::limits — `pub enum Plan { Free, Pro, Team }` ``, that is the label, the path and lines (the title for documents without a path), the source, up to 6 symbols, and the line that shares the most words with the question. Every source is cited, labelled `1`, `2` and so on in rank order. `model` is `"none"` and `usage` is zero. The CLI prints each bullet with its link on the next line, and ends with `no model · <latency> ms`.

Deep mode (`"mode": "deep"`) lets the model drive the `search`, `grep`, `read_file` and `get_session` tools for up to `LORE_DEEP_MAX_ROUNDS` rounds within `LORE_DEEP_BUDGET_SECS` seconds. Tool calls of one round run at the same time, each for at most 10 seconds; a slower one gives the model an error result. Past 80% of the budget, or at the round cap, the model gets one last call with the tools turned off and is told to answer. Cost is capped per tenant by `LORE_DEEP_CONCURRENCY` and `LORE_DEEP_DAILY_LIMIT` (both give a 429). A deep answer cites code as `[path:start-end]`, with the path as the tools print it minus the source name, and sessions as `[session:<id>]`. Each distinct citation appears once in `citations` (at most 50), in order of first appearance. The `label` is the text between the brackets. A citation whose path or session lives in the tenant's index carries `document_id`, `kind`, `title` and `url`; one that does not match keeps its `label`, `path` and lines, with `document_id` null. In deep mode `tool_calls` counts the tool calls, `sources_considered` is 0, `usage` is summed over all model calls and `model` is the model of the last call. If the budget runs out with no answer at all, the response is a 500.

The CLI prints the answer, a blank line, a `Sources:` list with `[label] path:start-end` (the title when there is no path) and the link on the next line, then a dim line `<model> · <latency> ms · <input> in / <output> out`, where `<input>` is the whole prompt, cache included, followed by `(<n> cached)` when part of it was read from the cache. The web page shows the same line. With `LORE_LLM=fake` the answer is `Fake answer from N sources.` followed by `[1] [2]` and so on: it exercises the plumbing without a model.

`GET /v1/info` needs no key. It reports what the server runs, for clients and health dashboards:

```bash
curl localhost:8080/v1/info    # {"version":"0.1.0","llm":"fake","embedder":"hash","modes":["fast","sources","deep"]}
```

`modes` always lists `fast` and `sources`, and lists `deep` only when the model provider can use tools (`anthropic` and `fake`, not `openai`).

## Web page

`GET /` serves a small built-in page for asking questions, for example in a standup. It is one self-contained HTML file embedded in the binary (no build step, no CDN) and needs no key to load. Open `LORE_URL/` (`http://localhost:8080/` with `make run`), paste a `read` key and type a question. Enter sends it, Shift+Enter adds a new line.

The key stays in the browser's local storage (under `lore.key`) and is sent only to this server, as `Authorization: Bearer <key>`. If the browser blocks local storage, the page still works and you paste the key again on each visit. The answer shows with numbered citations that link to the sources; a source is a link only when its URL starts with `http://` or `https://`. The `sources` option shows the sources as bullets without a model call. The `deep` option is enabled only when `GET /v1/info` lists `deep` in `modes`.

The page logic is checked by `tests/web.rs`, which also runs `tests/web_pure.js` under node when node is installed.

## Admin page

`GET /admin` serves a page for operators: what the server runs, what it holds, and how far a push has got. Like the ask page it is one self-contained HTML file embedded in the binary and makes no request to any other host. Open `LORE_URL/admin` (`http://localhost:8080/admin` with `make run`) and paste the admin token (`dev-admin` with `make run`). The token is kept in the tab's session storage (under `lore.admin`), so it is gone when the tab closes, and is sent only to this server. A wrong token closes the view and forgets it; **Forget** does the same on purpose.

The page shows:

- **New tenant**: creates a tenant from its name. Create a key for it in its card.
- **Server**: version, embedder, model provider, the fast and deep models with their effort, the process's memory now and at its peak (Linux only), and the number of tenants, sources, documents and chunks.
- **Pushes**: the repo pushes that are running, and the latest 20 runs that ended or went stale, with their state (`queued` for another push to the same source, `cloning` while the commit is fetched, `indexing`, `done` or `failed`; a run nobody has updated for 90 seconds is marked stalled), files done out of the total, chunks written, how many of those were embedded and how many reused a vector (from the embedding cache, from the document's earlier chunks, or from a repeated chunk in the same file), the time spent embedding, and why a failed push failed. While a push runs the list refreshes every 3 seconds, otherwise every 30. When a push ends, the counts are read again.
- **Tenants**: for each tenant its sources (name, kind, indexed commit, documents, chunks, size on disk for repos, when it was last indexed, and for repos with a remote how the last [sync](#sync-a-repo-from-its-remote) went, with the remote, the commit and the error in its tooltip) and its keys (prefix, scopes, created, revoked). **Create key** makes a key with the ticked scopes and shows it once, with a copy button. **Revoke** asks for a confirmation first. **Delete tenant** and each source's **Delete** say what goes with it and ask you to type the name to confirm; a push in progress makes them fail with a message.
- **Storage**: the database size, the repo mirrors and checkouts on disk, each table's data and index bytes, and each index with its kind (the vector index is `hnsw`, the full-text index `gin`).

Push progress is read from the `index_runs` table, so a restart does not forget it and a run by another process shows too; only repo index runs are tracked. The CLI stops waiting for a push after 15 minutes, but the server finishes it; the page shows when it does.

The page uses these endpoints, which need the admin token like the rest of the admin API:

```bash
curl -H "Authorization: Bearer $LORE_ADMIN_TOKEN" localhost:8080/v1/admin/overview   # server, storage, tenants
curl -H "Authorization: Bearer $LORE_ADMIN_TOKEN" localhost:8080/v1/admin/pushes     # running and recent pushes
```

`GET /v1/admin/overview` answers `{"now", "server", "storage", "tenants"}`. `server` has `version`, `llm`, `embedder`, `modes`, `models` (`fast` and `deep`, each with `model` and `effort`), `memory` (`rss_bytes` and `peak_rss_bytes`, null outside Linux), and the totals `tenants`, `sources`, `documents` and `chunks`. `storage` has `database_bytes`, `repos_bytes`, `tables` (`name`, `rows_estimate`, `data_bytes`, `index_bytes`, `total_bytes`, largest first) and `indexes` (`name`, `table`, `method`, `bytes`). Each tenant has `id`, `name`, `created_at`, `sources` (`id`, `name`, `kind`, `indexed_commit`, `remote_url`, `last_sync_at`, `last_sync_commit`, `last_sync_error`, `documents`, `chunks`, `last_indexed_at`, `disk_bytes`) and `keys` (`id`, `prefix`, `scopes`, `created_at`, `revoked_at`). `last_indexed_at` is the later of a repo's last push and the source's newest document write, and null until something is indexed; `disk_bytes` is null for sources that are not repos. `last_sync_at` is when the last sync ended, `last_sync_commit` the remote's head at the last sync that worked, and `last_sync_error` why the last one failed (null when it worked). The database reads share one snapshot, so the totals add up; the repo directories are walked on each call, which takes longer the more files the checkouts hold.

`GET /v1/admin/pushes` answers `{"now", "running", "recent"}`, newest first, from the `index_runs` table. A push has `id`, `tenant`, `tenant_id`, `source`, `source_id`, `commit`, `full`, `state`, `started_at`, `finished_at`, `updated_at`, `files_total` (null until the bundle is fetched), `files_done`, `chunks`, `chunks_embedded`, `embed_ms`, `error` and `stale` (see the progress route above). `state` is the run's stage. `embed_ms` is wall-clock time in the embedder, including waits while another push uses it. CPU and memory are not measured per push: pushes and searches share one process.

Keys are created and revoked with the existing `POST /v1/admin/tenants/{id}/keys` and `DELETE /v1/admin/keys/{id}`, tenants created with `POST /v1/admin/tenants`, and tenants and sources deleted with `DELETE /v1/admin/tenants/{id}` and `DELETE /v1/admin/sources/{id}` (see [Deleting tenants and sources](#deleting-tenants-and-sources)). The page sets `Content-Security-Policy` so it can only talk to its own server and cannot be framed, and `Cache-Control: no-store`. Its logic is checked by `tests/web.rs`, which runs `tests/web_admin_pure.js` under node when node is installed.

## Use lore from agents

lore serves MCP (the Model Context Protocol) over streamable HTTP at `/mcp`, so an agent can search and ask questions about what you indexed. The endpoint is stateless and answers with plain JSON. It takes the same `lk_` tenant keys as the REST API, sent as `Authorization: Bearer <key>`, and the key needs the `read` scope. A missing or bad key is a 401 and a key without `read` is a 403, both with the usual `{"error": "..."}` body. Every tool sees only the key's own tenant.

Claude Code:

```bash
claude mcp add --transport http lore "$LORE_URL/mcp" --header "Authorization: Bearer $LORE_KEY"
```

Any other MCP client that speaks streamable HTTP works the same way: register `$LORE_URL/mcp` with the same `Authorization: Bearer <key>` header.

The tools are written for agents. `search` runs no language model, answers in well under a second, and shows a short excerpt of each hit, so an agent can search several times without filling its context. `ask` runs lore's own model (about 5 s fast, 10 to 60 s deep) and suits a person, or an agent without the code at hand; its description says so. With lore connected and nothing else said, Claude Code still mostly uses its own Grep, so add a line to `CLAUDE.md`:

> This repository is indexed in the lore MCP server. Use lore's search tool first: it returns ranked passages (path, line range and the code itself) in well under a second. Answer from those passages when they are enough, and Read a file only when they are not.

| Tool | Input | Returns |
| --- | --- | --- |
| `search` | `query`, `k` (1 to 50, default 8), `kind` (list of `code`, `doc`, `session`, `note`), `source` | One text item, `No hits.` or one block per hit: a header `[1] <source> <path>:<start>-<end> (<kind>: <symbols>) id=<document id>` (the title in place of the path for documents without one), then at most 12 lines of the hit, numbered: the window with the most query words, or the whole hit when it is at most 15 lines. Common indentation is removed and lines are cut at 160 characters |
| `ask` | `question`, `mode` (`fast`, the default; `deep` runs the agent loop), `source` | One text item: the answer, a `Sources:` list with one line per citation, and `(fast mode, <model>, 4321 in / 381 out, 4.9 s)` (deep mode adds the number of tool calls) |
| `get_document` | `document_id` (the id of a search hit or a citation) | The document's text under a `# <title>` header with its kind and path, cut to 100,000 characters. An unknown id, or another tenant's, is the tool error `document not found` |
| `list_sources` | none | One text item: a JSON array of `{name, kind, indexed_commit, documents}` |

`scripts/mcp_smoke.sh` runs the whole exchange against a server: `LORE_URL=http://localhost:8080 LORE_ADMIN_TOKEN=dev-admin scripts/mcp_smoke.sh` prints the sorted tool names, the `src/billing/plans.rs` hit and `MCP SMOKE OK`. By default `/mcp` accepts any `Host` header; set `LORE_MCP_ALLOWED_HOSTS` to restrict it.
