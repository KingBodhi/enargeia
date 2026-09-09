//! Tier 2: selective LLM enrichment. Runs only against `wm_extraction_candidates` that
//! Tier 1 could not confidently resolve (`status = 'needs_llm'`), in bounded batches, never
//! inline with ingestion. LLM output is never trusted blindly: every proposed entity goes
//! back through the same matcher as any other mention.

use anyhow::Result;
use serde_json::Value;
use sqlx::SqlitePool;

use crate::llm::{extract_json_object, LlmClient};

const SYSTEM_PROMPT: &str = "You extract real-world entities and their relations from a short \
text passage. Respond with ONLY a JSON object, no prose, matching this shape exactly: \
{\"entities\":[{\"mention\":string,\"canonical_name\":string,\"entity_type\":string,\"confidence\":number(0-1)}],\
\"relations\":[{\"a\":string,\"b\":string,\"relation_type\":string}]}. \
entity_type must be one of: person, organization, location, event, product, vessel, aircraft, satellite, other. \
canonical_name is the normalized real-world name (e.g. \"Recorded Future\" not \"RF\" or \"the company\"). \
relation_type is a short snake_case verb phrase; prefer these when they apply: ceo_of, cfo_of, cto_of, \
founder_of, chairman_of, headquartered_in, based_in, owned_by, acquired, acquired_by, invested_in, \
partner_of, suing, employs, member_of, located_in, launched, competes_with. \
Every relation must be an object with keys a, b, relation_type, where a and b are mention strings from entities. \
If nothing is extractable, return {\"entities\":[],\"relations\":[]}.";

#[derive(Debug, Default)]
struct LlmExtraction {
    entities: Vec<LlmEntity>,
    relations: Vec<LlmRelation>,
}

#[derive(Debug)]
struct LlmEntity {
    mention: String,
    canonical_name: String,
    entity_type: String,
    confidence: f64,
}

#[derive(Debug)]
struct LlmRelation {
    a: String,
    b: String,
    relation_type: String,
}

fn str_at<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Lenient decoder: small local models produce duplicate keys, array-shaped relations, and
/// stray fields. Parsing to `Value` first (last duplicate wins) and reading fields by hand
/// recovers most of those responses instead of discarding the whole item.
fn decode_extraction(v: &Value) -> LlmExtraction {
    let mut out = LlmExtraction::default();

    if let Some(items) = v.get("entities").and_then(Value::as_array) {
        for item in items {
            let (mention, canonical, etype, conf) = match item {
                Value::Object(_) => (
                    str_at(item, &["mention", "text", "name"]),
                    str_at(item, &["canonical_name", "canonical", "name"]),
                    str_at(item, &["entity_type", "type", "label"]),
                    item.get("confidence").and_then(Value::as_f64),
                ),
                Value::Array(parts) => (
                    parts.first().and_then(Value::as_str),
                    parts.get(1).and_then(Value::as_str),
                    parts.get(2).and_then(Value::as_str),
                    parts.get(3).and_then(Value::as_f64),
                ),
                Value::String(s) => (Some(s.as_str()), Some(s.as_str()), None, None),
                _ => (None, None, None, None),
            };
            let mention = mention.or(canonical);
            let Some(mention) = mention else { continue };
            out.entities.push(LlmEntity {
                mention: mention.to_string(),
                canonical_name: canonical.unwrap_or(mention).to_string(),
                entity_type: etype.unwrap_or("other").to_lowercase(),
                confidence: conf.unwrap_or(0.6).clamp(0.0, 1.0),
            });
        }
    }

    if let Some(items) = v.get("relations").and_then(Value::as_array) {
        for item in items {
            let rel = match item {
                Value::Object(_) => (
                    str_at(item, &["a", "from", "subject", "source"]),
                    str_at(item, &["b", "to", "object", "target"]),
                    str_at(item, &["relation_type", "relation", "type", "predicate"]),
                ),
                Value::Array(parts) if parts.len() >= 3 => {
                    (parts[0].as_str(), parts[1].as_str(), parts[2].as_str())
                }
                _ => (None, None, None),
            };
            if let (Some(a), Some(b), Some(t)) = rel {
                out.relations.push(LlmRelation {
                    a: a.to_string(),
                    b: b.to_string(),
                    relation_type: normalize_relation(t),
                });
            }
        }
    }
    out
}

fn normalize_relation(t: &str) -> String {
    let s: String = t
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    let collapsed = s
        .split('_')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("_");
    if collapsed.is_empty() {
        "related_to".to_string()
    } else {
        collapsed.chars().take(48).collect()
    }
}

#[derive(Debug, Default)]
pub struct EnrichStats {
    pub candidates_processed: usize,
    pub entities_updated: usize,
    pub edges_created: usize,
    pub errors: usize,
}

/// Processes up to `batch_size` source items that have `needs_llm` candidates — one LLM call
/// per item, so co-mentioned entities share a call instead of one-per-mention.
pub async fn enrich_batch(
    pool: &SqlitePool,
    client: &LlmClient,
    batch_size: i64,
) -> Result<EnrichStats> {
    let mut stats = EnrichStats::default();

    let source_ids: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT source_item_id FROM wm_extraction_candidates WHERE status = 'needs_llm' LIMIT ?",
    )
    .bind(batch_size)
    .fetch_all(pool)
    .await?;

    for (source_item_id,) in source_ids {
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT raw_text, COALESCE(published_at, ingested_at) FROM wm_source_items WHERE id = ?",
        )
        .bind(&source_item_id)
        .fetch_optional(pool)
        .await?;
        let Some((text, observed_at)) = row else {
            continue;
        };

        let snippet: String = text.chars().take(4000).collect();
        let response = match client.complete(SYSTEM_PROMPT, &snippet, 1536, true).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(source_item_id, error = %e, "LLM enrichment call failed");
                stats.errors += 1;
                continue;
            }
        };

        let Some(json) = extract_json_object(&response) else {
            tracing::warn!(source_item_id, raw = %response, "LLM response contained no JSON object");
            stats.errors += 1;
            continue;
        };
        let value: Value = match serde_json::from_str(json) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(source_item_id, error = %e, raw = %response, "LLM returned unparseable JSON");
                stats.errors += 1;
                continue;
            }
        };
        let parsed = decode_extraction(&value);

        let mut mention_to_entity: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();

        for llm_entity in &parsed.entities {
            let eid = crate::resolve::upsert_resolved_entity(
                pool,
                &llm_entity.mention,
                &llm_entity.canonical_name,
                &llm_entity.entity_type,
                llm_entity.confidence,
            )
            .await?;
            mention_to_entity.insert(llm_entity.mention.to_lowercase(), eid.clone());
            mention_to_entity.insert(llm_entity.canonical_name.to_lowercase(), eid);
            stats.entities_updated += 1;
        }

        for rel in &parsed.relations {
            let a = mention_to_entity.get(&rel.a.to_lowercase());
            let b = mention_to_entity.get(&rel.b.to_lowercase());
            if let (Some(a), Some(b)) = (a, b) {
                crate::resolve::link_entities_typed(
                    pool,
                    a,
                    b,
                    &rel.relation_type,
                    &source_item_id,
                    &observed_at,
                )
                .await?;
                stats.edges_created += 1;
            }
        }

        // Resolve every candidate tied to this item whether or not the LLM matched it — an item
        // that comes back empty must not loop forever in `needs_llm`.
        sqlx::query("UPDATE wm_extraction_candidates SET status = 'resolved' WHERE source_item_id = ? AND status = 'needs_llm'")
            .bind(&source_item_id)
            .execute(pool)
            .await?;
        sqlx::query("UPDATE wm_source_items SET status = 'resolved' WHERE id = ?")
            .bind(&source_item_id)
            .execute(pool)
            .await?;

        stats.candidates_processed += 1;
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_duplicate_keys_and_array_relations() {
        let raw = r#"{"entities":[{"mention":"UAE","canonical_name":"United Arab Emirates","canonical_name":"United Arab Emirates","entity_type":"location","confidence":0.8},
            {"mention":"Fed","canonical_name":"Federal Reserve System","entity_type":"organization","confidence":0.9}],
            "relations":[["Fed","UAE","talks_with"],{"a":"UAE","b":"Fed","relation_type":"Hosted Talks With"}]}"#;
        let v: Value = serde_json::from_str(raw).unwrap();
        let x = decode_extraction(&v);
        assert_eq!(x.entities.len(), 2);
        assert_eq!(x.entities[0].canonical_name, "United Arab Emirates");
        assert_eq!(x.relations.len(), 2);
        assert_eq!(x.relations[0].relation_type, "talks_with");
        assert_eq!(x.relations[1].relation_type, "hosted_talks_with");
    }

    #[test]
    fn tolerates_missing_fields() {
        let v: Value =
            serde_json::from_str(r#"{"entities":[{"name":"Anthropic"}],"relations":"none"}"#)
                .unwrap();
        let x = decode_extraction(&v);
        assert_eq!(x.entities.len(), 1);
        assert_eq!(x.entities[0].entity_type, "other");
        assert!(x.relations.is_empty());
    }
}
