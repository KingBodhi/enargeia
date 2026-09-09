# Matcher evaluation

`labels.jsonl` holds (mention, candidate) pairs with a ground-truth `is_match`. Run:

```sh
enargeia eval match            # reads eval/labels.jsonl, writes eval/REPORT.md
```

## Labeling policy

- **true** — same real-world entity: spelling/spacing/punctuation variants, handles
  (`rebecca.bellan`), corporate-suffix variants (`Mistral` / `Mistral AI`), surname-only
  references to a named person, acronym ↔ expansion.
- **false** — different entity even when the strings are close: parent/division/product
  (`Google` / `Google Cloud`, `Claude` / `Claude Code`), event vs. organizer, similar-looking
  names (`CoinShares` / `Coinbase`), ambiguous first names (`Anthony` / `Anthony Ha`),
  ambiguous geography (`Washington` / `Washington, D.C.`), exact string with conflicting
  types (`Mercury` product vs. location).

## Provenance

The first 200-odd pairs are the highest-scoring Jaro-Winkler pairs produced by the v0.1
resolver on a real 49-article corpus (TechCrunch AI + Cointelegraph, 2026-09-08); the
remainder are constructed hard cases. Labels were assigned by the maintainers' assistant
model and should be audited — disagreements welcome as pull requests. The eval only uses
string/type features; the DB-derived priors (co-occurrence, corroboration, recency) are
applied at runtime and are not measured here.
