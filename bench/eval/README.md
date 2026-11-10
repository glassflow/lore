# Retrieval eval and agent comparison

Two measurements for changes to search: a labelled search eval, and Claude Code run with and without lore. Both are Python 3.9+ with the standard library only.

| File | What it is |
|---|---|
| `eval` | The search eval (runs `retrieval.py`) |
| `local.sh` | Indexes this repo in a local server and runs the eval on the public set: `make eval` |
| `agents.py` | Claude Code without lore against Claude Code with lore over MCP |
| `questions/lore.json` | The public set: 20 questions about lore's own code |
| `results/` | One `LABEL.json` per eval run (gitignored), and the committed baseline |
| `test_eval.py` | Unit tests of the scoring, the diff, the stream-json parsing and the grading |

## Reproduce the public-set numbers

From a fresh checkout, with Docker running:

```bash
make db-up
cargo build --release
LORE_EMBEDDER=fastembed LORE_LLM=fake LORE_ADMIN_TOKEN=dev-admin target/release/lore serve   # in another shell
make eval
```

`make eval` runs `bench/eval/local.sh`. It creates a tenant and a key, clones this checkout at the commit the set was written against (`commit` in `questions/lore.json`), pushes that clone as the source `lore`, and runs the eval. The indexed code stays at that commit whatever you have checked out, so the questions and their gold ranges stay valid, and only the server's ranking changes between runs. The first push embeds every file and took 2 minutes 20 seconds on an M2 Pro.

The label defaults to `<embedder>-<short HEAD>`, for example `fastembed-105500e`; `make eval EVAL_LABEL=mine` picks another. `local.sh` reads `LORE_URL` (default `http://localhost:8080`), `LORE_ADMIN_TOKEN` (default `dev-admin`) and `LORE_BIN` (default `lore` on the path, else `target/release/lore`, else `target/debug/lore`). `EVAL_KEY=lk_...` reuses the tenant of an earlier run, so nothing is pushed again; `local.sh` prints the key it creates. It never reads `LORE_KEY`, so a key for another server in your shell is not used by mistake.

`make run` uses the hash embedder, which gives much lower numbers (see the baseline below). Compare runs that use the same embedder.

Then compare with the committed baseline:

```bash
bench/eval/eval --diff baseline-fastembed-105500e fastembed-$(git rev-parse --short HEAD)
```

## The search eval

```bash
bench/eval/eval LABEL URL KEY questions.json...     # writes results/LABEL.json
bench/eval/eval --diff A B                          # A and B are labels or result files
bench/eval/eval --check REPO questions.json...      # shows each gold range as it reads at the set's commit
```

For each question the eval calls `GET /v1/search?q=<q>&k=50&source=<repo>` once (after one untimed warm-up search). A hit is a result in the question's source and file whose lines overlap a gold range. With `need: any` the question's rank is the best rank of its gold ranges; with `need: all` it is the worst, and the question is a miss unless every range is in the 50 results. The eval reports hit@5, hit@12 (fast mode's k: the passages a fast answer is written from), hit@50 and MRR (the mean of 1/rank, 0 for a miss), overall and per category, and the median latency of the searches as the client sees them. `results/LABEL.json` also keeps each question's rank, the rank of each gold range, its latency and its top 5 hits, so `--diff` can list every question whose rank changed.

`KEY` needs the `read` scope. Several sets can be run together; question ids must be unique across them.

## Question sets

```json
{"commit": "105500e75cdf3d65886d5220ca070440735b50bb",
 "questions": [
  {"id": "lore-01", "repo": "lore", "category": "enumeration",
   "q": "what kinds of documents can lore store?", "answer": "code, doc, session, note",
   "need": "any", "gold": [{"path": "src/search.rs", "start": 33, "end": 33}]}
 ]}
```

- `repo` is the source name the repo was pushed as, and the `source` filter of the search.
- `category` is one of `enumeration` (which X are there), `schema` (tables, fields, types), `where` (which file or function does X), `config` (settings and defaults) and `how` (how something works).
- `gold` lists line ranges, 1-based and inclusive. A range without `start` and `end` stands for the whole file. `need` is `any` (the default) or `all`.
- `answer` is the reference answer. The search eval does not read it; the agent comparison grades against it.
- `commit` is the commit the gold ranges were written against: one sha for the set, or an object of repo name to sha for a set over several repos. Gold ranges go stale as code moves. Run `--check` against a checkout to print every range at that commit, and again with `--at HEAD` to see what moved. A set may also be a bare list of questions, with no commit recorded.

The public set has 20 questions, 4 per category, about lore at commit `105500e`. Its gold ranges point at code and migrations, not at `docs/`: a hit on a doc page that states the answer counts as a miss.

Private sets about other code live outside this repo and are passed by path, for example `bench/eval/eval mine $LORE_URL $LORE_KEY ~/lore-eval/private.json`. Push each repo under the name its questions use, at the recorded commit.

## Agent comparison

```bash
git worktree add /tmp/lore-105500e 105500e        # a checkout at the set's commit
export LORE_URL=http://localhost:8080 LORE_KEY=lk_...   # a read key of the tenant that holds the repo
bench/eval/agents.py LABEL /tmp/lore-105500e bench/eval/questions/lore.json --source lore
```

For each prompt, two `claude -p` runs start at the same time in the checkout, both with only `Read`, `Grep` and `Glob` (`--tools`) and `--strict-mcp-config`:

- **no lore**: no MCP servers.
- **lore search-first**: lore's MCP server (`--mcp-config`, a temp file holding the key) and the search-first `CLAUDE.md` line from [Use lore from agents](../../docs/usage.md#use-lore-from-agents), passed with `--append-system-prompt`.

The workloads are single questions, prompts of 4 and prompts of 8 (`--sizes 1,4,8`; sizes above 1 keep only full groups). Prompts always go on stdin: a prompt that starts with `-` would be read as an option, and `--tools`, `--allowedTools` and `--mcp-config` take several values and would swallow a prompt that follows them.

Each run uses `--output-format stream-json --verbose`, which is parsed for the wall time (`duration_ms`), tool calls by name, the final context (input, cache creation and cache read tokens of the last model call), the total input tokens (the same three summed over the run) and the cost (`total_cost_usd`). A second `claude -p` with no tools (`--tools ""`) grades each answer against the reference answers as correct, partial or wrong per question.

Everything goes to `results/agents-LABEL/`: each run's stream and stderr, each grading reply, `summary.json` and `summary.md` (one row per workload and setup). The agent refuses a questions file inside the checkout it runs in, since Claude Code could read the answers there.

Options: `--setups none`, `--setups lore`, `--limit N` (the first N questions), `--model` (default `claude-opus-5-5`, or `BENCH_MODEL`), `--effort` (default `medium`), `--grader-model` (default the same model) and `--dry-run`, which prints the commands and spends nothing. Without `--dry-run` this costs money: every prompt is two Claude Code runs and two grading calls.

## Tests

```bash
make eval-test     # python3 -m unittest discover -s bench/eval -p 'test_*.py'
```

`cargo test` runs them too (`tests/bench_eval.rs`), and skips them when `python3` is missing. They use a fake search server on localhost, a recorded stream-json run (`testdata/stream.jsonl`) and a fake `claude` that records its arguments and stdin.
