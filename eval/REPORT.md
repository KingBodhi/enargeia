# Matcher evaluation report

Generated 2026-09-09 04:24 UTC by `enargeia eval match` from `eval/labels.jsonl`.

**261 labeled pairs** — 50 matches, 211 non-matches. Only string/type features are exercised here; co-occurrence, corroboration and recency priors are zero. See `eval/README.md` for the labeling policy and provenance.

## Headline

| Matcher | Precision | Recall | F1 | Auto-merges |
|---|---|---|---|---|
| v0.1 rule: Jaro-Winkler ≥ 0.87 → merge | 0.371 | 0.660 | 0.475 | 89 |
| Probabilistic: total ≥ 5.0 → merge | 0.829 | 0.580 | 0.682 | 35 |
| Probabilistic: merge **or review** (≥ 1.0) | 0.644 | 0.940 | 0.764 | 73 |

Auto-merge precision is the safety number: a wrong merge silently corrupts the graph. Merge-or-review recall is the coverage number: a true match that reaches review is fixed by a human rather than duplicated. At the defaults 14.6% of pairs land in review.

Best merge threshold with precision ≥ 0.85 on this sample: **6.5** (P 0.903, R 0.560, F1 0.691). Best F1 regardless of precision: 0.764 at 1.0 (P 0.644, R 0.940).

## Threshold sweep (positive iff total ≥ t)

| t | P | R | F1 | TP | FP | FN |
|---|---|---|---|---|---|---|
| 0.0 | 0.644 | 0.940 | 0.764 | 47 | 26 | 3 |
| 0.5 | 0.644 | 0.940 | 0.764 | 47 | 26 | 3 |
| 1.0 | 0.644 | 0.940 | 0.764 | 47 | 26 | 3 |
| 1.5 | 0.634 | 0.900 | 0.744 | 45 | 26 | 5 |
| 2.0 | 0.652 | 0.860 | 0.741 | 43 | 23 | 7 |
| 2.5 | 0.672 | 0.860 | 0.754 | 43 | 21 | 7 |
| 3.0 | 0.672 | 0.860 | 0.754 | 43 | 21 | 7 |
| 3.5 | 0.667 | 0.720 | 0.692 | 36 | 18 | 14 |
| 4.0 | 0.654 | 0.680 | 0.667 | 34 | 18 | 16 |
| 4.5 | 0.667 | 0.680 | 0.673 | 34 | 17 | 16 |
| 5.0 | 0.829 | 0.580 | 0.682 | 29 | 6 | 21 |
| 5.5 | 0.875 | 0.560 | 0.683 | 28 | 4 | 22 |
| 6.0 | 0.875 | 0.560 | 0.683 | 28 | 4 | 22 |
| 6.5 | 0.903 | 0.560 | 0.691 | 28 | 3 | 22 |
| 7.0 | 0.957 | 0.440 | 0.603 | 22 | 1 | 28 |
| 7.5 | 0.944 | 0.340 | 0.500 | 17 | 1 | 33 |
| 8.0 | 0.944 | 0.340 | 0.500 | 17 | 1 | 33 |
| 8.5 | 0.944 | 0.340 | 0.500 | 17 | 1 | 33 |
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
- **Microsoft** → Microsoft Research — total 6.5 (name_jw +2.5, token_jaccard +2.0, phonetic +1.0, type_agree +1.0)
- **New York** → New York City — total 6.0 (name_jw +2.5, token_jaccard +1.0, phonetic +0.5, extra_tokens -1.5, token_subset +2.5, type_agree +1.0)
- **Mount Shasta ranger station** → Mount Shasta — total 5.0 (name_jw +2.5, token_jaccard +1.0, phonetic +0.5, extra_tokens -2.5, token_subset +2.5, type_agree +1.0)
- **Goldman Sachs** → Goldman Sachs Asset Management — total 5.0 (name_jw +2.5, token_jaccard +1.0, phonetic +0.5, extra_tokens -2.5, token_subset +2.5, type_agree +1.0)

## False negatives at default (labeled match, but rejected as new)

- **Butterfill** → James Butterfil — total -2.5 (name_jw -0.5, token_jaccard -1.0, phonetic +0.5, extra_tokens -2.5, type_agree +1.0)
- **Washington, DC** → Washington, D.C. — total -0.5 (name_jw +2.5, token_jaccard -1.0, phonetic -0.5, extra_tokens -2.5, type_agree +1.0)
- **JP Morgan** → JPMorgan Chase — total -0.5 (name_jw +2.5, token_jaccard -1.0, phonetic -0.5, extra_tokens -2.5, type_agree +1.0)

## Matches sent to review at default

- Open AI → OpenAI — total 1.0
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
- frontier AI companies → frontier AI labs — total 4.5
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
- major companies → AI companies — total 3.0
- tech companies → AI companies — total 3.0
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
  "upper": 5.0,
  "lower": 1.0
}
```
