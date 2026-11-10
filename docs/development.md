# Development

You need Rust 1.95 (pinned in `rust-toolchain.toml`) and Docker. Postgres 17 with pgvector runs in Docker, never on the host.

```bash
make db-up      # Postgres + pgvector on localhost:5434 (user, password, db: lore)
make run        # lore serve on http://localhost:8080 (hash embedder, fake model)
make test       # cargo test --all-targets (DB tests read DATABASE_URL, set by the Makefile)
make lint       # cargo fmt --check and cargo clippy -D warnings
make check      # lint + test: the pre-push gate, and what CI runs
make image VERSION=dev   # docker build, tagged lore:dev
make eval       # the search eval on the public question set, against LORE_URL (bench/eval/README.md)
make eval-test  # the eval's own unit tests (python3; cargo test runs them too)
```

`docker compose up -d --build` starts both Postgres and lore in containers.
Check it with `curl localhost:8080/healthz`, which returns `{"status":"ok","db":"ok"}`.
`/livez` is the liveness probe and does not touch the database.

## Deploy to kind

The manifests in `deploy/k8s/base/` run lore and a pgvector Postgres on the kind cluster `factory`. You need `kind`, `kubectl`, Docker and `openssl`. Every `kubectl` call in the Makefile passes `--context kind-factory`, so the current context never matters.

```bash
make kind-up                          # creates the cluster `factory` if it does not exist
make image VERSION=dev                # docker build, tagged lore:dev
make kind-load VERSION=dev            # loads the image into the cluster
make kind-deploy VERSION=dev          # namespace and secrets if missing, then apply and wait for the rollout
```

`NS` picks the namespace (default `lore`; use `lore-dev-<name>` for experiments, for example `make kind-deploy NS=lore-dev-t7 VERSION=dev-t7`). `kind-deploy` generates an overlay in `deploy/k8s/overlays/generated/` (gitignored) that sets the image tag, and keeps two records there per namespace, written only after a rollout succeeds: `.current-<NS>` (the last tag that rolled out) and `.previous-<NS>` (the one it replaced). The records live in your checkout, so they are per worktree, not per cluster. Use a new `VERSION` for every build: kind loads images by tag and the Deployment does not change when the tag does not, so a rebuilt image under the same tag is not picked up. `NS` must be one word of lowercase letters, digits and `-`.

| Target | What it does |
|---|---|
| `make kind-secret` | Creates the namespace and the secrets `lore-db` and `lore-env` (random password and admin token) if they are missing. Existing secrets are never changed |
| `make kind-rollback` | Goes back to the last tag that rolled out when the live tag is a newer one that failed, otherwise to the previous tag; runs `kubectl rollout undo` when nothing is recorded |
| `make kind-logs` | Last 200 log lines of the lore pod |
| `make kind-delete NS=lore-dev-<name>` | Deletes a dev namespace. It refuses `lore`, any name that is not `lore-dev-*`, and any value with a space or other character outside `[a-z0-9-]` |

The first start downloads the embedding model into the `lore-data` volume, and the startup probe allows 3 minutes for it.

Verify the deployment (replace `lore` with your namespace, and `18080` with `18081` for a dev namespace):

```bash
kubectl --context kind-factory -n lore port-forward svc/lore 18080:80 >/tmp/lore-pf.log 2>&1 &
sleep 3
export LORE_URL=http://localhost:18080
export LORE_ADMIN_TOKEN=$(kubectl --context kind-factory -n lore get secret lore-env -o jsonpath='{.data.LORE_ADMIN_TOKEN}' | base64 -d)
cargo build --release && export PATH="$PWD/target/release:$PATH"
curl -s $LORE_URL/healthz            # {"status":"ok","db":"ok"}
scripts/smoke.sh m1                  # last line: SMOKE M1 OK
scripts/smoke.sh m2                  # last line: SMOKE M2 OK
scripts/smoke.sh m5                  # last line: SMOKE M5 OK (deep mode, with LORE_LLM=fake)
scripts/mcp_smoke.sh                 # last line: MCP SMOKE OK
```

`scripts/smoke.sh <stage>` (`m1`, `m2`, `m4`, `m5`, or `all` for every stage that exists) needs `LORE_URL`, `LORE_ADMIN_TOKEN`, and `lore`, `jq`, `git` and `curl` on the path. It creates fresh tenants on each run. It also works against `make run`. `scripts/mcp_smoke.sh` checks the MCP endpoint (see [Use lore from agents](usage.md#use-lore-from-agents)); it needs `curl` as well.

