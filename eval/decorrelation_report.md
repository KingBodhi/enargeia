# Decorrelation evaluation

Generated 2026-09-09 04:40 UTC by `enargeia eval decorrelation` on a scratch database (regex extractor).

## Step 1 — initial resolution

3 items, 2 auto-merged mentions, 3 new entities, 0 review.

- [PASS] both 'Mistral' mentions auto-merged into 'Mistral AI' before any human decision (sanity)

  s2: Some(("auto_merged", Some("27a96075-9cdb-4a80-885d-68679028d0e8"), Some(5.8)))
  s3: Some(("auto_merged", Some("27a96075-9cdb-4a80-885d-68679028d0e8"), Some(6.1)))

## Step 2 — human rejects the s2 merge

created 'Mistral' (ae8646e9-b82b-47bd-bbaa-335c7d1affab); decorrelated from 27a96075-9cdb-4a80-885d-68679028d0e8

- [PASS] reject created a distinct entity for the mention
- [PASS] the new entity and 'Mistral AI' are decorrelated
## Step 3 — full re-resolution with the decorrelation in place

3 items, 5 auto-merged, 0 new, 0 review.

  s2: Some(("auto_merged", Some("ae8646e9-b82b-47bd-bbaa-335c7d1affab"), Some(10.8)))
  s3: Some(("auto_merged", Some("ae8646e9-b82b-47bd-bbaa-335c7d1affab"), Some(11.100000000000001)))

- [PASS] s2: 'Mistral' did NOT auto-merge back into 'Mistral AI'
- [PASS] s2: resolved to the human-created entity or sent to review
- [PASS] s3: 'Mistral' did NOT auto-merge back into 'Mistral AI'
- [PASS] s3: resolved to the human-created entity or sent to review
- [PASS] merge(Mistral AI ← new) refused
- [PASS] merge(new ← Mistral AI) refused
## Step 4 — merge guard

- refusing merge: 27a96075-9cdb-4a80-885d-68679028d0e8 and ae8646e9-b82b-47bd-bbaa-335c7d1affab were previously decorrelated by a human decision
- refusing merge: ae8646e9-b82b-47bd-bbaa-335c7d1affab and 27a96075-9cdb-4a80-885d-68679028d0e8 were previously decorrelated by a human decision

## Result: PASS
