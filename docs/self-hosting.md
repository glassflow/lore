# Self-hosting lore on a VPS

lore runs on one small arm64 VPS (a Hetzner CAX21 class server is enough) with docker compose. The stack has four services:

- `lore`, the server;
- `db`, Postgres 17 with pgvector;
- `caddy`, which gets a TLS certificate by itself and refuses `/v1/admin/*` from the internet;
- `backup`, a `pg_dump` once a day, kept for seven days.

The files are in `deploy/`: `compose.prod.yml`, `Caddyfile`, `env.prod.example`. `compose.local.yml` is only for the rehearsal below.

## Rehearse it on your machine

```sh
make prod-local            # builds the image, starts the stack, checks it
make prod-local-backup     # takes a dump and checks that it holds the schema
make prod-local-down       # stops the stack and deletes its volumes
```

`make prod-local` needs docker compose 2.24 or newer and free ports 8081 and 8443. It creates `deploy/.env.prod` with random local values (it is ignored by git), uses `LORE_LLM=fake` and the hash embedder, so it needs no API key and downloads no model. It prints the `/healthz` body, then checks that `POST /mcp` without a key answers 401 (MCP is reachable through Caddy) and that `/v1/admin/tenants` answers 403 (Caddy blocks the admin API). Caddy serves `localhost` with its own CA, so use `curl -k`.

## Deploy

1. Create an arm64 VPS with Debian 13, Docker Engine and the compose plugin (2.24 or newer). The firewall allows only ports 22, 80 and 443.
2. Point a DNS A record for your domain at the server's IP.
3. On the server, copy `deploy/env.prod.example` to `/opt/lore/.env.prod` and fill it in: `POSTGRES_PASSWORD`, `LORE_ADMIN_TOKEN` (`openssl rand -hex 24`), `ANTHROPIC_API_KEY`. Keep the file private (`chmod 600`).
4. From a checkout of this repo, run:

   ```sh
   make deploy-prod HOST=root@<ip> LORE_DOMAIN=<domain> VERSION=<tag>
   ```

   It builds the image for arm64, sends it over ssh, copies `compose.prod.yml` and `Caddyfile` to `/opt/lore`, starts the stack and prints `/healthz`. The first start downloads the embedding model (about 130 MB) into the `loredata` volume, which is why the `lore` health check has a 180 second start period. A download that does not finish within `LORE_MODEL_DOWNLOAD_TIMEOUT_SECS` (5 minutes) stops the start with an error. To skip the download, copy the model directory of another install into the volume first, as described in [the configuration](configuration.md#the-embedding-model-download).

## Admin commands

Caddy blocks `/v1/admin`, so admin commands run on the server. The container already has `LORE_ADMIN_TOKEN` in its environment:

```sh
ssh <host> 'cd /opt/lore && VERSION=<deployed tag> LORE_DOMAIN=<domain> docker compose --env-file .env.prod -f compose.prod.yml exec lore lore --url http://localhost:8080 admin tenant create --name <tenant>'
```

`admin tenant delete <id>` and `admin source delete <id>` run the same way, and the [admin page](usage.md#admin-page) does all of this too when you reach it on the server (for example over an ssh tunnel). Deleting frees space inside Postgres for new rows but does not shrink its files (see [Deleting tenants and sources](usage.md#deleting-tenants-and-sources)).

Everything else (`repo push`, `search`, `ask`, MCP) goes through `https://<domain>` with a tenant key.

## Backups and restore

The `backup` service writes `/opt/lore/backups/lore-<date>-<time>.sql.gz` every 24 hours and deletes dumps older than 7 days. Copy them off the server if you want more than one machine's worth of safety.

To restore, on the server. The dump only creates objects, so loading it over a database that already has the tables fails on duplicate keys and loads nothing. Empty the schema first, with lore stopped:

```sh
cd /opt/lore
export VERSION=<deployed tag> LORE_DOMAIN=<domain>
C="docker compose --env-file .env.prod -f compose.prod.yml"
gunzip -t backups/<file> &&
$C stop lore backup &&
$C exec -T db psql -U lore -v ON_ERROR_STOP=1 lore -c 'DROP SCHEMA public CASCADE; CREATE SCHEMA public;' &&
gunzip -c backups/<file> | $C exec -T db psql -U lore -v ON_ERROR_STOP=1 lore &&
$C up -d --wait
```

`compose.prod.yml` refuses to load without `VERSION` and `LORE_DOMAIN`, so set them to the values of the running deployment first, replacing the whole placeholder, `<` and `>` included; without them the first compose command fails before anything is changed. The first link, `gunzip -t`, checks that the dump file exists and is intact before anything is touched (a pipe reports psql's status, not gunzip's, so a bad file would otherwise pass). The steps are chained with `&&`, so a failed load never reaches `up`. If the load fails, lore stays stopped: fix the cause and run the block again.

The dump creates the `vector` extension again, so nothing else is needed. `ON_ERROR_STOP=1` makes psql stop at the first error instead of exiting 0 over a half-loaded database.

The `loredata` volume (repo mirrors and the model cache) is not in the dump. After a restore onto a fresh volume, run `lore repo push --full` for each repo (a repo with a `remote_url` gets its mirror and checkout back on its next sync by itself). lore also answers `need_full_bundle`, and the CLI then retries with a full bundle on its own.

## Private repos

The deployment sets no `LORE_GIT_TOKEN_<TENANT>`, no `LORE_SYNC_INTERVAL_SECS` and no `LORE_GITHUB_WEBHOOK_SECRET`, so the poller and the webhook are off and have no token to use. A private repo is synced by a call to `POST /v1/repos/{id}/sync` that carries the token in its body (see [usage](usage.md#sync-a-repo-from-its-remote)): lore uses it for that fetch and never stores or logs it. It is visible in the environment of the git child process (`/proc/<pid>/environ`) while the fetch runs, so keep other users off the host.

## Upgrade and roll back

Upgrade by running `make deploy-prod` again with a new `VERSION`. Roll back by running it with the previous `VERSION`. Migrations only move forward, so undoing one means restoring a backup taken before the upgrade.
