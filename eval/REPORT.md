# Matcher evaluation report

Generated 2026-09-09 18:23 UTC by `enargeia eval match` from `eval/labels.jsonl`.

**261 labeled pairs** — 50 matches, 211 non-matches. Only string/type features are exercised here; co-occurrence, corroboration and recency priors are zero. See `eval/README.md` for the labeling policy and provenance.

## Headline

| Matcher | Precision | Recall | F1 | Auto-merges |
|---|---|---|---|---|
| v0.1 rule: Jaro-Winkler ≥ 0.87 → merge | 0.371 | 0.660 | 0.475 | 89 |
| Probabilistic: total ≥ 5.0 → merge | 0.821 | 0.640 | 0.719 | 39 |
| Probabilistic: merge **or review** (≥ 1.0) | 0.671 | 0.980 | 0.797 | 73 |

Auto-merge precision is the safety number: a wrong merge silently corrupts the graph. Merge-or-review recall is the coverage number: a true match that reaches review is fixed by a human rather than duplicated. At the defaults 13.0% of pairs land in review.

Best merge threshold with precision ≥ 0.85 on this sample: **6.5** (P 0.886, R 0.620, F1 0.729). Best F1 regardless of precision: 0.800 at 3.0 (P 0.708, R 0.920).

## Threshold sweep (positive iff total ≥ t)

| t | P | R | F1 | TP | FP | FN |
|---|---|---|---|---|---|---|
| 0.0 | 0.671 | 0.980 | 0.797 | 49 | 24 | 1 |
| 0.5 | 0.671 | 0.980 | 0.797 | 49 | 24 | 1 |
| 1.0 | 0.671 | 0.980 | 0.797 | 49 | 24 | 1 |
| 1.5 | 0.667 | 0.960 | 0.787 | 48 | 24 | 2 |
| 2.0 | 0.687 | 0.920 | 0.786 | 46 | 21 | 4 |
| 2.5 | 0.708 | 0.920 | 0.800 | 46 | 19 | 4 |
| 3.0 | 0.708 | 0.920 | 0.800 | 46 | 19 | 4 |
| 3.5 | 0.684 | 0.780 | 0.729 | 39 | 18 | 11 |
| 4.0 | 0.673 | 0.740 | 0.705 | 37 | 18 | 13 |
| 4.5 | 0.685 | 0.740 | 0.712 | 37 | 17 | 13 |
| 5.0 | 0.821 | 0.640 | 0.719 | 32 | 7 | 18 |
| 5.5 | 0.861 | 0.620 | 0.721 | 31 | 5 | 19 |
| 6.0 | 0.861 | 0.620 | 0.721 | 31 | 5 | 19 |
| 6.5 | 0.886 | 0.620 | 0.729 | 31 | 4 | 19 |
| 7.0 | 0.962 | 0.500 | 0.658 | 25 | 1 | 25 |
| 7.5 | 0.952 | 0.400 | 0.563 | 20 | 1 | 30 |
| 8.0 | 0.952 | 0.400 | 0.563 | 20 | 1 | 30 |
| 8.5 | 0.952 | 0.400 | 0.563 | 20 | 1 | 30 |
| 9.0 | 0.941 | 0.320 | 0.478 | 16 | 1 | 34 |
| 9.5 | 0.941 | 0.320 | 0.478 | 16 | 1 | 34 |
| 10.0 | 0.941 | 0.320 | 0.478 | 16 | 1 | 34 |
| 10.5 | 0.941 | 0.320 | 0.478 | 16 | 1 | 34 |
| 11.0 | 0.941 | 0.320 | 0.478 | 16 | 1 | 34 |
| 11.5 | 0.941 | 0.320 | 0.478 | 16 | 1 | 34 |
| 12.0 | 0.000 | 0.000 | 0.000 | 0 | 0 | 50 |

## False positives at default (merged, but labeled non-match)

- **Google** → Google Cloud — total 11.5 (name_jw +4.5, alias_exact +3.0, token_jaccard +2.0, phonetic +1.0, type_agree +1.0)
- **frontier lab** → frontier AI labs — total 6.5 (name_jw +2.5, token_jaccard +2.0, phonetic +1.0, type_agree +1.0)
- **frontier AI companies** → frontier AI labs — total 6.5 (name_jw +2.5, token_jaccard +2.0, phonetic +1.0, type_agree +1.0)
- **Microsoft** → Microsoft Research — total 6.5 (name_jw +2.5, token_jaccard +2.0, phonetic +1.0, type_agree +1.0)
- **New York** → New York City — total 6.0 (name_jw +2.5, token_jaccard +1.0, phonetic +0.5, extra_tokens -1.5, token_subset +2.5, type_agree +1.0)
- **Mount Shasta ranger station** → Mount Shasta — total 5.0 (name_jw +2.5, token_jaccard +1.0, phonetic +0.5, extra_tokens -2.5, token_subset +2.5, type_agree +1.0)
- **Goldman Sachs** → Goldman Sachs Asset Management — total 5.0 (name_jw +2.5, token_jaccard +1.0, phonetic +0.5, extra_tokens -2.5, token_subset +2.5, type_agree +1.0)

## False negatives at default (labeled match, but rejected as new)

- **Butterfill** → James Butterfil — total -2.5 (name_jw -0.5, token_jaccard -1.0, phonetic +0.5, extra_tokens -2.5, type_agree +1.0)

## Matches sent to review at default

- Recorded Futur → Recorded Future — total 1.0
- Mosaic → Mosaic Asset Company — total 1.5
- Meta Platforms Inc. → Meta — total 1.5
- US Federal Reserve → Federal Reserve — total 3.0
- Steinhardt → Jacob Steinhardt — total 3.0
- Mallers → Jack Mallers — total 3.0
- Amodei → Dario Amodei — total 3.0
- Altman → Sam Altman — total 3.0
- Kalanick → Travis Kalanick — total 3.0
- Zuckerberg → Mark Zuckerberg — total 3.0
- The startup community → startup community — total 3.5
- Nvidia Corporation → Nvidia — total 3.5
- Astra → Astra model — total 4.5
- Hegotá → Hegotá upgrade — total 4.5
- Coinbase Global → Coinbase — total 4.5
- Amazon.com → Amazon — total 4.5
- Goldman → Goldman Sachs — total 4.5

## Non-matches sent to review at default

- Anthony → Anthony Ha — total 4.5
- ChatGPT → ChatGPT Atlas — total 4.5
- Rebecca → Rebecca Bellan — total 4.5
- Zcash Trust → Zcash — total 4.5
- OpenAI-affiliated → OpenAI — total 4.5
- Liquid Federation → Liquid — total 4.5
- Google DeepMind → Google Cloud — total 4.5
- Circle → Circle K — total 4.5
- Mercury → Mercury — total 4.5
- Delta → Delta — total 4.5
- Ethereum → Ethereum Foundation — total 4.0
- incident → wiki incident — total 3.0
- Gemini → Gemini Spark — total 2.0
- Alphabet → Alphabet Soup — total 2.0
- Washington → Washington, D.C. — total 1.5
- New York state → New York City — total 1.5
- Apple Inc → Apple Records — total 1.5

## Weights used

```json
{
  "name": [
    -3.0,
    -0.5,
    2.5,
    4.5
  ],
  "alias_exact": 3.0,
  "jaccard": [
    -1.0,
    1.0,
    2.0
  ],
  "phonetic": [
    -0.5,
    0.5,
    1.0
  ],
  "type_agree": [
    -6.0,
    -1.5,
    0.0,
    1.0
  ],
  "extra_tokens": [
    0.0,
    -1.5,
    -2.5
  ],
  "token_subset": [
    0.0,
    1.0,
    2.5
  ],
  "acronym_match": 6.0,
  "acronym_mismatch": -3.0,
  "cooc": [
    0.0,
    1.5,
    2.5
  ],
  "corroboration": [
    0.0,
    0.3,
    0.7,
    1.0
  ],
  "recent": 0.3,
  "prior_cap": 2.0,
  "upper": 5.0,
  "lower": 1.0
}
```
