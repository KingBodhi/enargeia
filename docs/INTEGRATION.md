# Integrating Enargeia with another system

Enargeia is a sidecar: one process, one SQLite file, one HTTP surface. A dashboard, an
agent, or a client portal talks to it over `/api`; nothing else is shared. This is how the
PCG dashboard (or any tenant) plugs in without either codebase depending on the other.

```
┌──────────────┐   Bearer token    ┌──────────────────┐   feeds / gazetteers / live layers
│  dashboard   │ ───────────────▶  │  enargeia serve  │ ◀──────────────────────────────────
│  or agent    │ ◀───────────────  │  127.0.0.1:8787  │
└──────────────┘   JSON            └──────────────────┘
        ▲                                   │
        │  POST digest (escalations only)   │  enargeia watch --webhook <dashboard-url>
        └───────────────────────────────────┘
```

## 1. Provision a token for the tenant

```sh
enargeia token create --name pcg-dashboard --role client --ask-daily-limit 100
#   → prints enk_… once; store it in the tenant's secrets
```

A `client` token can read the graph and ask questions; it cannot record decisions or
decorrelations, it is quota-limited per UTC day, and its answers cite `commercial_clean`
sources only. Give a human analyst an `analyst` token instead; keep `operator` for the
people who run the engine. `GET /api/whoami` tells any caller what it holds.

## 2. Read the world model

| Need | Call |
|---|---|
| Health / counts | `GET /api/health`, `GET /api/status` |
| Search entities by name | `GET /api/entities?q=Anthropic&limit=20` |
| Recent live entities of a type | `GET /api/entities?type=organization&limit=50` |
| One entity with its relations and coordinates | `GET /api/entities/{id}` |
| Full provenance for an entity | `GET /api/entities/{id}/why` |
| Relations (bi-temporal fields included) | `GET /api/edges?limit=200` |
| Everything geocoded, as GeoJSON | `GET /api/geo` |
| Live satellites (SGP4, CelesTrak) | `GET /api/live/satellites?group=active&limit=3000` |
| Live seismic / explosion events (USGS) | `GET /api/live/quakes?feed=2.5_day` |

All GETs are open on the bound address; bind to loopback (default) and put the process
behind your own reverse proxy or VPN for anything beyond one machine.

## 3. Ask a question (sourced answer from the graph)

```sh
curl -s -X POST http://127.0.0.1:8787/api/ask \
  -H "Authorization: Bearer $ENK" -H 'Content-Type: application/json' \
  -d '{"question":"Who is suing OpenAI?","as_of":"2026-06-30"}'
```

Response fields: `answer` (prose with `[n]` citations followed by the numbered source list),
`llm` (which model answered), `principal`, `license_filter`, `asks_remaining_today`. A `429`
means the day's quota is spent; a `403` means the role may not do that.

## 4. Receive escalations from the standing watch

Run the watch with the tenant's webhook:

```sh
ENARGEIA_ALERT_WEBHOOK=https://dashboard.example/hooks/enargeia \
enargeia watch --mission missions/ai-ecosystem.toml --interval-secs 3600 --quakes 2.5_day
```

Only cycles with escalations POST. Payload:

```json
{
  "source": "enargeia",
  "kind": "watch_digest",
  "ended": "2026-09-09T15:49:22+00:00",
  "escalations": ["The New York Times —suing→ OpenAI (weight 2)", "USGS non-seismic event: …"],
  "new_typed_edges": 14,
  "review_pending": 2232,
  "digest_md": "# Watch digest — …"
}
```

The receiver decides what to do (a card, a message, a task); the full markdown digest for
every cycle is on disk under `watch/`.

## 5. Feed the engine from the other side (optional)

If the dashboard already collects content (e.g. a Pulse Engine SQLite database), point
`ENARGEIA_PULSE_DB_PATH` at it and `enargeia ingest` reads new rows read-only. Otherwise
give the mission file its own RSS/GDELT sources. Remember the license map in
[DATA-ATTRIBUTION.md](../DATA-ATTRIBUTION.md): what goes in decides what a client token may
be shown.

## What is deliberately not here

No shared database, no shared auth, no UI embedding. The globe at `/` is for operators and
analysts; a tenant that wants a map draws its own from `/api/geo` and the live layers.
