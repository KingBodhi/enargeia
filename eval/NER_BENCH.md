# Tier 1 extraction: fp32 vs int8 GLiNER (2026-09-09)

Same 40 real articles (avg 5,055 chars), fresh database per run, `enargeia resolve`,
`gliner_small-v2.1` ONNX exports on CPU (Ryzen-class desktop, while a Tier 2 job shared the
machine — absolute numbers are indicative, the comparison is fair).

| Export | Time | Throughput | Distinct mentions | Entities | Auto-merged | Avg confidence |
|---|---|---|---|---|---|---|
| `model.onnx` (fp32, 611 MB) | 58.0 s | 0.69 items/s | 393 | 340 | 163 | 0.766 |
| `model_int8.onnx` (183 MB) | 41.2 s | 0.97 items/s | 283 | 248 | 102 | 0.716 |

Mention overlap (case-folded surface forms): 267 shared, Jaccard 0.65. The int8 export drops
real entities ("Amazon", "Anthropic settlement", "4YFN") and adds generic spans ("one person",
"regulated workspace").

**Decision:** fp32 stays the default. A 1.4× speedup is not worth a 28% recall loss at the
first hop, where anything missed never reaches the matcher. `ENARGEIA_NER_ONNX=model_int8.onnx`
remains available for memory-constrained hosts (`enargeia models fetch --int8`). The real
throughput lever is elsewhere: chunk batching and GPU execution providers for ORT.
