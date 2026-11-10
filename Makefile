VERSION ?= dev
NS ?= lore
KCTX = --context kind-factory
GEN = deploy/k8s/overlays/generated
DATABASE_URL ?= postgres://lore:lore@localhost:5434/lore
export DATABASE_URL

.PHONY: db-up run test lint check eval eval-test image kind-up kind-load kind-secret kind-deploy kind-rollback kind-logs kind-delete prod-local prod-local-backup prod-local-down deploy-prod

db-up:
	docker compose up -d --wait db

run:
	LORE_EMBEDDER=hash LORE_LLM=fake LORE_ADMIN_TOKEN=dev-admin cargo run -- serve

test:
	cargo test --all-targets

lint:
	cargo fmt --all -- --check
	cargo clippy --all-targets -- -D warnings

check: lint test

# The search eval on the public question set, against the server at LORE_URL (default
# http://localhost:8080). Real numbers need the fastembed embedder; see bench/eval/README.md.
eval:
	bench/eval/local.sh $(EVAL_LABEL)

eval-test:
	python3 -m unittest discover -s bench/eval -p 'test_*.py'

image:
	docker build --build-arg VERSION=$(VERSION) -t lore:$(VERSION) .

# Kind deployment. Every kubectl call passes $(KCTX): the kubeconfig also holds other
# clusters, and `kind create cluster` switches the current context.
# NS must be lore (the releaser's) or lore-dev-* (a builder's), one word of [a-z0-9-] only:
# CHECK_NS refuses the wrong names at parse time (so `make -n` refuses too), and
# GUARD_NS repeats the test on the exact string the recipes see, so no value can be split
# into several kubectl arguments.
export NS
CHECK_NS = $(if $(word 2,$(NS)),$(error NS must be one word, got '$(NS)'),$(if $(filter lore lore-dev-%,$(NS)),,$(error NS must be lore or lore-dev-*, got '$(NS)')))
# The character class is spelled out: a-z also matches capitals in some locales.
GUARD_NS = case "$$NS" in *[!abcdefghijklmnopqrstuvwxyz0123456789-]*|"") echo "NS must be lore or lore-dev-*"; exit 1 ;; lore|lore-dev-?*) ;; *) echo "NS must be lore or lore-dev-*"; exit 1 ;; esac
GUARD_VERSION = case "$(VERSION)" in *[!abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-]*|"") echo "VERSION must be letters, digits, . _ -"; exit 1 ;; esac

kind-up:
	kind get clusters | grep -qx factory || kind create cluster --name factory

kind-load:
	kind load docker-image lore:$(VERSION) --name factory

kind-secret:
	@: $(CHECK_NS)
	@$(GUARD_NS)
	kubectl $(KCTX) create namespace $(NS) --dry-run=client -o yaml | kubectl $(KCTX) apply -f -
	@if ! kubectl $(KCTX) -n $(NS) get secret lore-db >/dev/null 2>&1; then \
		pw=$$(openssl rand -hex 16); \
		[ -n "$$pw" ] || { echo "openssl produced no password"; exit 1; }; \
		kubectl $(KCTX) -n $(NS) create secret generic lore-db --from-literal=POSTGRES_PASSWORD="$$pw"; \
	fi
	@if ! kubectl $(KCTX) -n $(NS) get secret lore-env >/dev/null 2>&1; then \
		pw=$$(kubectl $(KCTX) -n $(NS) get secret lore-db -o jsonpath='{.data.POSTGRES_PASSWORD}' | base64 -d); \
		tok=$$(openssl rand -hex 24); \
		[ -n "$$pw" ] && [ -n "$$tok" ] || { echo "empty db password or admin token, not creating lore-env"; exit 1; }; \
		kubectl $(KCTX) -n $(NS) create secret generic lore-env \
			--from-literal=LORE_ADMIN_TOKEN="$$tok" \
			--from-literal=LORE_DATABASE_URL="postgres://lore:$$pw@lore-db:5432/lore"; \
	fi

# Per worktree and per NS, $(GEN) records two tags, written only after a rollout succeeds:
# .current-<NS> is the last tag that rolled out, .previous-<NS> the one it replaced.
kind-deploy: kind-secret
	@: $(CHECK_NS)
	@$(GUARD_NS)
	@$(GUARD_VERSION)
	mkdir -p $(GEN)
	printf 'apiVersion: kustomize.config.k8s.io/v1beta1\nkind: Kustomization\nresources:\n  - ../../base\nnamespace: $(NS)\nimages:\n  - name: lore\n    newName: lore\n    newTag: "$(VERSION)"\n' > $(GEN)/kustomization.yaml
	kubectl $(KCTX) apply -k $(GEN)
	kubectl $(KCTX) -n $(NS) rollout status deploy/lore --timeout=300s
	@cur=$$(cat $(GEN)/.current-$(NS) 2>/dev/null || true); \
	if [ "$$cur" != "$(VERSION)" ]; then \
		if [ -n "$$cur" ]; then echo "$$cur" > $(GEN)/.previous-$(NS); echo "previous tag $$cur saved to $(GEN)/.previous-$(NS)"; fi; \
		echo "$(VERSION)" > $(GEN)/.current-$(NS); \
	fi

# Goes back to .current when the live tag is something else (the last rollout failed),
# otherwise to .previous. With no record it falls back to the cluster's own undo.
kind-rollback:
	@: $(CHECK_NS)
	@$(GUARD_NS)
	@live=$$(kubectl $(KCTX) -n $(NS) get deploy lore -o jsonpath='{.spec.template.spec.containers[0].image}' 2>/dev/null || true); \
	live=$${live##*:}; \
	cur=$$(cat $(GEN)/.current-$(NS) 2>/dev/null || true); \
	prev=$$(cat $(GEN)/.previous-$(NS) 2>/dev/null || true); \
	if [ -n "$$cur" ] && [ -n "$$live" ] && [ "$$live" != "$$cur" ]; then target=$$cur; else target=$$prev; fi; \
	if [ -n "$$target" ]; then \
		echo "rolling back $(NS) to $$target"; \
		make kind-deploy NS=$(NS) VERSION=$$target; \
	else \
		kubectl $(KCTX) -n $(NS) rollout undo deploy/lore \
			&& kubectl $(KCTX) -n $(NS) rollout status deploy/lore --timeout=300s; \
	fi

kind-logs:
	@: $(CHECK_NS)
	@$(GUARD_NS)
	kubectl $(KCTX) -n $(NS) logs deploy/lore --tail=200

# Dev namespaces only: lore (the releaser's) is refused, and so is anything not named lore-dev-*
# or holding a character outside [a-z0-9-] (a space would make kubectl delete several).
kind-delete:
	@: $(CHECK_NS)
	@case "$$NS" in *[!abcdefghijklmnopqrstuvwxyz0123456789-]*|"") echo "kind-delete is for dev namespaces only"; exit 1 ;; lore-dev-?*) ;; *) echo "kind-delete is for dev namespaces only"; exit 1 ;; esac
	kubectl $(KCTX) delete namespace $(NS) --wait=false

# Rehearsal of the production stack on this machine (docs/self-hosting.md): Caddy on
# 8081 and 8443, the fake model, the hash embedder. deploy/.env.prod holds random local values.
COMPOSE_LOCAL = cd deploy && VERSION=$(VERSION) LORE_DOMAIN=localhost docker compose -p lore-local --env-file .env.prod -f compose.prod.yml -f compose.local.yml

prod-local:
	$(MAKE) image VERSION=$(VERSION)
	@test -f deploy/.env.prod || { umask 077; printf 'POSTGRES_PASSWORD=%s\nLORE_ADMIN_TOKEN=%s\n' "$$(openssl rand -hex 16)" "$$(openssl rand -hex 24)" > deploy/.env.prod; echo "created deploy/.env.prod"; }
	$(COMPOSE_LOCAL) up -d --wait
	@i=0; until body=$$(curl -sk https://localhost:8443/healthz) && echo "$$body" | grep -q '"ok"'; do i=$$((i+2)); if [ $$i -gt 120 ]; then echo "healthz did not answer ok within 120s"; exit 1; fi; sleep 2; done; echo "$$body"
	@code=$$(curl -sk -o /dev/null -w '%{http_code}' -X POST https://localhost:8443/mcp -H 'Content-Type: application/json' -d '{}'); echo "POST /mcp -> $$code"; [ "$$code" = 401 ] || { echo "expected 401: MCP must be reachable through Caddy"; exit 1; }
	@code=$$(curl -sk -o /dev/null -w '%{http_code}' https://localhost:8443/v1/admin/tenants); echo "GET /v1/admin/tenants -> $$code"; [ "$$code" = 403 ] || { echo "expected 403: Caddy must block the admin API"; exit 1; }

prod-local-backup:
	@set -e; f=$$($(COMPOSE_LOCAL) exec -T backup bash -c 'set -o pipefail; f=/backups/lore-now-$$(date +%s).sql.gz; pg_dump -h db -U lore lore | gzip > $$f && basename $$f'); \
	echo "backups/$$f"; \
	gzip -dc deploy/backups/$$f | grep -q 'CREATE TABLE public.chunks' || { echo "dump has no chunks table"; exit 1; }

prod-local-down:
	$(COMPOSE_LOCAL) down -v

# Run by hand only: builds the image and deploys it to HOST over ssh. CI never runs it.
deploy-prod:
	@test -n "$(HOST)" -a -n "$(LORE_DOMAIN)" -a "$(VERSION)" != dev || { echo "usage: make deploy-prod HOST=root@<ip> LORE_DOMAIN=<domain> VERSION=<tag>"; exit 1; }
	docker build --platform linux/arm64 --build-arg VERSION=$(VERSION) -t lore:$(VERSION) .
	docker save lore:$(VERSION) | gzip | ssh $(HOST) 'gunzip | docker load'
	ssh $(HOST) 'mkdir -p /opt/lore/backups && test -f /opt/lore/.env.prod' || { echo "create /opt/lore/.env.prod first, see docs/self-hosting.md"; exit 1; }
	scp deploy/compose.prod.yml deploy/Caddyfile $(HOST):/opt/lore/
	ssh $(HOST) 'cd /opt/lore && VERSION=$(VERSION) LORE_DOMAIN=$(LORE_DOMAIN) docker compose --env-file .env.prod -f compose.prod.yml up -d --wait'
	curl -fsS --retry 10 --retry-delay 3 --retry-all-errors https://$(LORE_DOMAIN)/healthz
