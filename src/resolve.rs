//! Tier 1: deterministic, always-on resolution. No LLM calls, pure CPU.
//!
//! 1. Extract candidate mention spans from raw text (capitalized multi-word heuristic).
//! 2. Score each mention against a gazetteer (existing entities' canonical_name + aliases)
//!    with Jaro-Winkler similarity.
//! 3. score >= AUTO_MERGE_THRESHOLD  -> merge into the matched entity.
//!    score <  NEW_ENTITY_FLOOR (or no gazetteer hit at all) -> new low-confidence entity, needs_llm.
//!    otherwise -> pending_review (ambiguous, cheap enough to also flag needs_llm for a real decision).

use std::sync::OnceLock;

use anyhow::{bail, Result};
use chrono::{Duration, Utc};
use regex::Regex;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::models::{new_id, now, RawItem, WmEntity};

const AUTO_MERGE_THRESHOLD: f64 = 0.87;
const NEW_ENTITY_FLOOR: f64 = 0.5;
/// Lattice caps entity expiry at 30 days out unless `noExpiry` is set explicitly; we mirror
/// that default rather than letting every fact live forever with equal weight.
const DEFAULT_EXPIRY_DAYS: i64 = 30;

fn default_expiry() -> String {
    (Utc::now() + Duration::days(DEFAULT_EXPIRY_DAYS)).to_rfc3339()
}

fn mention_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Runs of 1-4 capitalized words - a cheap, language-agnostic-ish proxy for
        // named-entity spans. Deliberately permissive; false positives get filtered
        // by the gazetteer/threshold step or fall through to Tier 2.
        Regex::new(r"\b([A-Z][a-zA-Z0-9&\-]*(?:\s+[A-Z][a-zA-Z0-9&\-]*){0,3})\b").unwrap()
    })
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
        "INSERT INTO wm_source_items (id, source_type, source_ref, content_hash, title, raw_text, ingested_at, status, license_class) \
         VALUES (?, ?, ?, ?, ?, ?, ?, 'pending', ?)",
    )
    .bind(&id)
    .bind(&item.source_type)
    .bind(&item.source_ref)
    .bind(&hash)
    .bind(&item.title)
    .bind(&item.text)
    .bind(now())
    .bind(&item.license_class)
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

async fn load_gazetteer(pool: &SqlitePool) -> Result<Vec<WmEntity>> {
    Ok(sqlx::query_as::<_, WmEntity>("SELECT * FROM wm_entities")
        .fetch_all(pool)
        .await?)
}

fn best_match(mention: &str, gazetteer: &[WmEntity]) -> Option<(String, f64)> {
    let mut best: Option<(String, f64)> = None;
    for entity in gazetteer {
        let mut candidates = vec![entity.canonical_name.clone()];
        candidates.extend(entity.alias_list());
        for name in candidates {
            let score = strsim::jaro_winkler(&mention.to_lowercase(), &name.to_lowercase());
            if best.as_ref().map(|(_, s)| score > *s).unwrap_or(true) {
                best = Some((entity.id.clone(), score));
            }
        }
    }
    best
}

fn extract_mentions(text: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for cap in mention_regex().captures_iter(text) {
        let m = cap[1].trim().to_string();
        // filter obviously-not-entity single common words (very short, all-caps acronyms kept)
        if m.split_whitespace().count() == 1 && m.len() < 3 {
            continue;
        }
        if seen.insert(m.clone()) {
            out.push(m);
        }
    }
    out
}

/// Run the Tier 1 pass over every `pending` source item, capped at `limit` items per call.
pub async fn resolve_pending(pool: &SqlitePool, limit: i64) -> Result<ResolveStats> {
    let mut stats = ResolveStats::default();

    let items: Vec<(String, String)> =
        sqlx::query_as("SELECT id, raw_text FROM wm_source_items WHERE status = 'pending' LIMIT ?")
            .bind(limit)
            .fetch_all(pool)
            .await?;

    for (item_id, raw_text) in items {
        stats.items_processed += 1;
        let mentions = extract_mentions(&raw_text);
        let mut resolved_entity_ids: Vec<String> = Vec::new();
        let mut any_needs_llm = false;

        for mention in mentions {
            // reload gazetteer each mention so within-item co-mentions can also resolve
            // against entities newly created earlier in this same loop
            let gazetteer = load_gazetteer(pool).await?;
            let matched = best_match(&mention, &gazetteer);

            let (status, entity_id, score) = match matched {
                Some((eid, score)) if score >= AUTO_MERGE_THRESHOLD => {
                    touch_entity(pool, &eid).await?;
                    stats.auto_merged += 1;
                    ("auto_merged", Some(eid), Some(score))
                }
                Some((_, score)) if score < NEW_ENTITY_FLOOR => {
                    let eid = create_entity(pool, &mention, 0.4).await?;
                    stats.new_entities += 1;
                    any_needs_llm = true;
                    ("needs_llm", Some(eid), Some(score))
                }
                None => {
                    let eid = create_entity(pool, &mention, 0.4).await?;
                    stats.new_entities += 1;
                    any_needs_llm = true;
                    ("needs_llm", Some(eid), None)
                }
                Some((eid, score)) => {
                    stats.pending_review += 1;
                    any_needs_llm = true;
                    ("pending_review", Some(eid.clone()), Some(score))
                }
            };

            sqlx::query(
                "INSERT INTO wm_extraction_candidates (id, source_item_id, mention_text, mention_type_guess, best_match_entity_id, match_score, status, created_at) \
                 VALUES (?, ?, ?, NULL, ?, ?, ?, ?)",
            )
            .bind(new_id())
            .bind(&item_id)
            .bind(&mention)
            .bind(&entity_id)
            .bind(score)
            .bind(status)
            .bind(now())
            .execute(pool)
            .await?;

            if let Some(eid) = entity_id {
                resolved_entity_ids.push(eid);
            }
        }

        // co-occurrence edges: anything mentioned together in this item is linked
        resolved_entity_ids.sort();
        resolved_entity_ids.dedup();
        for pair in resolved_entity_ids.windows(2) {
            link_entities(pool, &pair[0], &pair[1], &item_id).await?;
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

async fn create_entity(pool: &SqlitePool, mention: &str, confidence: f64) -> Result<String> {
    let id = new_id();
    let ts = now();
    sqlx::query(
        "INSERT INTO wm_entities (id, entity_type, canonical_name, aliases, external_ids, confidence, metadata, first_seen, last_seen, is_live, expiry_time, last_source_update_time) \
         VALUES (?, 'unknown', ?, '[]', '{}', ?, '{}', ?, ?, 1, ?, ?)",
    )
    .bind(&id)
    .bind(mention)
    .bind(confidence)
    .bind(&ts)
    .bind(&ts)
    .bind(default_expiry())
    .bind(&ts)
    .execute(pool)
    .await?;
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

async fn is_decorrelated(pool: &SqlitePool, entity_a: &str, entity_b: &str) -> Result<bool> {
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

    Ok(())
}

async fn link_entities(pool: &SqlitePool, a: &str, b: &str, source_item_id: &str) -> Result<()> {
    link_entities_typed(pool, a, b, "mentioned_with", source_item_id).await
}

/// Public typed variant used by Tier 2 (LLM-derived relations carry a real `edge_type`
/// instead of the generic co-occurrence default Tier 1 uses).
pub async fn link_entities_typed(
    pool: &SqlitePool,
    a: &str,
    b: &str,
    edge_type: &str,
    source_item_id: &str,
) -> Result<()> {
    if a == b {
        return Ok(());
    }
    let existing: Option<(String, String)> = sqlx::query_as(
        "SELECT id, source_item_ids FROM wm_edges WHERE (from_id = ? AND to_id = ?) OR (from_id = ? AND to_id = ?)",
    )
    .bind(a)
    .bind(b)
    .bind(b)
    .bind(a)
    .fetch_optional(pool)
    .await?;

    if let Some((edge_id, source_ids_json)) = existing {
        let mut ids: Vec<String> = serde_json::from_str(&source_ids_json).unwrap_or_default();
        if !ids.contains(&source_item_id.to_string()) {
            ids.push(source_item_id.to_string());
        }
        sqlx::query(
            "UPDATE wm_edges SET weight = weight + 1, source_item_ids = ?, last_seen = ? WHERE id = ?",
        )
        .bind(serde_json::to_string(&ids)?)
        .bind(now())
        .bind(&edge_id)
        .execute(pool)
        .await?;
    } else {
        let ts = now();
        sqlx::query(
            "INSERT INTO wm_edges (id, from_id, to_id, edge_type, weight, metadata, source_item_ids, first_seen, last_seen) \
             VALUES (?, ?, ?, ?, 1.0, '{}', ?, ?, ?)",
        )
        .bind(new_id())
        .bind(a)
        .bind(b)
        .bind(edge_type)
        .bind(serde_json::to_string(&vec![source_item_id])?)
        .bind(&ts)
        .bind(&ts)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Tier 2 entry point: resolve an LLM-proposed entity (mention + normalized canonical_name)
/// against the existing gazetteer using the same Jaro-Winkler check Tier 1 uses, so LLM
/// output is never blindly trusted into a duplicate node. Returns the entity id, creating
/// one if nothing matched closely enough.
pub async fn upsert_resolved_entity(
    pool: &SqlitePool,
    mention: &str,
    canonical_name: &str,
    entity_type: &str,
    confidence: f64,
) -> Result<String> {
    let gazetteer = load_gazetteer(pool).await?;
    let matched =
        best_match(canonical_name, &gazetteer).filter(|(_, score)| *score >= AUTO_MERGE_THRESHOLD);

    if let Some((id, _)) = matched {
        let existing = gazetteer.iter().find(|e| e.id == id);
        let mut aliases = existing.map(|e| e.alias_list()).unwrap_or_default();
        if !aliases.contains(&mention.to_string()) && mention != canonical_name {
            aliases.push(mention.to_string());
        }
        let new_confidence = existing
            .map(|e| e.confidence.max(confidence))
            .unwrap_or(confidence);
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
        .bind(&id)
        .execute(pool)
        .await?;
        Ok(id)
    } else {
        let id = new_id();
        let ts = now();
        let aliases = if mention != canonical_name {
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
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use sqlx::sqlite::SqlitePoolOptions;

    use super::*;

    #[test]
    fn extracts_capitalized_spans() {
        let mentions =
            extract_mentions("Recorded Future acquired by Mastercard, said Babel Street.");
        assert!(mentions.iter().any(|m| m == "Recorded Future"));
        assert!(mentions.iter().any(|m| m == "Mastercard"));
        assert!(mentions.iter().any(|m| m.starts_with("Babel Street")));
    }

    async fn memory_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn decorrelated_pair_refuses_merge() {
        let pool = memory_pool().await;
        let a = create_entity(&pool, "Apple Inc", 0.9).await.unwrap();
        let b = create_entity(&pool, "Apple Records", 0.9).await.unwrap();

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

        // both entities must still exist - merge was refused, not partially applied
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM wm_entities")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 2);
    }

    #[tokio::test]
    async fn undecorrelated_pair_merges_successfully() {
        let pool = memory_pool().await;
        let a = create_entity(&pool, "Recorded Future", 0.9).await.unwrap();
        let b = create_entity(&pool, "RF Inc", 0.5).await.unwrap();

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
    }

    #[tokio::test]
    async fn stale_entity_expires_and_touch_revives() {
        let pool = memory_pool().await;
        let id = create_entity(&pool, "Some Corp", 0.5).await.unwrap();
        // force it into the past so expire_stale_entities has something to catch
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
}
