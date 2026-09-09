//! `enargeia relations audit`: re-checks every stored typed relation against the source text
//! it came from with the same grounding rule Tier 2 now applies at insert time. Exists
//! because graphs built before the rule (or by a weaker model) carry invented relations,
//! and the honest fix is to re-verify them, not to hope the next brief avoids them.

use std::collections::HashMap;

use anyhow::Result;
use sqlx::SqlitePool;

use crate::{ground, models::WmEntity};

#[derive(Debug, Default)]
pub struct AuditStats {
    pub checked: usize,
    pub kept: usize,
    pub failed: usize,
    pub no_source_text: usize,
    pub deleted: usize,
}

/// (edge id, from_id, to_id, edge_type, source_item_ids)
type EdgeRow = (String, String, String, String, String);

async fn entity(
    pool: &SqlitePool,
    cache: &mut HashMap<String, WmEntity>,
    id: &str,
) -> Result<Option<WmEntity>> {
    if let Some(e) = cache.get(id) {
        return Ok(Some(e.clone()));
    }
    let e: Option<WmEntity> = sqlx::query_as("SELECT * FROM wm_entities WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    if let Some(e) = &e {
        cache.insert(id.to_string(), e.clone());
    }
    Ok(e)
}

/// Returns the failing relations as human-readable lines; deletes them when `delete`.
pub async fn audit_relations(pool: &SqlitePool, delete: bool) -> Result<(AuditStats, Vec<String>)> {
    let edges: Vec<EdgeRow> = sqlx::query_as(
        "SELECT id, from_id, to_id, edge_type, source_item_ids FROM wm_edges WHERE edge_type <> 'mentioned_with'",
    )
    .fetch_all(pool)
    .await?;
    let mut stats = AuditStats::default();
    let mut failures = Vec::new();
    let mut cache: HashMap<String, WmEntity> = HashMap::new();
    let mut texts: HashMap<String, String> = HashMap::new();

    for (edge_id, from_id, to_id, edge_type, source_ids) in edges {
        stats.checked += 1;
        let (Some(a), Some(b)) = (
            entity(pool, &mut cache, &from_id).await?,
            entity(pool, &mut cache, &to_id).await?,
        ) else {
            continue;
        };
        let ids: Vec<String> = serde_json::from_str(&source_ids).unwrap_or_default();
        // Grounded if ANY of the relation's sources supports it.
        let mut supported = false;
        let mut had_text = false;
        for sid in &ids {
            if !texts.contains_key(sid) {
                let row: Option<(String,)> =
                    sqlx::query_as("SELECT raw_text FROM wm_source_items WHERE id = ?")
                        .bind(sid)
                        .fetch_optional(pool)
                        .await?;
                texts.insert(
                    sid.clone(),
                    row.map(|(t,)| t.to_lowercase()).unwrap_or_default(),
                );
            }
            let text = &texts[sid];
            if text.is_empty() {
                continue;
            }
            had_text = true;
            let a_names: Vec<String> = std::iter::once(a.canonical_name.clone())
                .chain(a.alias_list())
                .collect();
            let b_names: Vec<String> = std::iter::once(b.canonical_name.clone())
                .chain(b.alias_list())
                .collect();
            let a_refs: Vec<&str> = a_names.iter().map(String::as_str).collect();
            let b_refs: Vec<&str> = b_names.iter().map(String::as_str).collect();
            let (Some(sa), Some(sb)) = (
                ground::occurring_name(text, &a_refs),
                ground::occurring_name(text, &b_refs),
            ) else {
                continue;
            };
            if ground::relation_grounded(text, sa, sb, &edge_type, &a.entity_type, &b.entity_type) {
                supported = true;
                break;
            }
        }
        if !had_text {
            stats.no_source_text += 1;
            continue;
        }
        if supported {
            stats.kept += 1;
        } else {
            stats.failed += 1;
            failures.push(format!(
                "{} —{}→ {}  [{}]",
                a.canonical_name, edge_type, b.canonical_name, edge_id
            ));
            if delete {
                sqlx::query("DELETE FROM wm_edges WHERE id = ?")
                    .bind(&edge_id)
                    .execute(pool)
                    .await?;
                stats.deleted += 1;
            }
        }
    }
    Ok((stats, failures))
}

/// Entities whose names fail today's `plausible_name` gate (graphs built before the gate keep
/// them). With `delete`, removes them and everything hanging off them; candidates that
/// pointed at them are detached rather than deleted, so the mention evidence survives.
pub async fn prune_entities(pool: &SqlitePool, delete: bool) -> Result<(usize, Vec<String>)> {
    let rows: Vec<(String, String, String)> =
        sqlx::query_as("SELECT id, canonical_name, entity_type FROM wm_entities")
            .fetch_all(pool)
            .await?;
    let checked = rows.len();
    let mut failing = Vec::new();
    for (id, name, etype) in rows {
        if crate::extract::plausible_name(&name, &etype) {
            continue;
        }
        failing.push(format!("{name} [{etype}] {id}"));
        if delete {
            sqlx::query("UPDATE wm_extraction_candidates SET best_match_entity_id = NULL, status = 'rejected_name' WHERE best_match_entity_id = ?")
                .bind(&id)
                .execute(pool)
                .await?;
            for sql in [
                "DELETE FROM wm_edges WHERE from_id = ? OR to_id = ?",
                "DELETE FROM wm_decorrelations WHERE entity_a_id = ? OR entity_b_id = ?",
            ] {
                sqlx::query(sql).bind(&id).bind(&id).execute(pool).await?;
            }
            for sql in [
                "DELETE FROM wm_entity_keys WHERE entity_id = ?",
                "DELETE FROM wm_geo WHERE entity_id = ?",
                "DELETE FROM wm_entities WHERE id = ?",
            ] {
                sqlx::query(sql).bind(&id).execute(pool).await?;
            }
        }
    }
    Ok((checked, failing))
}
