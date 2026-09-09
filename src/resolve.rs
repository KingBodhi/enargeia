//! Tier 1: deterministic, always-on resolution. No LLM calls.
//!
//! 1. Extract mentions (GLiNER zero-shot NER when a model is present; regex fallback).
//! 2. Block: pull ≤50 candidate entities that share a name key with the mention.
//! 3. Score each candidate with the multi-signal probabilistic matcher.
//! 4. Merge (touch the entity), Review (park for a human / Tier 2), or New (create).
//!
//! Every candidate row keeps its feature breakdown, so a decision can always be explained.
//! Human decorrelations are honored here too: a merge is demoted to review when the winning
//! candidate was declared distinct from another close candidate.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, Utc};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::{
    block,
    extract::default_extractor,
    matcher::{decide, score, Candidate, Decision, MatchScore, Weights},
    models::{new_id, now, RawItem, WmEntity, WmExtractionCandidate},
};

/// Lattice caps entity expiry at 30 days out unless `noExpiry` is set explicitly; we mirror
/// that default rather than letting every fact live forever with equal weight.
const DEFAULT_EXPIRY_DAYS: i64 = 30;
const RECENT_DAYS: i64 = 30;
/// A decorrelated runner-up within this score gap of the winner forces review.
const DECORRELATION_VETO_GAP: f64 = 3.0;

/// Relation types where a subject (`from`) can only hold one valid object at a time, or an
/// object (`to`) only one valid subject. A newer contradicting fact supersedes the older one.
/// (edge_type, from_exclusive, to_exclusive)
pub const EXCLUSIVE_RELATIONS: &[(&str, bool, bool)] = &[
    ("ceo_of", true, true),
    ("cfo_of", true, true),
    ("cto_of", true, true),
    ("coo_of", true, true),
    ("chairman_of", true, true),
    ("president_of", true, true),
    ("headquartered_in", true, false),
    ("based_in", true, false),
    ("owned_by", true, false),
    ("acquired_by", true, false),
    ("parent_of", false, true),
];

fn default_expiry() -> String {
    (Utc::now() + Duration::days(DEFAULT_EXPIRY_DAYS)).to_rfc3339()
}

fn is_recent(ts: &str) -> bool {
    DateTime::parse_from_rfc3339(ts)
        .map(|t| Utc::now() - t.with_timezone(&Utc) < Duration::days(RECENT_DAYS))
        .unwrap_or(false)
}

pub fn content_hash(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Insert a raw item if new (dedup by content hash). Returns the source_item id when inserted,
/// `None` when it was already seen.
pub async fn ingest_item(pool: &SqlitePool, item: &RawItem) -> Result<Option<String>> {
    let hash = content_hash(&format!("{}|{}", item.source_ref, item.text));
    let existing: Option<(String,)> =
        sqlx::query_as("SELECT id FROM wm_source_items WHERE content_hash = ?")
            .bind(&hash)
            .fetch_optional(pool)
            .await?;
    if existing.is_some() {
        return Ok(None);
    }
    let id = new_id();
    sqlx::query(
        "INSERT INTO wm_source_items (id, source_type, source_ref, content_hash, title, raw_text, ingested_at, status, license_class, published_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, 'pending', ?, ?)",
    )
    .bind(&id)
    .bind(&item.source_type)
    .bind(&item.source_ref)
    .bind(&hash)
    .bind(&item.title)
    .bind(&item.text)
    .bind(now())
    .bind(&item.license_class)
    .bind(&item.published_at)
    .execute(pool)
    .await?;
    Ok(Some(id))
}

#[derive(Debug, Default)]
pub struct ResolveStats {
    pub items_processed: usize,
    pub auto_merged: usize,
    pub new_entities: usize,
    pub needs_llm: usize,
    pub pending_review: usize,
}

async fn cooc_fraction(pool: &SqlitePool, entity_id: &str, cooc_ids: &[String]) -> Result<f64> {
    if cooc_ids.is_empty() {
        return Ok(0.0);
    }
    let placeholders = cooc_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT COUNT(*) FROM wm_edges WHERE (from_id = ? AND to_id IN ({placeholders})) \
         OR (to_id = ? AND from_id IN ({placeholders}))"
    );
    let mut q = sqlx::query_as::<_, (i64,)>(&sql).bind(entity_id);
    for id in cooc_ids {
        q = q.bind(id);
    }
    q = q.bind(entity_id);
    for id in cooc_ids {
        q = q.bind(id);
    }
    let (n,) = q.fetch_one(pool).await?;
    Ok((n as f64 / cooc_ids.len() as f64).min(1.0))
}

async fn corroboration_count(pool: &SqlitePool, entity_id: &str) -> Result<u32> {
    let (n,): (i64,) = sqlx::query_as(
        "SELECT COUNT(DISTINCT source_item_id) FROM wm_extraction_candidates \
         WHERE best_match_entity_id = ? AND status IN ('auto_merged', 'resolved', 'confirmed')",
    )
    .bind(entity_id)
    .fetch_one(pool)
    .await?;
    Ok(n as u32)
}

async fn decorrelated_with_any(
    pool: &SqlitePool,
    entity_id: &str,
    others: &[String],
) -> Result<bool> {
    if others.is_empty() {
        return Ok(false);
    }
    let placeholders = others.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT COUNT(*) FROM wm_decorrelations WHERE (entity_a_id = ? AND entity_b_id IN ({placeholders})) \
         OR (entity_b_id = ? AND entity_a_id IN ({placeholders}))"
    );
    let mut q = sqlx::query_as::<_, (i64,)>(&sql).bind(entity_id);
    for id in others {
        q = q.bind(id);
    }
    q = q.bind(entity_id);
    for id in others {
        q = q.bind(id);
    }
    let (n,) = q.fetch_one(pool).await?;
    Ok(n > 0)
}

/// Best-scoring existing entity for a mention, via blocking + the probabilistic matcher.
/// `cooc_ids` are entities already resolved from the same source item (context evidence).
/// If the winner would merge but a human previously decorrelated it from a close runner-up,
/// the score is demoted into the review band and the veto is recorded in the breakdown.
pub async fn best_candidate(
    pool: &SqlitePool,
    mention: &str,
    mention_type: Option<&str>,
    cooc_ids: &[String],
    weights: &Weights,
) -> Result<Option<(WmEntity, MatchScore)>> {
    let mut scored: Vec<(WmEntity, MatchScore)> = Vec::new();
    for ent in block::candidates_for(pool, mention).await? {
        let cooc = cooc_fraction(pool, &ent.id, cooc_ids).await?;
        let corroboration = corroboration_count(pool, &ent.id).await?;
        let aliases = ent.alias_list();
        let cand = Candidate {
            canonical: &ent.canonical_name,
            aliases: &aliases,
            entity_type: &ent.entity_type,
            cooc,
            corroboration,
            recent: is_recent(&ent.last_seen),
        };
        let ms = score(mention, mention_type, &cand, weights);
        scored.push((ent, ms));
    }
    if scored.is_empty() {
        return Ok(None);
    }
    scored.sort_by(|a, b| {
        b.1.total
            .partial_cmp(&a.1.total)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let (best_ent, mut best_ms) = scored.remove(0);

    if decide(best_ms.total, weights) == Decision::Merge {
        let close: Vec<String> = scored
            .iter()
            .filter(|(_, ms)| best_ms.total - ms.total < DECORRELATION_VETO_GAP)
            .map(|(e, _)| e.id.clone())
            .collect();
        if decorrelated_with_any(pool, &best_ent.id, &close).await? {
            let demotion = weights.upper - 0.5 - best_ms.total;
            best_ms
                .contributions
                .push(("decorrelation_veto".to_string(), demotion));
            best_ms.total += demotion;
        }
    }
    Ok(Some((best_ent, best_ms)))
}

/// Run the Tier 1 pass over every `pending` source item, capped at `limit` items per call.
pub async fn resolve_pending(pool: &SqlitePool, limit: i64) -> Result<ResolveStats> {
    let mut stats = ResolveStats::default();
    let extractor = default_extractor()?;
    let weights = Weights::from_env();
    block::ensure_index(pool).await?;
    tracing::info!(
        extractor = extractor.name(),
        upper = weights.upper,
        lower = weights.lower,
        "tier 1"
    );

    let items: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT id, raw_text, COALESCE(published_at, ingested_at) FROM wm_source_items WHERE status = 'pending' LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;

    for (item_id, raw_text, observed_at) in items {
        stats.items_processed += 1;
        let mentions = extractor.extract(&raw_text)?;
        let mut resolved_entity_ids: Vec<String> = Vec::new();
        let mut any_needs_llm = false;

        for m in mentions {
            let mention = m.text.as_str();
            let label = m.label.as_deref();
            let confidence = f64::from(m.score).clamp(0.3, 0.9);
            let best = best_candidate(pool, mention, label, &resolved_entity_ids, &weights).await?;

            let (status, entity_id, total, features) = match best {
                Some((ent, ms)) => match decide(ms.total, &weights) {
                    Decision::Merge => {
                        touch_entity(pool, &ent.id).await?;
                        stats.auto_merged += 1;
                        ("auto_merged", ent.id, Some(ms.total), Some(ms))
                    }
                    Decision::Review => {
                        stats.pending_review += 1;
                        any_needs_llm = true;
                        ("pending_review", ent.id, Some(ms.total), Some(ms))
                    }
                    Decision::New => {
                        let eid =
                            create_entity(pool, mention, label.unwrap_or("unknown"), confidence)
                                .await?;
                        stats.new_entities += 1;
                        any_needs_llm = true;
                        ("needs_llm", eid, Some(ms.total), Some(ms))
                    }
                },
                None => {
                    let eid = create_entity(pool, mention, label.unwrap_or("unknown"), confidence)
                        .await?;
                    stats.new_entities += 1;
                    any_needs_llm = true;
                    ("needs_llm", eid, None, None)
                }
            };
            let features_json = features.map(|f| serde_json::to_string(&f).unwrap_or_default());

            sqlx::query(
                "INSERT INTO wm_extraction_candidates (id, source_item_id, mention_text, mention_type_guess, best_match_entity_id, match_score, status, created_at, feature_scores) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(new_id())
            .bind(&item_id)
            .bind(mention)
            .bind(label)
            .bind(&entity_id)
            .bind(total)
            .bind(status)
            .bind(now())
            .bind(features_json)
            .execute(pool)
            .await?;

            if !resolved_entity_ids.contains(&entity_id) {
                resolved_entity_ids.push(entity_id);
            }
        }

        // co-occurrence edges: anything mentioned together in this item is linked
        resolved_entity_ids.sort();
        resolved_entity_ids.dedup();
        for pair in resolved_entity_ids.windows(2) {
            link_entities_typed(
                pool,
                &pair[0],
                &pair[1],
                "mentioned_with",
                &item_id,
                &observed_at,
            )
            .await?;
        }

        let new_status = if any_needs_llm {
            "needs_llm"
        } else {
            "resolved"
        };
        if any_needs_llm {
            stats.needs_llm += 1;
        }
        sqlx::query("UPDATE wm_source_items SET status = ? WHERE id = ?")
            .bind(new_status)
            .bind(&item_id)
            .execute(pool)
            .await?;
    }

    Ok(stats)
}

pub async fn create_entity(
    pool: &SqlitePool,
    mention: &str,
    entity_type: &str,
    confidence: f64,
) -> Result<String> {
    let id = new_id();
    let ts = now();
    sqlx::query(
        "INSERT INTO wm_entities (id, entity_type, canonical_name, aliases, external_ids, confidence, metadata, first_seen, last_seen, is_live, expiry_time, last_source_update_time) \
         VALUES (?, ?, ?, '[]', '{}', ?, '{}', ?, ?, 1, ?, ?)",
    )
    .bind(&id)
    .bind(entity_type)
    .bind(mention)
    .bind(confidence)
    .bind(&ts)
    .bind(&ts)
    .bind(default_expiry())
    .bind(&ts)
    .execute(pool)
    .await?;
    block::index_entity(pool, &id, mention, &[]).await?;
    Ok(id)
}

/// A fresh source touch refreshes liveness: `last_seen`, `expiry_time`, and
/// `last_source_update_time` all move forward, and a previously-expired entity is revived.
/// Mirrors Lattice's "updates trigger on provenance.source_update_time changing."
async fn touch_entity(pool: &SqlitePool, id: &str) -> Result<()> {
    let ts = now();
    sqlx::query(
        "UPDATE wm_entities SET last_seen = ?, expiry_time = ?, last_source_update_time = ?, is_live = 1 WHERE id = ?",
    )
    .bind(&ts)
    .bind(default_expiry())
    .bind(&ts)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn add_alias(pool: &SqlitePool, entity: &WmEntity, alias: &str) -> Result<()> {
    let mut aliases = entity.alias_list();
    if alias == entity.canonical_name || aliases.iter().any(|a| a == alias) {
        return Ok(());
    }
    aliases.push(alias.to_string());
    sqlx::query("UPDATE wm_entities SET aliases = ? WHERE id = ?")
        .bind(serde_json::to_string(&aliases)?)
        .bind(&entity.id)
        .execute(pool)
        .await?;
    block::index_entity(pool, &entity.id, &entity.canonical_name, &aliases).await
}

/// Flips `is_live = 0` on any entity whose `expiry_time` has passed and hasn't been refreshed
/// by a fresh source touch. Returns how many were expired. Run periodically (CLI `expire`
/// subcommand / cron) - not wired into the ingest/resolve hot path.
pub async fn expire_stale_entities(pool: &SqlitePool) -> Result<usize> {
    let result = sqlx::query(
        "UPDATE wm_entities SET is_live = 0 WHERE is_live = 1 AND expiry_time IS NOT NULL AND expiry_time < ?",
    )
    .bind(now())
    .execute(pool)
    .await?;
    Ok(result.rows_affected() as usize)
}

/// Persists a human "these are NOT the same entity" decision so no future merge (Tier 1, Tier 2,
/// or a manual dedup pass) can re-propose collapsing this pair - Lattice's "sticky decorrelation."
pub async fn decorrelate(
    pool: &SqlitePool,
    entity_a: &str,
    entity_b: &str,
    reason: Option<&str>,
) -> Result<()> {
    if entity_a == entity_b {
        bail!("cannot decorrelate an entity from itself");
    }
    let (a, b) = if entity_a < entity_b {
        (entity_a, entity_b)
    } else {
        (entity_b, entity_a)
    };
    sqlx::query(
        "INSERT INTO wm_decorrelations (id, entity_a_id, entity_b_id, reason, decided_at) VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT(entity_a_id, entity_b_id) DO UPDATE SET reason = excluded.reason, decided_at = excluded.decided_at",
    )
    .bind(new_id())
    .bind(a)
    .bind(b)
    .bind(reason)
    .bind(now())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn is_decorrelated(pool: &SqlitePool, entity_a: &str, entity_b: &str) -> Result<bool> {
    let (a, b) = if entity_a < entity_b {
        (entity_a, entity_b)
    } else {
        (entity_b, entity_a)
    };
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT id FROM wm_decorrelations WHERE entity_a_id = ? AND entity_b_id = ?",
    )
    .bind(a)
    .bind(b)
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}

/// The actual "correlator merge" operation the decorrelation guard protects: folds `absorb_id`
/// into `keep_id` (edges repointed, aliases/candidates carried over, absorb_id deleted).
/// Refuses outright if this exact pair was previously decorrelated by a human.
pub async fn merge_entities(pool: &SqlitePool, keep_id: &str, absorb_id: &str) -> Result<()> {
    if keep_id == absorb_id {
        bail!("keep_id and absorb_id must differ");
    }
    if is_decorrelated(pool, keep_id, absorb_id).await? {
        bail!("refusing merge: {keep_id} and {absorb_id} were previously decorrelated by a human decision");
    }

    let keep: Option<WmEntity> = sqlx::query_as("SELECT * FROM wm_entities WHERE id = ?")
        .bind(keep_id)
        .fetch_optional(pool)
        .await?;
    let absorb: Option<WmEntity> = sqlx::query_as("SELECT * FROM wm_entities WHERE id = ?")
        .bind(absorb_id)
        .fetch_optional(pool)
        .await?;
    let (Some(keep), Some(absorb)) = (keep, absorb) else {
        bail!("both entities must exist to merge");
    };

    let mut aliases = keep.alias_list();
    for a in absorb.alias_list() {
        if !aliases.contains(&a) {
            aliases.push(a);
        }
    }
    if !aliases.contains(&absorb.canonical_name) && absorb.canonical_name != keep.canonical_name {
        aliases.push(absorb.canonical_name.clone());
    }

    sqlx::query("UPDATE wm_edges SET from_id = ? WHERE from_id = ?")
        .bind(keep_id)
        .bind(absorb_id)
        .execute(pool)
        .await?;
    sqlx::query("UPDATE wm_edges SET to_id = ? WHERE to_id = ?")
        .bind(keep_id)
        .bind(absorb_id)
        .execute(pool)
        .await?;
    sqlx::query("UPDATE wm_extraction_candidates SET best_match_entity_id = ? WHERE best_match_entity_id = ?")
        .bind(keep_id)
        .bind(absorb_id)
        .execute(pool)
        .await?;
    sqlx::query("UPDATE wm_entities SET aliases = ?, confidence = ?, last_seen = ? WHERE id = ?")
        .bind(serde_json::to_string(&aliases)?)
        .bind(keep.confidence.max(absorb.confidence))
        .bind(now())
        .bind(keep_id)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM wm_entities WHERE id = ?")
        .bind(absorb_id)
        .execute(pool)
        .await?;
    block::index_entity(pool, keep_id, &keep.canonical_name, &aliases).await?;

    Ok(())
}

fn exclusivity(edge_type: &str) -> (bool, bool) {
    EXCLUSIVE_RELATIONS
        .iter()
        .find(|(t, _, _)| *t == edge_type)
        .map(|(_, f, t)| (*f, *t))
        .unwrap_or((false, false))
}

/// Records a relation observed in `source_item_id`, valid from `observed_at` (the source's
/// publication time). Repeat observations accumulate weight and sources; the earliest
/// observation defines `valid_at`. For exclusive relation types, a newer contradicting fact
/// invalidates older ones; an older fact arriving late is inserted already superseded.
pub async fn link_entities_typed(
    pool: &SqlitePool,
    a: &str,
    b: &str,
    edge_type: &str,
    source_item_id: &str,
    observed_at: &str,
) -> Result<()> {
    if a == b {
        return Ok(());
    }
    let symmetric = edge_type == "mentioned_with";
    let existing: Option<(String, String, Option<String>)> = if symmetric {
        sqlx::query_as(
            "SELECT id, source_item_ids, valid_at FROM wm_edges WHERE edge_type = ? AND ((from_id = ? AND to_id = ?) OR (from_id = ? AND to_id = ?))",
        )
        .bind(edge_type)
        .bind(a)
        .bind(b)
        .bind(b)
        .bind(a)
        .fetch_optional(pool)
        .await?
    } else {
        sqlx::query_as(
            "SELECT id, source_item_ids, valid_at FROM wm_edges WHERE edge_type = ? AND from_id = ? AND to_id = ?",
        )
        .bind(edge_type)
        .bind(a)
        .bind(b)
        .fetch_optional(pool)
        .await?
    };

    if let Some((edge_id, source_ids_json, valid_at)) = existing {
        let mut ids: Vec<String> = serde_json::from_str(&source_ids_json).unwrap_or_default();
        if !ids.contains(&source_item_id.to_string()) {
            ids.push(source_item_id.to_string());
        }
        let earliest = match valid_at {
            Some(v) if v.as_str() <= observed_at => v,
            _ => observed_at.to_string(),
        };
        sqlx::query(
            "UPDATE wm_edges SET weight = weight + 1, source_item_ids = ?, last_seen = ?, valid_at = ? WHERE id = ?",
        )
        .bind(serde_json::to_string(&ids)?)
        .bind(now())
        .bind(earliest)
        .bind(&edge_id)
        .execute(pool)
        .await?;
        return Ok(());
    }

    let new_id_ = new_id();
    let ts = now();
    let (from_excl, to_excl) = exclusivity(edge_type);
    let mut invalid_at: Option<String> = None;
    let mut superseded_by: Option<String> = None;

    for (excl, col, key) in [(from_excl, "from_id", a), (to_excl, "to_id", b)] {
        if !excl {
            continue;
        }
        let sql = format!(
            "SELECT id, valid_at FROM wm_edges WHERE edge_type = ? AND {col} = ? AND invalid_at IS NULL"
        );
        let rivals: Vec<(String, Option<String>)> = sqlx::query_as(&sql)
            .bind(edge_type)
            .bind(key)
            .fetch_all(pool)
            .await?;
        for (rival_id, rival_valid) in rivals {
            let rival_valid = rival_valid.unwrap_or_default();
            if rival_valid.as_str() <= observed_at {
                // The new fact is newer: it supersedes the rival.
                sqlx::query("UPDATE wm_edges SET invalid_at = ?, superseded_by = ? WHERE id = ?")
                    .bind(observed_at)
                    .bind(&new_id_)
                    .bind(&rival_id)
                    .execute(pool)
                    .await?;
            } else if invalid_at
                .as_deref()
                .map(|v| rival_valid.as_str() < v)
                .unwrap_or(true)
            {
                // The rival is newer: the new fact was already superseded when it became known.
                invalid_at = Some(rival_valid);
                superseded_by = Some(rival_id);
            }
        }
    }

    sqlx::query(
        "INSERT INTO wm_edges (id, from_id, to_id, edge_type, weight, metadata, source_item_ids, first_seen, last_seen, valid_at, invalid_at, superseded_by) \
         VALUES (?, ?, ?, ?, 1.0, '{}', ?, ?, ?, ?, ?, ?)",
    )
    .bind(&new_id_)
    .bind(a)
    .bind(b)
    .bind(edge_type)
    .bind(serde_json::to_string(&vec![source_item_id])?)
    .bind(&ts)
    .bind(&ts)
    .bind(observed_at)
    .bind(invalid_at)
    .bind(superseded_by)
    .execute(pool)
    .await?;
    Ok(())
}

/// Tier 2 entry point: resolve an LLM-proposed entity (mention + normalized canonical_name)
/// through the same blocking + matcher path as any other mention, so LLM output is never
/// trusted blindly into a duplicate node. Returns the entity id, creating one if nothing
/// matched confidently enough.
pub async fn upsert_resolved_entity(
    pool: &SqlitePool,
    mention: &str,
    canonical_name: &str,
    entity_type: &str,
    confidence: f64,
) -> Result<String> {
    let weights = Weights::from_env();
    let best = best_candidate(pool, canonical_name, Some(entity_type), &[], &weights).await?;
    let matched = best.filter(|(_, ms)| decide(ms.total, &weights) == Decision::Merge);

    if let Some((existing, _)) = matched {
        let mut aliases = existing.alias_list();
        for a in [mention, canonical_name] {
            if !aliases.iter().any(|x| x == a) && a != existing.canonical_name {
                aliases.push(a.to_string());
            }
        }
        let new_confidence = existing.confidence.max(confidence);
        let ts = now();
        sqlx::query(
            "UPDATE wm_entities SET entity_type = CASE WHEN entity_type = 'unknown' THEN ? ELSE entity_type END, \
             aliases = ?, confidence = ?, last_seen = ?, expiry_time = ?, last_source_update_time = ?, is_live = 1 WHERE id = ?",
        )
        .bind(entity_type)
        .bind(serde_json::to_string(&aliases)?)
        .bind(new_confidence)
        .bind(&ts)
        .bind(default_expiry())
        .bind(&ts)
        .bind(&existing.id)
        .execute(pool)
        .await?;
        block::index_entity(pool, &existing.id, &existing.canonical_name, &aliases).await?;
        Ok(existing.id)
    } else {
        let id = new_id();
        let ts = now();
        let aliases: Vec<String> = if mention != canonical_name {
            vec![mention.to_string()]
        } else {
            vec![]
        };
        sqlx::query(
            "INSERT INTO wm_entities (id, entity_type, canonical_name, aliases, external_ids, confidence, metadata, first_seen, last_seen, is_live, expiry_time, last_source_update_time) \
             VALUES (?, ?, ?, ?, '{}', ?, '{}', ?, ?, 1, ?, ?)",
        )
        .bind(&id)
        .bind(entity_type)
        .bind(canonical_name)
        .bind(serde_json::to_string(&aliases)?)
        .bind(confidence)
        .bind(&ts)
        .bind(&ts)
        .bind(default_expiry())
        .bind(&ts)
        .execute(pool)
        .await?;
        block::index_entity(pool, &id, canonical_name, &aliases).await?;
        Ok(id)
    }
}

/// A human decision on a review candidate.
/// - `confirm`: the mention is the proposed entity (alias added, entity touched).
/// - `reject`: the mention is a different entity; a new entity is created and the pair is
///   decorrelated so nothing re-merges them.
/// - `new`: create a distinct entity without asserting anything about the proposed one.
pub async fn apply_decision(
    pool: &SqlitePool,
    candidate_id: &str,
    decision: &str,
    actor: &str,
    reason: Option<&str>,
) -> Result<String> {
    let cand: WmExtractionCandidate =
        sqlx::query_as("SELECT * FROM wm_extraction_candidates WHERE id = ?")
            .bind(candidate_id)
            .fetch_optional(pool)
            .await?
            .context("candidate not found")?;
    let label = cand.mention_type_guess.as_deref().unwrap_or("unknown");

    let (new_status, summary) = match decision {
        "confirm" => {
            let target = cand
                .best_match_entity_id
                .as_deref()
                .context("candidate has no proposed entity to confirm")?;
            let ent: WmEntity = sqlx::query_as("SELECT * FROM wm_entities WHERE id = ?")
                .bind(target)
                .fetch_one(pool)
                .await?;
            touch_entity(pool, target).await?;
            add_alias(pool, &ent, &cand.mention_text).await?;
            (
                "confirmed",
                format!(
                    "confirmed '{}' → {} ({})",
                    cand.mention_text, ent.canonical_name, ent.id
                ),
            )
        }
        "reject" => {
            let new_ent = create_entity(pool, &cand.mention_text, label, 0.7).await?;
            let mut msg = format!("created '{}' ({new_ent})", cand.mention_text);
            if let Some(prev) = cand.best_match_entity_id.as_deref() {
                decorrelate(pool, &new_ent, prev, reason).await?;
                msg.push_str(&format!("; decorrelated from {prev}"));
            }
            sqlx::query(
                "UPDATE wm_extraction_candidates SET best_match_entity_id = ? WHERE id = ?",
            )
            .bind(&new_ent)
            .bind(candidate_id)
            .execute(pool)
            .await?;
            ("rejected", msg)
        }
        "new" => {
            let new_ent = create_entity(pool, &cand.mention_text, label, 0.7).await?;
            sqlx::query(
                "UPDATE wm_extraction_candidates SET best_match_entity_id = ? WHERE id = ?",
            )
            .bind(&new_ent)
            .bind(candidate_id)
            .execute(pool)
            .await?;
            (
                "new",
                format!("created '{}' ({new_ent})", cand.mention_text),
            )
        }
        other => bail!("unknown decision {other:?} (expected confirm|reject|new)"),
    };

    sqlx::query("UPDATE wm_extraction_candidates SET status = ? WHERE id = ?")
        .bind(new_status)
        .bind(candidate_id)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO wm_decisions (id, candidate_id, decision, actor, reason, decided_at) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(new_id())
    .bind(candidate_id)
    .bind(decision)
    .bind(actor)
    .bind(reason)
    .bind(now())
    .execute(pool)
    .await?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use sqlx::sqlite::SqlitePoolOptions;

    use super::*;

    async fn memory_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    async fn edge(pool: &SqlitePool, id: &str) -> (Option<String>, Option<String>, Option<String>) {
        sqlx::query_as("SELECT valid_at, invalid_at, superseded_by FROM wm_edges WHERE id = ?")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn edge_id(pool: &SqlitePool, a: &str, b: &str, t: &str) -> String {
        let (id,): (String,) = sqlx::query_as(
            "SELECT id FROM wm_edges WHERE from_id = ? AND to_id = ? AND edge_type = ?",
        )
        .bind(a)
        .bind(b)
        .bind(t)
        .fetch_one(pool)
        .await
        .unwrap();
        id
    }

    #[tokio::test]
    async fn decorrelated_pair_refuses_merge() {
        let pool = memory_pool().await;
        let a = create_entity(&pool, "Apple Inc", "organization", 0.9)
            .await
            .unwrap();
        let b = create_entity(&pool, "Apple Records", "organization", 0.9)
            .await
            .unwrap();

        decorrelate(&pool, &a, &b, Some("different companies"))
            .await
            .unwrap();
        assert!(is_decorrelated(&pool, &a, &b).await.unwrap());
        assert!(
            is_decorrelated(&pool, &b, &a).await.unwrap(),
            "decorrelation must be order-independent"
        );

        let err = merge_entities(&pool, &a, &b).await.unwrap_err();
        assert!(err.to_string().contains("decorrelated"));

        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM wm_entities")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 2);
    }

    #[tokio::test]
    async fn undecorrelated_pair_merges_successfully() {
        let pool = memory_pool().await;
        let a = create_entity(&pool, "Recorded Future", "organization", 0.9)
            .await
            .unwrap();
        let b = create_entity(&pool, "RF Inc", "organization", 0.5)
            .await
            .unwrap();

        merge_entities(&pool, &a, &b).await.unwrap();

        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM wm_entities")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 1);
        let kept: WmEntity = sqlx::query_as("SELECT * FROM wm_entities WHERE id = ?")
            .bind(&a)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(kept.alias_list().contains(&"RF Inc".to_string()));
        let cands = block::candidates_for(&pool, "RF Inc").await.unwrap();
        assert!(cands.iter().any(|e| e.id == a));
    }

    #[tokio::test]
    async fn stale_entity_expires_and_touch_revives() {
        let pool = memory_pool().await;
        let id = create_entity(&pool, "Some Corp", "organization", 0.5)
            .await
            .unwrap();
        sqlx::query("UPDATE wm_entities SET expiry_time = '2000-01-01T00:00:00Z' WHERE id = ?")
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();

        let expired = expire_stale_entities(&pool).await.unwrap();
        assert_eq!(expired, 1);
        let row: (i64,) = sqlx::query_as("SELECT is_live FROM wm_entities WHERE id = ?")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.0, 0);

        touch_entity(&pool, &id).await.unwrap();
        let row: (i64,) = sqlx::query_as("SELECT is_live FROM wm_entities WHERE id = ?")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            row.0, 1,
            "a fresh source touch should revive an expired entity"
        );
    }

    #[tokio::test]
    async fn blocking_plus_matcher_merges_variant_and_rejects_lookalike() {
        let pool = memory_pool().await;
        let mistral = create_entity(&pool, "Mistral AI", "organization", 0.8)
            .await
            .unwrap();
        let _coinbase = create_entity(&pool, "Coinbase", "organization", 0.8)
            .await
            .unwrap();
        let w = Weights::default();

        let best = best_candidate(&pool, "Mistral", Some("organization"), &[], &w)
            .await
            .unwrap();
        let (ent, ms) = best.expect("candidate");
        assert_eq!(ent.id, mistral);
        assert_eq!(decide(ms.total, &w), Decision::Merge, "{ms:?}");

        let best = best_candidate(&pool, "CoinShares", Some("organization"), &[], &w)
            .await
            .unwrap();
        if let Some((_, ms)) = best {
            assert_ne!(decide(ms.total, &w), Decision::Merge, "{ms:?}");
        }
    }

    #[tokio::test]
    async fn decorrelation_vetoes_tier1_merge_between_close_candidates() {
        let pool = memory_pool().await;
        let a = create_entity(&pool, "Mistral AI", "organization", 0.8)
            .await
            .unwrap();
        let b = create_entity(&pool, "Mistral Labs", "organization", 0.8)
            .await
            .unwrap();
        let w = Weights::default();

        let (_, ms) = best_candidate(&pool, "Mistral", Some("organization"), &[], &w)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            decide(ms.total, &w),
            Decision::Merge,
            "sanity: merges before decorrelation {ms:?}"
        );

        decorrelate(&pool, &a, &b, Some("different companies"))
            .await
            .unwrap();
        let (_, ms) = best_candidate(&pool, "Mistral", Some("organization"), &[], &w)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decide(ms.total, &w), Decision::Review, "{ms:?}");
        assert!(ms
            .contributions
            .iter()
            .any(|(k, _)| k == "decorrelation_veto"));
    }

    #[tokio::test]
    async fn tier2_upsert_reuses_matching_entity() {
        let pool = memory_pool().await;
        let id = create_entity(&pool, "Federal Trade Commission", "organization", 0.8)
            .await
            .unwrap();
        let got = upsert_resolved_entity(
            &pool,
            "FTC",
            "Federal Trade Commission",
            "organization",
            0.9,
        )
        .await
        .unwrap();
        assert_eq!(got, id);
        let ent: WmEntity = sqlx::query_as("SELECT * FROM wm_entities WHERE id = ?")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(ent.alias_list().contains(&"FTC".to_string()));
    }

    #[tokio::test]
    async fn newer_exclusive_fact_supersedes_older() {
        let pool = memory_pool().await;
        let alice = create_entity(&pool, "Alice", "person", 0.9).await.unwrap();
        let bob = create_entity(&pool, "Bob", "person", 0.9).await.unwrap();
        let acme = create_entity(&pool, "Acme", "organization", 0.9)
            .await
            .unwrap();

        link_entities_typed(
            &pool,
            &alice,
            &acme,
            "ceo_of",
            "s1",
            "2025-01-01T00:00:00+00:00",
        )
        .await
        .unwrap();
        link_entities_typed(
            &pool,
            &bob,
            &acme,
            "ceo_of",
            "s2",
            "2026-06-01T00:00:00+00:00",
        )
        .await
        .unwrap();

        let old = edge_id(&pool, &alice, &acme, "ceo_of").await;
        let new = edge_id(&pool, &bob, &acme, "ceo_of").await;
        let (_, old_invalid, old_sup) = edge(&pool, &old).await;
        assert_eq!(old_invalid.as_deref(), Some("2026-06-01T00:00:00+00:00"));
        assert_eq!(old_sup.as_deref(), Some(new.as_str()));
        let (_, new_invalid, _) = edge(&pool, &new).await;
        assert!(new_invalid.is_none());
    }

    #[tokio::test]
    async fn older_fact_arriving_late_does_not_rewrite_history() {
        let pool = memory_pool().await;
        let alice = create_entity(&pool, "Alice", "person", 0.9).await.unwrap();
        let bob = create_entity(&pool, "Bob", "person", 0.9).await.unwrap();
        let acme = create_entity(&pool, "Acme", "organization", 0.9)
            .await
            .unwrap();

        // The newer fact is processed first (feeds are not chronological).
        link_entities_typed(
            &pool,
            &bob,
            &acme,
            "ceo_of",
            "s2",
            "2026-06-01T00:00:00+00:00",
        )
        .await
        .unwrap();
        link_entities_typed(
            &pool,
            &alice,
            &acme,
            "ceo_of",
            "s1",
            "2025-01-01T00:00:00+00:00",
        )
        .await
        .unwrap();

        let old = edge_id(&pool, &alice, &acme, "ceo_of").await;
        let new = edge_id(&pool, &bob, &acme, "ceo_of").await;
        let (old_valid, old_invalid, old_sup) = edge(&pool, &old).await;
        assert_eq!(old_valid.as_deref(), Some("2025-01-01T00:00:00+00:00"));
        assert_eq!(old_invalid.as_deref(), Some("2026-06-01T00:00:00+00:00"));
        assert_eq!(old_sup.as_deref(), Some(new.as_str()));
        let (_, new_invalid, _) = edge(&pool, &new).await;
        assert!(new_invalid.is_none(), "the current CEO must stay valid");
    }

    #[tokio::test]
    async fn non_exclusive_relations_coexist_and_accumulate() {
        let pool = memory_pool().await;
        let a = create_entity(&pool, "a16z", "organization", 0.9)
            .await
            .unwrap();
        let x = create_entity(&pool, "X Corp", "organization", 0.9)
            .await
            .unwrap();
        let y = create_entity(&pool, "Y Corp", "organization", 0.9)
            .await
            .unwrap();
        link_entities_typed(
            &pool,
            &a,
            &x,
            "invested_in",
            "s1",
            "2026-01-01T00:00:00+00:00",
        )
        .await
        .unwrap();
        link_entities_typed(
            &pool,
            &a,
            &y,
            "invested_in",
            "s2",
            "2026-02-01T00:00:00+00:00",
        )
        .await
        .unwrap();
        link_entities_typed(
            &pool,
            &a,
            &x,
            "invested_in",
            "s3",
            "2025-12-01T00:00:00+00:00",
        )
        .await
        .unwrap();
        let (valid, invalid, _) = edge(&pool, &edge_id(&pool, &a, &x, "invested_in").await).await;
        assert_eq!(
            valid.as_deref(),
            Some("2025-12-01T00:00:00+00:00"),
            "earliest observation defines valid_at"
        );
        assert!(invalid.is_none());
        let (w,): (f64,) =
            sqlx::query_as("SELECT weight FROM wm_edges WHERE from_id = ? AND to_id = ?")
                .bind(&a)
                .bind(&x)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(w, 2.0);
    }

    #[tokio::test]
    async fn reject_decision_creates_entity_and_decorrelates() {
        let pool = memory_pool().await;
        let e = create_entity(&pool, "Mistral AI", "organization", 0.8)
            .await
            .unwrap();
        sqlx::query("INSERT INTO wm_source_items (id, source_type, source_ref, content_hash, raw_text, ingested_at, status) VALUES ('s1','rss','u','h','t','2026-01-01','pending')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO wm_extraction_candidates (id, source_item_id, mention_text, mention_type_guess, best_match_entity_id, match_score, status, created_at) VALUES ('c1','s1','Mistral','organization',?,4.0,'pending_review','2026-01-01')")
            .bind(&e)
            .execute(&pool)
            .await
            .unwrap();

        let msg = apply_decision(&pool, "c1", "reject", "test", Some("different company"))
            .await
            .unwrap();
        assert!(msg.contains("decorrelated"));
        let (status, new_id): (String, String) = sqlx::query_as(
            "SELECT status, best_match_entity_id FROM wm_extraction_candidates WHERE id = 'c1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "rejected");
        assert_ne!(new_id, e);
        assert!(is_decorrelated(&pool, &new_id, &e).await.unwrap());
        let (n,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM wm_decisions WHERE candidate_id = 'c1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(n, 1);
    }
}
