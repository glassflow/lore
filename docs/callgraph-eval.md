# Symbol retrieval validation

The fourth RRF list changes ranking, not the search response shape. It activates on explicit symbol names or call/definition/connection intent, rather than incidental English words. For example, both `what calls index_text?` and `how does checkout reach persist?` activate it. An ordinary question about a time budget does not.

## Ranking

Local search-only evaluation uses `fastembed:bge-small-en-v1.5`, the default embedding model, and no language model. Both servers index the same 117 files and 1,773 chunks from commit `105500e75cdf3d65886d5220ca070440735b50bb`. The baseline executable is built from that commit. Matched comparisons also use identical stored vectors and chunk ids in isolated databases. Build baseline and candidate in separate Cargo target directories, or verify copied executables, to avoid stale shared-target artifacts.

The public 20-question set from #32 has **no per-question rank changes**:

| Category | Baseline ranks | Symbol retrieval ranks |
| --- | --- | --- |
| enumeration (01–04) | 1, miss, 10, miss | 1, miss, 10, miss |
| schema (05–08) | 10, 2, 13, 35 | 10, 2, 13, 35 |
| where (09–12) | 2, 1, 1, 29 | 2, 1, 1, 29 |
| config (13–16) | 12, 14, 10, 2 | 12, 14, 10, 2 |
| how (17–20) | 1, 28, 6, 1 | 1, 28, 6, 1 |

Overall hit@5 is 8/20, hit@12 13/20, hit@50 18/20 and MRR 0.365 for both. This is no-regression evidence, not evidence that the general suite improves.

The supplemental [symbol questions](callgraph-queries.json) were specified from call sites and definitions in the original source before comparing their results. They cover two caller lookups and two definition/callee relationships. Gold ranges remain fixed at the original commit; `need: all` requires both ranges for the how questions.

| Question | Baseline rank | Symbol retrieval rank |
| --- | ---: | ---: |
| where `index_text` is called for sessions | 2 | 1 |
| where MCP uses `document_text` | 1 | 1 |
| how `session_id` chooses and bounds its value | 4 | 2 |
| how `index_text` embeds chunks asynchronously | 27 | 34 |

Where MRR improves from 0.750 to 1.000; how MRR from 0.144 to 0.265. Overall MRR improves from 0.447 to 0.632. Hit@5 and hit@12 stay 3/4; hit@50 stays 4/4. The last question **regresses from 27 to 34**, so this is a small mixed-result set, not a general quality guarantee. Common-name caps can omit useful callees.

With the #32 search-only harness, run each server against the same fixed corpus and gold sets:

```bash
bench/eval/eval LABEL URL KEY bench/eval/questions/lore.json
bench/eval/eval SYMBOL_LABEL URL KEY docs/callgraph-queries.json
bench/eval/eval --diff BASE_LABEL CANDIDATE_LABEL
```

No Claude comparison or paid grading is needed.

## Resource gates

`tests/callgraph.rs` contains a separately run latency gate:

```bash
DATABASE_URL=postgres://lore:lore@localhost:5434/lore_i37 \
  cargo test --test callgraph related_latency -- --ignored --nocapture
```

It starts a fresh process and database, stores 20,092 chunks, and measures 40 calls including the first call, misses, two-name queries, eight symbols with five definitions each, and a 25-definition expansion with 50 caller candidates and 64 selected callee names. It alternates unfiltered and source-filtered lookups. The heavy rows hold 256 metadata entries of up to 128 bytes. Definitions fill the output before expansion when the 30-result budget is already full. The full suite separately checks 100-definition and 200-caller truncation, and retrieval through a one-connection pool.

On the delivered implementation, the first call took **4.825 ms** and the worst of 40 took **32.093 ms**, below the issue's **50 ms** added-latency ceiling. These are added-branch wall times, not full search times. No latency limit was loosened.

The new GIN indexes use `fastupdate=off`. A default pending list made a negative 64-name lookup scan recent bulk inserts and miss the 50 ms gate. Direct index updates shift that work to ingestion instead; lookup limits and accuracy were not relaxed. This gate measures the added branch, not embedding/model startup, network latency or a production-sized corpus.

Full cold HTTP searches also exercise the default local embedding model, `k=50` and `source=lore`. Each baseline/candidate pair starts fresh servers on cloned data with identical stored vectors and ids; their first search is timed after the server finishes loading the model, without query warm-up. Each query runs in both server orders. The real 1,773-chunk corpus covers `index_text` and eight real symbol names. A 21,863-chunk copy adds 20,000 distractors, 40 five-per-name definitions with maximum-sized metadata arrays, and 50 caller candidates as in the branch gate.

| Query shape | Largest candidate minus baseline (ms) |
| --- | ---: |
| Real `index_text` lookup | 10.392 |
| Eight real symbols | 25.962 |
| Heavy corpus `checkout` lookup | 25.185 |
| Heavy corpus `persist` lookup | -7.343 |
| Absent symbol | -54.154 |
| Two background names | 12.679 |
| Five heavy symbols, caller/callee expansion | 18.152 |
| Eight heavy symbols, definition budget full | 2.457 |

All 16 paired first searches (32 fresh server processes) remain below **50 ms** added latency; the largest increase is **25.962 ms**, not an average. Absolute candidate search times reach 161.189 ms, since embedding and existing retrieval are included. These local measurements are not a production latency guarantee or a controlled speedup claim.

The full-entrypoint check caught two costs absent from the original unfiltered branch-only check: extra cold pool connections, and source-first plans that scanned all chunks for absent callee names. Search now embeds before acquiring a single connection for lexical, vector, related and hit loading. Symbol queries probe GIN one name at a time, materialize matching ids only, then apply source filters and stable ordering. Limits, matches and ambiguity rules are unchanged.

With the final migration, real local embeddings, and the fixed 117-file corpus, a fresh push wrote 1,773 chunks in **135.642 s**, including **123.435 s** embedding all 1,773 chunks. A forced metadata re-index wrote the same 1,773 chunks in **7.601 s**, reusing every vector (zero embedding calls). The re-index test invalidated stored content hashes in its isolated database so it exercised actual deletes/inserts and GIN updates, not the unchanged-file fast path. These are representative write-cost measurements, not a controlled ingestion speedup claim against the baseline.
