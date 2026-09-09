//! Context assembler: the "reason over the graph, not raw documents" hop. Resolves a
//! free-text question to matching entities, walks `wm_edges` to a bounded depth, and
//! formats a source-cited context for the LLM to answer from.

use std::collections::{HashSet, VecDeque};

use anyhow::Result;
use sqlx::SqlitePool;

use crate::{
    llm::LlmClient,
    models::{WmEdge, WmEntity},
};

const MATCH_THRESHOLD: f64 = 0.55;
const MAX_SEED_MATCHES: usize = 5;
const DEFAULT_DEPTH: usize = 2;

/// Only entities the resolver considers current. Reasoning runs over live facts; resolution
/// in `resolve.rs` still dedups against full history so stale entities are not re-created.
async fn live_entities(pool: &SqlitePool) -> Result<Vec<WmEntity>> {
    Ok(
        sqlx::query_as::<_, WmEntity>("SELECT * FROM wm_entities WHERE is_live = 1")
            .fetch_all(pool)
            .await?,
    )
}

/// Free-text match against canonical_name + aliases, best-scoring first. Live entities only.
pub async fn find_matching_entities(pool: &SqlitePool, query: &str) -> Result<Vec<WmEntity>> {
    let entities = live_entities(pool).await?;
    let q = query.to_lowercase();
    let mut scored: Vec<(f64, WmEntity)> = entities
        .into_iter()
        .map(|e| {
            let mut names = vec![e.canonical_name.clone()];
            names.extend(e.alias_list());
            let best = names
                .iter()
                .map(|n| strsim::jaro_winkler(&q, &n.to_lowercase()))
                .fold(0.0_f64, f64::max);
            (best, e)
        })
        .filter(|(s, _)| *s >= MATCH_THRESHOLD)
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
    Ok(scored
        .into_iter()
        .take(MAX_SEED_MATCHES)
        .map(|(_, e)| e)
        .collect())
}

struct GraphSlice {
    entities: Vec<WmEntity>,
    edges: Vec<WmEdge>,
}

async fn bfs(pool: &SqlitePool, seeds: &[String], depth: usize) -> Result<GraphSlice> {
    let all_edges: Vec<WmEdge> = sqlx::query_as::<_, WmEdge>("SELECT * FROM wm_edges")
        .fetch_all(pool)
        .await?;
    let mut visited: HashSet<String> = seeds.iter().cloned().collect();
    let mut frontier: VecDeque<(String, usize)> = seeds.iter().map(|s| (s.clone(), 0)).collect();
    let mut used_edges: Vec<WmEdge> = Vec::new();

    while let Some((id, d)) = frontier.pop_front() {
        if d >= depth {
            continue;
        }
        for edge in &all_edges {
            let neighbor = if edge.from_id == id {
                Some(edge.to_id.clone())
            } else if edge.to_id == id {
                Some(edge.from_id.clone())
            } else {
                None
            };
            if let Some(n) = neighbor {
                used_edges.push(edge.clone());
                if visited.insert(n.clone()) {
                    frontier.push_back((n, d + 1));
                }
            }
        }
    }

    let ids: Vec<String> = visited.into_iter().collect();
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!("SELECT * FROM wm_entities WHERE id IN ({placeholders})");
    let mut q = sqlx::query_as::<_, WmEntity>(&sql);
    for id in &ids {
        q = q.bind(id);
    }
    let entities: Vec<WmEntity> = q.fetch_all(pool).await.unwrap_or_default();

    Ok(GraphSlice {
        entities,
        edges: used_edges,
    })
}

fn format_context(slice: &GraphSlice) -> String {
    let mut out = String::new();
    out.push_str("KNOWN ENTITIES:\n");
    for e in &slice.entities {
        out.push_str(&format!(
            "- [{}] {} (type: {}, confidence: {:.2})\n",
            &e.id[..8.min(e.id.len())],
            e.canonical_name,
            e.entity_type,
            e.confidence
        ));
    }
    out.push_str("\nRELATIONS:\n");
    let mut seen_edges = HashSet::new();
    for edge in &slice.edges {
        if !seen_edges.insert(edge.id.clone()) {
            continue;
        }
        let name = |id: &str| {
            slice
                .entities
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.canonical_name.as_str())
                .unwrap_or("?")
        };
        out.push_str(&format!(
            "- {} --[{}]--> {} (sources: {})\n",
            name(&edge.from_id),
            edge.edge_type,
            name(&edge.to_id),
            edge.source_item_ids
        ));
    }
    out
}

const ASK_SYSTEM_PROMPT: &str = "You answer questions using ONLY the provided entity/relation \
context. If the context does not contain the answer, say so plainly; do not guess or use outside \
knowledge. Cite entity names exactly as they appear in the context.";

pub async fn ask(pool: &SqlitePool, client: &LlmClient, question: &str) -> Result<String> {
    let seeds = find_matching_entities(pool, question).await?;
    if seeds.is_empty() {
        return Ok("No entities in the graph match this question yet; ingest and resolve more content first.".to_string());
    }
    let seed_ids: Vec<String> = seeds.iter().map(|e| e.id.clone()).collect();
    let slice = bfs(pool, &seed_ids, DEFAULT_DEPTH).await?;
    let context = format_context(&slice);
    let prompt = format!("CONTEXT:\n{context}\n\nQUESTION: {question}");
    client
        .complete(ASK_SYSTEM_PROMPT, &prompt, 1024, false)
        .await
}
