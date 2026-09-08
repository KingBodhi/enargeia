//! Tier 2: selective LLM enrichment. Only runs against `wm_extraction_candidates`
//! Tier 1 couldn't confidently resolve (`status = 'needs_llm'`). Run periodically
//! (CLI subcommand / cron), never inline with ingestion - keeps cost bounded.

use anyhow::Result;
use serde::Deserialize;
use sqlx::SqlitePool;

use crate::llm::AnthropicClient;

const SYSTEM_PROMPT: &str = "You extract real-world entities and their relations from a short \
text passage. Respond with ONLY a JSON object, no prose, matching this shape exactly: \
{\"entities\":[{\"mention\":string,\"canonical_name\":string,\"entity_type\":string,\"confidence\":number(0-1)}],\
\"relations\":[{\"a\":string,\"b\":string,\"relation_type\":string}]}. \
entity_type should be one of: person, organization, location, product, event, other. \
canonical_name should be the normalized real-world name (e.g. \"Recorded Future\" not \"RF\" or \"the company\"). \
If nothing extractable, return {\"entities\":[],\"relations\":[]}.";

#[derive(Debug, Deserialize)]
struct LlmExtraction {
    entities: Vec<LlmEntity>,
    #[serde(default)]
    relations: Vec<LlmRelation>,
}

#[derive(Debug, Deserialize)]
struct LlmEntity {
    mention: String,
    canonical_name: String,
    entity_type: String,
    confidence: f64,
}

#[derive(Debug, Deserialize)]
struct LlmRelation {
    a: String,
    b: String,
    relation_type: String,
}

#[derive(Debug, Default)]
pub struct EnrichStats {
    pub candidates_processed: usize,
    pub entities_updated: usize,
    pub edges_created: usize,
    pub errors: usize,
}

/// Processes up to `batch_size` `needs_llm` candidates, one LLM call per distinct source item
/// (so co-mentioned entities in the same text share one call instead of one-per-mention).
pub async fn enrich_batch(
    pool: &SqlitePool,
    client: &AnthropicClient,
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
        let raw_text: Option<(String,)> =
            sqlx::query_as("SELECT raw_text FROM wm_source_items WHERE id = ?")
                .bind(&source_item_id)
                .fetch_optional(pool)
                .await?;
        let Some((text,)) = raw_text else { continue };

        let snippet: String = text.chars().take(4000).collect();
        let response = match client.complete(SYSTEM_PROMPT, &snippet, 1024).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(source_item_id, error = %e, "LLM enrichment call failed");
                stats.errors += 1;
                continue;
            }
        };

        let parsed: LlmExtraction = match serde_json::from_str(response.trim()) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(source_item_id, error = %e, raw = %response, "LLM returned unparseable JSON");
                stats.errors += 1;
                continue;
            }
        };

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
            mention_to_entity.insert(llm_entity.mention.clone(), eid);
            stats.entities_updated += 1;
        }

        for rel in &parsed.relations {
            if let (Some(a), Some(b)) =
                (mention_to_entity.get(&rel.a), mention_to_entity.get(&rel.b))
            {
                crate::resolve::link_entities_typed(
                    pool,
                    a,
                    b,
                    &rel.relation_type,
                    &source_item_id,
                )
                .await?;
                stats.edges_created += 1;
            }
        }

        // resolve every candidate tied to this source item, whether the LLM matched it or not -
        // an item that comes back empty still shouldn't loop forever in `needs_llm`.
        sqlx::query(
            "UPDATE wm_extraction_candidates SET status = 'resolved' WHERE source_item_id = ? AND status = 'needs_llm'",
        )
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
