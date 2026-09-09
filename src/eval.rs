//! Evaluations that ship with the repo.
//!
//! - `enargeia eval match`: measures the matcher against `eval/labels.jsonl` and writes
//!   `eval/REPORT.md`. Compares the v0.1 rule (auto-merge iff Jaro-Winkler ≥ 0.87) against the
//!   probabilistic matcher at its default thresholds, and sweeps the merge threshold. Only
//!   string/type features are exercised; DB-derived priors are zero here.
//! - `enargeia eval decorrelation`: proves the sticky-decorrelation guarantee end to end on a
//!   scratch database and writes `eval/decorrelation_report.md`.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use sqlx::SqlitePool;

use crate::{
    matcher::{decide, score, Candidate, Decision, MatchScore, Weights},
    models::RawItem,
    resolve,
};

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
            let mut ms = score(&p.mention, p.mention_type.as_deref(), &cand, weights);
            // The pipeline never scores a mention that fails the name gate; the eval
            // must not either, or gate-rejected junk shows up as matcher errors.
            let mtype = p.mention_type.as_deref().unwrap_or(&p.candidate_type);
            if !crate::extract::plausible_name(&p.mention, mtype) {
                ms.total = -10.0;
                ms.contributions.push(("name_gate".to_string(), -10.0));
            }
            Scored { pair: p, ms }
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

    // (title, labeled as match?, decision bucket, ascending order, show evidence)
    let sections: [(&str, bool, Decision, bool, bool); 4] = [
        (
            "False positives at default (merged, but labeled non-match)",
            false,
            Decision::Merge,
            false,
            true,
        ),
        (
            "False negatives at default (labeled match, but rejected as new)",
            true,
            Decision::New,
            true,
            true,
        ),
        (
            "Matches sent to review at default",
            true,
            Decision::Review,
            true,
            false,
        ),
        (
            "Non-matches sent to review at default",
            false,
            Decision::Review,
            false,
            false,
        ),
    ];
    for (title, is_match, bucket, asc, detail) in sections {
        md.push_str(&format!("\n## {title}\n\n"));
        let mut rs: Vec<&Scored> = rows
            .iter()
            .filter(|r| r.pair.is_match == is_match && decide(r.ms.total, weights) == bucket)
            .collect();
        rs.sort_by(|a, b| {
            let o = a.ms.total.partial_cmp(&b.ms.total).unwrap();
            if asc {
                o
            } else {
                o.reverse()
            }
        });
        if rs.is_empty() {
            md.push_str("_none_\n");
        }
        for r in rs {
            if detail {
                md.push_str(&format!(
                    "- **{}** → {} — total {:.1} ({})\n",
                    r.pair.mention,
                    r.pair.candidate,
                    r.ms.total,
                    fmt_contrib(&r.ms)
                ));
            } else {
                md.push_str(&format!(
                    "- {} → {} — total {:.1}\n",
                    r.pair.mention, r.pair.candidate, r.ms.total
                ));
            }
        }
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

// ---------------------------------------------------------------------------------------------
// Decorrelation evaluation

pub struct DecorrelationOutcome {
    pub passed: bool,
    pub report_md: String,
}

fn raw(id: &str, text: &str, date: &str) -> RawItem {
    RawItem {
        source_type: "eval".to_string(),
        source_ref: format!("eval://{id}"),
        title: Some(id.to_string()),
        text: text.to_string(),
        license_class: "commercial_clean".to_string(),
        published_at: Some(format!("{date}T00:00:00+00:00")),
    }
}

type CandidateState = (String, Option<String>, Option<f64>);

async fn candidate_status(
    pool: &SqlitePool,
    source_ref: &str,
    mention: &str,
) -> Result<Option<CandidateState>> {
    Ok(sqlx::query_as(
        "SELECT c.status, c.best_match_entity_id, c.match_score FROM wm_extraction_candidates c \
         JOIN wm_source_items s ON s.id = c.source_item_id WHERE s.source_ref = ? AND c.mention_text = ? \
         ORDER BY c.created_at DESC LIMIT 1",
    )
    .bind(source_ref)
    .bind(mention)
    .fetch_optional(pool)
    .await?)
}

fn check(md: &mut String, passed: &mut bool, ok: bool, what: &str) {
    md.push_str(&format!(
        "- [{}] {what}\n",
        if ok { "PASS" } else { "FAIL" }
    ));
    *passed &= ok;
}

/// Runs on a scratch database. Uses the regex extractor so the result does not depend on a
/// downloaded model. Steps:
/// 1. Ingest three items; "Mistral" mentions auto-merge into the "Mistral AI" entity.
/// 2. A human rejects one of those merges → a new entity is created and the pair is decorrelated.
/// 3. The whole corpus is re-resolved from scratch (entities and decorrelations kept).
/// 4. Assert: no "Mistral" mention auto-merges into "Mistral AI" again; `merge` refuses both ways.
pub async fn run_decorrelation(pool: &SqlitePool) -> Result<DecorrelationOutcome> {
    std::env::set_var("ENARGEIA_EXTRACTOR", "regex");
    let mut md = String::new();
    let mut passed = true;

    md.push_str("# Decorrelation evaluation\n\n");
    md.push_str(&format!(
        "Generated {} by `enargeia eval decorrelation` on a scratch database (regex extractor).\n\n",
        chrono::Utc::now().format("%Y-%m-%d %H:%M UTC")
    ));

    for (id, text, date) in [
        (
            "s1",
            "Mistral AI raised new funding from investors in Paris.",
            "2026-01-01",
        ),
        (
            "s2",
            "Mistral said the new model is open to everyone.",
            "2026-02-01",
        ),
        (
            "s3",
            "Mistral announced a partnership with Nvidia.",
            "2026-03-01",
        ),
    ] {
        resolve::ingest_item(pool, &raw(id, text, date)).await?;
    }
    let stats = resolve::resolve_pending(pool, 100).await?;
    md.push_str(&format!(
        "## Step 1 — initial resolution\n\n{} items, {} auto-merged mentions, {} new entities, {} review.\n\n",
        stats.items_processed, stats.auto_merged, stats.new_entities, stats.pending_review
    ));
    let e: Option<(String,)> =
        sqlx::query_as("SELECT id FROM wm_entities WHERE canonical_name = 'Mistral AI'")
            .fetch_optional(pool)
            .await?;
    let Some((e_id,)) = e else {
        check(
            &mut md,
            &mut passed,
            false,
            "entity 'Mistral AI' exists after resolution",
        );
        return Ok(DecorrelationOutcome {
            passed: false,
            report_md: md,
        });
    };
    let s2 = candidate_status(pool, "eval://s2", "Mistral").await?;
    let s3 = candidate_status(pool, "eval://s3", "Mistral").await?;
    let merged_initially = matches!(
        (&s2, &s3),
        (Some((st2, Some(b2), _)), Some((st3, Some(b3), _)))
            if st2 == "auto_merged" && st3 == "auto_merged" && *b2 == e_id && *b3 == e_id
    );
    check(
        &mut md,
        &mut passed,
        merged_initially,
        "both 'Mistral' mentions auto-merged into 'Mistral AI' before any human decision (sanity)",
    );
    md.push_str(&format!("\n  s2: {s2:?}\n  s3: {s3:?}\n\n"));

    // Step 2 — human rejects the s2 merge.
    let (cand_id,): (String,) = sqlx::query_as(
        "SELECT c.id FROM wm_extraction_candidates c JOIN wm_source_items s ON s.id = c.source_item_id \
         WHERE s.source_ref = 'eval://s2' AND c.mention_text = 'Mistral' ORDER BY c.created_at DESC LIMIT 1",
    )
    .fetch_one(pool)
    .await?;
    let msg = resolve::apply_decision(
        pool,
        &cand_id,
        "reject",
        "eval",
        Some("not the same company"),
    )
    .await?;
    md.push_str(&format!(
        "## Step 2 — human rejects the s2 merge\n\n{msg}\n\n"
    ));
    let (n_id,): (String,) =
        sqlx::query_as("SELECT best_match_entity_id FROM wm_extraction_candidates WHERE id = ?")
            .bind(&cand_id)
            .fetch_one(pool)
            .await?;
    check(
        &mut md,
        &mut passed,
        n_id != e_id,
        "reject created a distinct entity for the mention",
    );
    let decorrelated = resolve::is_decorrelated(pool, &n_id, &e_id).await?;
    check(
        &mut md,
        &mut passed,
        decorrelated,
        "the new entity and 'Mistral AI' are decorrelated",
    );

    // Step 3 — re-resolve the whole corpus from scratch (entities + decorrelations persist).
    sqlx::query("DELETE FROM wm_extraction_candidates")
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM wm_edges").execute(pool).await?;
    sqlx::query("UPDATE wm_source_items SET status = 'pending'")
        .execute(pool)
        .await?;
    let stats = resolve::resolve_pending(pool, 100).await?;
    md.push_str(&format!(
        "## Step 3 — full re-resolution with the decorrelation in place\n\n{} items, {} auto-merged, {} new, {} review.\n\n",
        stats.items_processed, stats.auto_merged, stats.new_entities, stats.pending_review
    ));
    let s2 = candidate_status(pool, "eval://s2", "Mistral").await?;
    let s3 = candidate_status(pool, "eval://s3", "Mistral").await?;
    md.push_str(&format!("  s2: {s2:?}\n  s3: {s3:?}\n\n"));
    for (label, st) in [("s2", &s2), ("s3", &s3)] {
        let re_merged =
            matches!(st, Some((status, Some(b), _)) if status == "auto_merged" && *b == e_id);
        check(
            &mut md,
            &mut passed,
            !re_merged,
            &format!("{label}: 'Mistral' did NOT auto-merge back into 'Mistral AI'"),
        );
        let to_new =
            matches!(st, Some((status, Some(b), _)) if status == "auto_merged" && *b == n_id);
        let reviewed = matches!(st, Some((status, _, _)) if status == "pending_review");
        check(
            &mut md,
            &mut passed,
            to_new || reviewed,
            &format!("{label}: resolved to the human-created entity or sent to review"),
        );
    }

    // Step 4 — the merge operation refuses the pair in both directions.
    let a = resolve::merge_entities(pool, &e_id, &n_id).await;
    let b = resolve::merge_entities(pool, &n_id, &e_id).await;
    check(
        &mut md,
        &mut passed,
        a.is_err(),
        "merge(Mistral AI ← new) refused",
    );
    check(
        &mut md,
        &mut passed,
        b.is_err(),
        "merge(new ← Mistral AI) refused",
    );
    md.push_str(&format!(
        "## Step 4 — merge guard\n\n- {}\n- {}\n\n",
        a.err()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "merged (unexpected)".into()),
        b.err()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "merged (unexpected)".into())
    ));

    md.push_str(&format!(
        "## Result: {}\n",
        if passed { "PASS" } else { "FAIL" }
    ));
    Ok(DecorrelationOutcome {
        passed,
        report_md: md,
    })
}
