//! `enargeia eval match`: measures the matcher against `eval/labels.jsonl` and writes
//! `eval/REPORT.md`. Compares the v0.1 rule (auto-merge iff Jaro-Winkler ≥ 0.87) against
//! the probabilistic matcher at its default thresholds, and sweeps the merge threshold.
//! Only string/type features are exercised; DB-derived priors are zero here.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::matcher::{decide, score, Candidate, Decision, MatchScore, Weights};

#[derive(Debug, Deserialize)]
struct LabeledPair {
    mention: String,
    #[serde(default)]
    mention_type: Option<String>,
    candidate: String,
    #[serde(default)]
    candidate_type: String,
    #[serde(default)]
    candidate_aliases: Vec<String>,
    is_match: bool,
}

struct Scored<'a> {
    pair: &'a LabeledPair,
    ms: MatchScore,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Prf {
    pub tp: usize,
    pub fp: usize,
    pub fn_: usize,
    pub tn: usize,
}

impl Prf {
    pub fn precision(&self) -> f64 {
        if self.tp + self.fp == 0 {
            0.0
        } else {
            self.tp as f64 / (self.tp + self.fp) as f64
        }
    }
    pub fn recall(&self) -> f64 {
        if self.tp + self.fn_ == 0 {
            0.0
        } else {
            self.tp as f64 / (self.tp + self.fn_) as f64
        }
    }
    pub fn f1(&self) -> f64 {
        let p = self.precision();
        let r = self.recall();
        if p + r == 0.0 {
            0.0
        } else {
            2.0 * p * r / (p + r)
        }
    }
}

fn tally<'a>(rows: &[Scored<'a>], predict: impl Fn(&Scored<'a>) -> bool) -> Prf {
    let mut p = Prf::default();
    for r in rows {
        match (predict(r), r.pair.is_match) {
            (true, true) => p.tp += 1,
            (true, false) => p.fp += 1,
            (false, true) => p.fn_ += 1,
            (false, false) => p.tn += 1,
        }
    }
    p
}

pub struct EvalOutcome {
    pub pairs: usize,
    pub baseline: Prf,
    /// Merge decisions only.
    pub probabilistic: Prf,
    /// Merge-or-review: a true match that lands in review is caught by a human instead of
    /// silently duplicated, so this is the operationally relevant recall.
    pub merge_or_review: Prf,
    pub review_rate: f64,
    pub report_md: String,
}

pub fn run(labels_path: &Path, weights: &Weights) -> Result<EvalOutcome> {
    let text = std::fs::read_to_string(labels_path)
        .with_context(|| format!("reading {}", labels_path.display()))?;
    let pairs: Vec<LabeledPair> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, l)| serde_json::from_str(l).with_context(|| format!("labels line {}", i + 1)))
        .collect::<Result<_>>()?;

    let rows: Vec<Scored> = pairs
        .iter()
        .map(|p| {
            let cand = Candidate {
                canonical: &p.candidate,
                aliases: &p.candidate_aliases,
                entity_type: &p.candidate_type,
                cooc: 0.0,
                corroboration: 0,
                recent: false,
            };
            Scored {
                pair: p,
                ms: score(&p.mention, p.mention_type.as_deref(), &cand, weights),
            }
        })
        .collect();

    let positives = pairs.iter().filter(|p| p.is_match).count();
    let baseline = tally(&rows, |r| r.ms.features.name_jw >= 0.87);
    let probabilistic = tally(&rows, |r| decide(r.ms.total, weights) == Decision::Merge);
    let merge_or_review = tally(&rows, |r| decide(r.ms.total, weights) != Decision::New);
    let reviews = rows
        .iter()
        .filter(|r| decide(r.ms.total, weights) == Decision::Review)
        .count();
    let review_rate = reviews as f64 / rows.len().max(1) as f64;

    let mut sweep: Vec<(f64, Prf)> = Vec::new();
    let mut t = 0.0;
    while t <= 12.0 {
        sweep.push((t, tally(&rows, |r| r.ms.total >= t)));
        t += 0.5;
    }
    // Operating point: auto-merge must be safe, so prefer the highest-F1 threshold among
    // those with precision ≥ 0.85 (fall back to best F1 overall).
    let safe_best = sweep
        .iter()
        .filter(|(_, p)| p.precision() >= 0.85)
        .max_by(|a, b| a.1.f1().partial_cmp(&b.1.f1()).unwrap());
    let overall_best = sweep
        .iter()
        .max_by(|a, b| a.1.f1().partial_cmp(&b.1.f1()).unwrap());

    let mut md = String::new();
    md.push_str("# Matcher evaluation report\n\n");
    md.push_str(&format!(
        "Generated {} by `enargeia eval match` from `{}`.\n\n",
        chrono::Utc::now().format("%Y-%m-%d %H:%M UTC"),
        labels_path.display()
    ));
    md.push_str(&format!(
        "**{} labeled pairs** — {} matches, {} non-matches. Only string/type features are \
         exercised here; co-occurrence, corroboration and recency priors are zero. See \
         `eval/README.md` for the labeling policy and provenance.\n\n",
        pairs.len(),
        positives,
        pairs.len() - positives
    ));

    md.push_str("## Headline\n\n| Matcher | Precision | Recall | F1 | Auto-merges |\n|---|---|---|---|---|\n");
    md.push_str(&format!(
        "| v0.1 rule: Jaro-Winkler ≥ 0.87 → merge | {:.3} | {:.3} | {:.3} | {} |\n",
        baseline.precision(),
        baseline.recall(),
        baseline.f1(),
        baseline.tp + baseline.fp
    ));
    md.push_str(&format!(
        "| Probabilistic: total ≥ {:.1} → merge | {:.3} | {:.3} | {:.3} | {} |\n",
        weights.upper,
        probabilistic.precision(),
        probabilistic.recall(),
        probabilistic.f1(),
        probabilistic.tp + probabilistic.fp
    ));
    md.push_str(&format!(
        "| Probabilistic: merge **or review** (≥ {:.1}) | {:.3} | {:.3} | {:.3} | {} |\n\n",
        weights.lower,
        merge_or_review.precision(),
        merge_or_review.recall(),
        merge_or_review.f1(),
        merge_or_review.tp + merge_or_review.fp
    ));
    md.push_str(&format!(
        "Auto-merge precision is the safety number: a wrong merge silently corrupts the graph. \
         Merge-or-review recall is the coverage number: a true match that reaches review is fixed by a \
         human rather than duplicated. At the defaults {:.1}% of pairs land in review.\n\n",
        review_rate * 100.0
    ));
    if let Some((t, p)) = safe_best {
        md.push_str(&format!(
            "Best merge threshold with precision ≥ 0.85 on this sample: **{t:.1}** (P {:.3}, R {:.3}, F1 {:.3}).",
            p.precision(),
            p.recall(),
            p.f1()
        ));
    }
    if let Some((t, p)) = overall_best {
        md.push_str(&format!(
            " Best F1 regardless of precision: {:.3} at {t:.1} (P {:.3}, R {:.3}).\n\n",
            p.f1(),
            p.precision(),
            p.recall()
        ));
    }

    md.push_str("## Threshold sweep (positive iff total ≥ t)\n\n| t | P | R | F1 | TP | FP | FN |\n|---|---|---|---|---|---|---|\n");
    for (t, p) in &sweep {
        md.push_str(&format!(
            "| {:.1} | {:.3} | {:.3} | {:.3} | {} | {} | {} |\n",
            t,
            p.precision(),
            p.recall(),
            p.f1(),
            p.tp,
            p.fp,
            p.fn_
        ));
    }

    let fmt_contrib = |ms: &MatchScore| {
        ms.contributions
            .iter()
            .map(|(k, v)| format!("{k} {v:+.1}"))
            .collect::<Vec<_>>()
            .join(", ")
    };

    md.push_str("\n## False positives at default (merged, but labeled non-match)\n\n");
    let mut fps: Vec<&Scored> = rows
        .iter()
        .filter(|r| !r.pair.is_match && decide(r.ms.total, weights) == Decision::Merge)
        .collect();
    fps.sort_by(|a, b| b.ms.total.partial_cmp(&a.ms.total).unwrap());
    if fps.is_empty() {
        md.push_str("_none_\n");
    }
    for r in fps {
        md.push_str(&format!(
            "- **{}** → {} — total {:.1} ({})\n",
            r.pair.mention,
            r.pair.candidate,
            r.ms.total,
            fmt_contrib(&r.ms)
        ));
    }

    md.push_str("\n## False negatives at default (labeled match, but rejected as new)\n\n");
    let mut fns: Vec<&Scored> = rows
        .iter()
        .filter(|r| r.pair.is_match && decide(r.ms.total, weights) == Decision::New)
        .collect();
    fns.sort_by(|a, b| a.ms.total.partial_cmp(&b.ms.total).unwrap());
    if fns.is_empty() {
        md.push_str("_none_\n");
    }
    for r in fns {
        md.push_str(&format!(
            "- **{}** → {} — total {:.1} ({})\n",
            r.pair.mention,
            r.pair.candidate,
            r.ms.total,
            fmt_contrib(&r.ms)
        ));
    }

    md.push_str("\n## Matches sent to review at default\n\n");
    let mut revs: Vec<&Scored> = rows
        .iter()
        .filter(|r| r.pair.is_match && decide(r.ms.total, weights) == Decision::Review)
        .collect();
    revs.sort_by(|a, b| a.ms.total.partial_cmp(&b.ms.total).unwrap());
    if revs.is_empty() {
        md.push_str("_none_\n");
    }
    for r in revs {
        md.push_str(&format!(
            "- {} → {} — total {:.1}\n",
            r.pair.mention, r.pair.candidate, r.ms.total
        ));
    }

    md.push_str("\n## Non-matches sent to review at default\n\n");
    let mut nrevs: Vec<&Scored> = rows
        .iter()
        .filter(|r| !r.pair.is_match && decide(r.ms.total, weights) == Decision::Review)
        .collect();
    nrevs.sort_by(|a, b| b.ms.total.partial_cmp(&a.ms.total).unwrap());
    if nrevs.is_empty() {
        md.push_str("_none_\n");
    }
    for r in nrevs {
        md.push_str(&format!(
            "- {} → {} — total {:.1}\n",
            r.pair.mention, r.pair.candidate, r.ms.total
        ));
    }

    md.push_str("\n## Weights used\n\n```json\n");
    md.push_str(&serde_json::to_string_pretty(weights)?);
    md.push_str("\n```\n");

    Ok(EvalOutcome {
        pairs: pairs.len(),
        baseline,
        probabilistic,
        merge_or_review,
        review_rate,
        report_md: md,
    })
}
