# Data attribution and license classes

Every source item Enargeia stores carries a `license_class`, asserted by the adapter that
ingested it — never assumed. This file is the human-readable side of that field.

| `license_class` | Meaning |
|---|---|
| `commercial_clean` | The source's published terms permit commercial use of the data. |
| `non_commercial_only` | Usable for research/personal work only. Enargeia will not ship an adapter for these by default. |
| `unknown` | Terms vary per publisher or were not asserted. Treat as non-commercial until verified. |

## Adapters in this release

| Adapter | Source | Class | Terms / attribution |
|---|---|---|---|
| `rss` | Arbitrary RSS/Atom feeds | `unknown` | Terms are set by each publisher. Verify before commercial use. |
| `gdelt` | [GDELT Project](https://www.gdeltproject.org/) DOC 2.0 API | `commercial_clean` | GDELT permits commercial use and redistribution with attribution. Enargeia observes the documented rate limit (one request per 5 seconds). Article text is copyright its publisher; the API returns metadata. |
| `pulse` (optional) | A local Pulse Engine SQLite database | `unknown` | Internal/operator-provided; terms follow the upstream sources. |

## Live layers and reference data in this release

| Source | Class | Terms / attribution |
|---|---|---|
| [CelesTrak](https://celestrak.org/) | `commercial_clean` | General-perturbation element sets (OMM JSON), propagated locally with SGP4 for the satellite layer. Element sets are cached for hours per group, per CelesTrak's request. Data © CelesTrak. |
| [GeoNames](https://www.geonames.org/) | `commercial_clean` | `cities15000` and `countryInfo` under CC BY 4.0 — "This work is licensed under a Creative Commons Attribution 4.0 License; data © GeoNames." Used only for geocoding location entities. |

## Planned adapters (documented here before they land)

| Source | Expected class | Notes |
|---|---|---|
| [VesselAPI](https://vesselapi.com/) | `commercial_clean` | Free tier; per their terms. |
| [FlightAware AeroAPI](https://www.flightaware.com/commercial/aeroapi/) | `commercial_clean` (paid) | Metered commercial API. |
| Space-Track.org | `commercial_clean` with a caveat | Redistribution of derived analysis to third parties requires approval. |

Sources deliberately **not** supported because their terms prohibit competitive/commercial
products: ACLED, GTD, Cloudflare Radar, Global Fishing Watch, AISHub, OpenSky (without a
written license).

## Models

| Model | License | Notes |
|---|---|---|
| GLiNER v2.x ONNX exports (e.g. `gliner_small-v2.1`) | Apache-2.0 | Used for zero-shot NER. **Do not** use `gliner_base` (CC-BY-NC-4.0). |
| Local LLMs via Ollama/vLLM | per model | Enrichment output is never stored as fact without passing the same matcher as every other mention. |
