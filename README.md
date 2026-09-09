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

Working, early, honest about its numbers. Zero-shot NER runs locally (GLiNER, ONNX, CPU); the
matcher is measured on 261 labeled pairs from a real corpus ([eval/REPORT.md](eval/REPORT.md));
bi-temporal relations, sticky decorrelation, provenance, missions, a standing watch, scoped
client tokens, and a globe with live satellite and seismic layers are all in. Tier 2 quality
is bounded by the local model you point it at (a 7B model needs the grounding checks it gets);
the corpus is whatever feeds you give it. See the roadmap for what is next.

## Quickstart

```sh
cargo build --release
./target/release/enargeia ingest --gdelt "bitcoin" --rss https://cointelegraph.com/rss
./target/release/enargeia resolve
./target/release/enargeia status
./target/release/enargeia enrich        # Tier 2; uses local Ollama by default
./target/release/enargeia ask "Which companies are mentioned alongside Mastercard?"
```

Human-in-the-loop review and provenance:

```sh
enargeia review                           # candidates awaiting a decision, with evidence
enargeia decide <candidate_id> confirm    # or: reject (creates + decorrelates) | new
enargeia why "Mistral AI"                 # every mention, source, score, relation, decision
enargeia dossier "Mistral AI" --out d.md  # target profile: dated timeline, network, sources, grounded assessment
enargeia ask "Who runs Acme?" --as-of 2025-12-31   # the graph as it was, not as it is
enargeia decorrelate <entity_a> <entity_b> --reason "different companies"
enargeia merge <keep_id> <absorb_id>      # refuses if the pair was decorrelated
enargeia expire
enargeia eval decorrelation               # proves a rejected merge never comes back
```

Relations are bi-temporal: `valid_at` comes from the source's publication date, and
exclusive relation types (`ceo_of`, `headquartered_in`, `acquired_by`, …) invalidate their
predecessor when a newer contradicting fact arrives — an older fact processed late is stored
already superseded, so ingestion order never rewrites history.

## Serve: API and globe

```sh
enargeia geo fetch && enargeia geo load && enargeia geo code   # GeoNames gazetteer (CC BY 4.0)
enargeia serve --bind 127.0.0.1:8787 [--token <secret>]
```

`/` is a single-file CesiumJS globe: resolved entities with coordinates, a live CelesTrak
satellite layer propagated with SGP4, a USGS seismic/explosion layer, search, the review
queue, and the ask box. Everything it does goes through the JSON API under `/api`
(`/api/entities`, `/api/entities/{id}`, `/api/entities/{id}/why`, `/api/edges`,
`/api/candidates`, `/api/geo`, `/api/live/satellites`, `/api/live/quakes?feed=2.5_day`,
`POST /api/ask`, `POST /api/candidates/{id}/decision`,
`POST /api/decorrelate`). GET endpoints are open; endpoints that change the graph or call
the LLM require `Authorization: Bearer <token>` when `--token`/`ENARGEIA_TOKEN` is set.

## Clients and scoped tokens

```sh
enargeia token create --name pcg-dashboard --role client --ask-daily-limit 50
enargeia token create --name analyst-1 --role analyst
enargeia token list
enargeia token revoke pcg-dashboard
```

| Role | Decisions | Decorrelate | `ask` | Sources cited |
|---|---|---|---|---|
| `operator` (root `ENARGEIA_TOKEN`, or a token) | yes | yes | unlimited | all |
| `analyst` | yes | no | unlimited unless limited | all unless `--commercial-only` |
| `client` | no | no | `--ask-daily-limit` per UTC day | `commercial_clean` only (unless `--any-source`) |

Tokens are stored hashed and shown once. `GET /api/whoami` reports the caller's scope;
`POST /api/ask` returns `asks_remaining_today` and answers only from sources the token may
cite — a client is never handed non-commercial data by accident. With no root token and no
stored tokens the API is open (it binds to loopback by default).

Integration recipe for a dashboard or agent: [docs/INTEGRATION.md](docs/INTEGRATION.md). Run unattended with the systemd user units in [deploy/](deploy/).

## Standing watch

```sh
enargeia watch --mission missions/ai-ecosystem.toml --interval-secs 3600 --quakes 2.5_day \
               --webhook https://example.invalid/hook      # or ENARGEIA_ALERT_WEBHOOK
```

Every cycle ingests the mission's sources, resolves, enriches a bounded number of items, and
writes `watch/digest-<time>.md`. Only escalations — high-consequence relation types
(`suing`, `acquired`, `ceo_of`, …), relations corroborated by more than one source, major
earthquakes, and non-seismic USGS events (explosions, blasts) — are posted to the webhook.
Routine digests stay on disk. `--once` runs a single cycle.

## Missions

```sh
enargeia mission run missions/ai-ecosystem.toml    # → missions/ai-ecosystem/BRIEF.md + graph.json
```

A mission is a TOML file: sources, a scope statement, and questions. The brief answers each
question from the graph with numbered source citations and appends what the graph knows.
Shipped missions: `ai-ecosystem` (labs and their relationships), `ai-leaders` (the people:
roles, moves, investors, disputes), `corruption-watch` (enforcement actions, audits and
investigative reporting — SEC, DOJ, GAO, OCCRP, ICIJ, ProPublica, Courthouse News, Bellingcat).
Every mission states its `target_class`: public actors in their professional capacity.

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

## Matching quality

Resolution uses blocking (name/token/phonetic keys) plus a multi-signal Fellegi–Sunter
scorer: name similarity, alias match, token overlap, phonetic agreement, type agreement,
extra-token and containment structure, acronym/initials logic, and — at runtime — co-mention
context, corroboration and recency (applied only to a review-worthy name match, and capped so
context can confirm a match but never carry one). Every candidate stores its feature breakdown. Measured on
labeled pairs from a real corpus; see [eval/REPORT.md](eval/REPORT.md) for the current
precision/recall against the single-metric baseline it replaced.

```sh
enargeia eval match      # regenerates eval/REPORT.md from eval/labels.jsonl
```

## Roadmap

1. ~~Zero-shot local NER (GLiNER via `gline-rs`) replacing the regex extractor.~~ Done.
2. ~~Multi-signal probabilistic matcher with a committed precision/recall report.~~ Done.
3. ~~Bi-temporal edges (`valid_at` / `invalid_at`) with contradiction supersession and `ask --as-of`.~~ Done.
4. ~~Reproducible decorrelation evaluation and a `why <entity>` provenance trace.~~ Done.
5. ~~HTTP API, geocoding, first live layer (CelesTrak), embedded globe.~~ Done.
6. ~~Standing watch cadence with an escalation-only alert channel; USGS seismic/explosion layer.~~ Done.
7. Next: key-registered live layers (NASA FIRMS, VesselAPI, FlightAware), scoped tokens for a
   second tenant, and larger local models for Tier 2.

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
