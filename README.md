# Enargeia

**A single-binary, local-inference entity-resolution engine that turns open feeds into a resolved, time-aware, human-corrected world model.**

Every serious intelligence platform runs the same loop: ingest many sources, *resolve* mentions
into canonical entities, and reason over the resolved graph rather than over raw documents.
Enargeia is that loop as one Rust binary and one SQLite file — with the discipline the open-source
tier usually skips:

- **Sticky decorrelation.** When a human says "these are not the same entity," that decision is
  stored as data and no automated pass can re-merge the pair later.
- **License provenance on every item.** Each ingested record carries a `license_class` asserted
  by its adapter, so commercial output can be gated honestly. See [DATA-ATTRIBUTION.md](DATA-ATTRIBUTION.md).
- **Time-aware entities.** Entities decay unless refreshed by a fresh source; reasoning runs over
  what is currently live, while resolution still dedups against full history.
- **Cost-tiered inference.** Deterministic matching handles the majority for free; an LLM — local
  by default — is spent only on the ambiguous residue, and its output is re-checked by the same
  matcher as everything else.
- **No infrastructure.** No Postgres, Redis, Elasticsearch, queues, or cloud account required.

## Status

Early. The pipeline works end to end and is tested, but extraction is currently heuristic
(regex + Jaro-Winkler), GDELT items are title-only, and match quality has not yet been measured
on a labeled sample. The roadmap below is in order of what closes that gap.

## Quickstart

```sh
cargo build --release
./target/release/enargeia ingest --gdelt "bitcoin" --rss https://cointelegraph.com/rss
./target/release/enargeia resolve
./target/release/enargeia status
./target/release/enargeia enrich        # Tier 2; uses local Ollama by default
./target/release/enargeia ask "Which companies are mentioned alongside Mastercard?"
```

Human corrections:

```sh
enargeia decorrelate <entity_a> <entity_b> --reason "different companies"
enargeia merge <keep_id> <absorb_id>      # refuses if the pair was decorrelated
enargeia expire
```

## Configuration

| Variable | Default | Purpose |
|---|---|---|
| `ENARGEIA_DB_PATH` | `data/enargeia.sqlite` | Engine database |
| `ENARGEIA_LLM_PROVIDER` | auto | `openai` (any OpenAI-compatible endpoint) or `anthropic` |
| `ENARGEIA_LLM_BASE_URL` | `http://localhost:11434` | OpenAI-compatible base URL (Ollama, vLLM, LM Studio…) |
| `ENARGEIA_LLM_MODEL` | `qwen2.5:7b` / `claude-sonnet-5` | Model name |
| `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` | — | Only if the endpoint requires one |
| `ENARGEIA_PULSE_DB_PATH` | — | Optional read-only adapter over a local Pulse Engine database |
| `RUST_LOG` | — | e.g. `enargeia=debug` |

## How it works

```
sources ──> ingest (dedup, license_class) ──> Tier 1: extract mentions, match against gazetteer
                                                   ├─ confident ──> merge into entity (unless decorrelated)
                                                   ├─ ambiguous ──> pending_review (human queue)
                                                   └─ unknown ───> new entity + needs_llm
                                              Tier 2: batched LLM ──> typed relations, re-matched
graph (entities · edges · decorrelations) ──> context assembler ──> ask (sourced answer)
```

## Roadmap

1. Zero-shot local NER (GLiNER via `gline-rs`) replacing the regex extractor.
2. Multi-signal probabilistic matcher (Fellegi–Sunter) with a committed precision/recall report.
3. Bi-temporal edges (`valid_at` / `invalid_at`) replacing the fixed expiry.
4. Reproducible decorrelation evaluation and a `why <entity>` provenance trace.
5. HTTP API, geocoding, first live layer (CelesTrak), embedded globe.

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
