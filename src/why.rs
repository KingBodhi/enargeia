//! `enargeia why <entity>`: the provenance trace. For an entity, every mention that resolved
//! to it (with source, tier, score and feature breakdown), every relation with its validity
//! window, and every human decision or decorrelation that touched it. "Why do you believe
//! this?" answered from data, not from a summary.

use anyhow::{Context, Result};
use sqlx::SqlitePool;

use crate::{block, matcher::MatchScore, models::WmEntity};

/// (candidate id, mention, label, score, status, feature_scores, title, source_ref, license, published_at)
type MentionRow = (
    String,
    String,
    Option<String>,
    Option<f64>,
    String,
    Option<String>,
    String,
    String,
    String,
    Option<String>,
);
/// (from_id, to_id, edge_type, weight, valid_at, invalid_at, superseded_by, source_item_ids)
type EdgeRow = (
    String,
    String,
    String,
    f64,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
);

/// Resolve an id, exact name, or fuzzy name to an entity.
pub async fn find_entity(pool: &SqlitePool, query: &str) -> Result<WmEntity> {
    if let Some(e) = sqlx::query_as::<_, WmEntity>("SELECT * FROM wm_entities WHERE id = ?")
        .bind(query)
        .fetch_optional(pool)
        .await?
    {
        return Ok(e);
    }
    if let Some(e) = sqlx::query_as::<_, WmEntity>(
        "SELECT * FROM wm_entities WHERE lower(canonical_name) = lower(?) ORDER BY confidence DESC LIMIT 1",
    )
    .bind(query)
    .fetch_optional(pool)
    .await?
    {
        return Ok(e);
    }
    block::candidates_for(pool, query)
        .await?
        .into_iter()
        .next()
        .with_context(|| format!("no entity matches {query:?}"))
}

fn short(ts: &str) -> &str {
    ts.get(..16).unwrap_or(ts)
}

fn breakdown(features: Option<&str>) -> String {
    features
        .and_then(|f| serde_json::from_str::<MatchScore>(f).ok())
        .map(|ms| {
            ms.contributions
                .iter()
                .map(|(k, v)| format!("{k} {v:+.1}"))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

async fn entity_name(pool: &SqlitePool, id: &str) -> Result<String> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT canonical_name FROM wm_entities WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(n,)| n).unwrap_or_else(|| "?".to_string()))
}

pub async fn why(pool: &SqlitePool, query: &str) -> Result<String> {
    let e = find_entity(pool, query).await?;
    let mut out = String::new();
    let aliases = e.alias_list();

    out.push_str(&format!(
        "{} [{}] id={}\n  aliases: {}\n  confidence {:.2} · live={} · first_seen {} · last_seen {} · expiry {}\n",
        e.canonical_name,
        e.entity_type,
        e.id,
        if aliases.is_empty() { "—".to_string() } else { aliases.join(", ") },
        e.confidence,
        e.is_live,
        short(&e.first_seen),
        short(&e.last_seen),
        e.expiry_time.as_deref().map(short).unwrap_or("none")
    ));

    let rows: Vec<MentionRow> = sqlx::query_as(
        "SELECT c.id, c.mention_text, c.mention_type_guess, c.match_score, c.status, c.feature_scores, \
                COALESCE(s.title, ''), s.source_ref, s.license_class, s.published_at \
         FROM wm_extraction_candidates c JOIN wm_source_items s ON s.id = c.source_item_id \
         WHERE c.best_match_entity_id = ? ORDER BY c.created_at",
    )
    .bind(&e.id)
    .fetch_all(pool)
    .await?;
    out.push_str(&format!("\nMENTIONS ({}):\n", rows.len()));
    for (cid, mention, label, score, status, features, title, source_ref, license, published) in
        rows
    {
        let tier = match (features.is_some(), status.as_str()) {
            (true, _) => "tier1",
            (false, "needs_llm" | "new" | "rejected") => "tier1, created here",
            _ => "tier2/human",
        };
        let evidence = breakdown(features.as_deref());
        out.push_str(&format!(
            "  - \"{}\" [{}] → {} ({}{}){}\n      source: {}{} — {} ({})\n      candidate {}\n",
            mention,
            label.as_deref().unwrap_or("?"),
            status,
            tier,
            score.map(|s| format!(", score {s:.1}")).unwrap_or_default(),
            if evidence.is_empty() {
                String::new()
            } else {
                format!("\n      evidence: {evidence}")
            },
            published
                .as_deref()
                .map(|p| format!("{} · ", short(p)))
                .unwrap_or_default(),
            if title.is_empty() {
                "(untitled)"
            } else {
                &title
            },
            source_ref,
            license,
            cid
        ));
    }

    let edges: Vec<EdgeRow> = sqlx::query_as(
        "SELECT from_id, to_id, edge_type, weight, valid_at, invalid_at, superseded_by, source_item_ids \
         FROM wm_edges WHERE from_id = ? OR to_id = ? ORDER BY (edge_type = 'mentioned_with'), weight DESC",
    )
    .bind(&e.id)
    .bind(&e.id)
    .fetch_all(pool)
    .await?;
    out.push_str(&format!("\nRELATIONS ({}):\n", edges.len()));
    for (from, to, etype, weight, valid, invalid, superseded, sources) in edges {
        let other_id = if from == e.id { &to } else { &from };
        let other_name = entity_name(pool, other_id).await?;
        let arrow = if from == e.id { "→" } else { "←" };
        let n_sources = serde_json::from_str::<Vec<String>>(&sources)
            .map(|v| v.len())
            .unwrap_or(0);
        let window = match (&valid, &invalid) {
            (Some(v), None) => format!("valid since {}", short(v)),
            (Some(v), Some(i)) => format!(
                "valid {} → {} (superseded{})",
                short(v),
                short(i),
                superseded
                    .as_deref()
                    .map(|s| format!(" by {}", &s[..8.min(s.len())]))
                    .unwrap_or_default()
            ),
            (None, Some(i)) => format!("invalid since {}", short(i)),
            (None, None) => "no validity recorded".to_string(),
        };
        out.push_str(&format!(
            "  - {arrow} {other_name} [{etype}] weight {weight:.0} · {window} · {n_sources} source(s)\n"
        ));
    }

    let decisions: Vec<(String, String, String, Option<String>, String)> = sqlx::query_as(
        "SELECT d.decision, d.actor, c.mention_text, d.reason, d.decided_at \
         FROM wm_decisions d JOIN wm_extraction_candidates c ON c.id = d.candidate_id \
         WHERE c.best_match_entity_id = ? ORDER BY d.decided_at",
    )
    .bind(&e.id)
    .fetch_all(pool)
    .await?;
    let decor: Vec<(String, String, Option<String>, String)> = sqlx::query_as(
        "SELECT entity_a_id, entity_b_id, reason, decided_at FROM wm_decorrelations WHERE entity_a_id = ? OR entity_b_id = ?",
    )
    .bind(&e.id)
    .bind(&e.id)
    .fetch_all(pool)
    .await?;
    let n_human = decisions.len() + decor.len();
    out.push_str(&format!("\nHUMAN DECISIONS ({n_human}):\n"));
    for (decision, actor, mention, reason, at) in decisions {
        out.push_str(&format!(
            "  - {} \"{}\" by {} at {}{}\n",
            decision,
            mention,
            actor,
            short(&at),
            reason.map(|r| format!(" — {r}")).unwrap_or_default()
        ));
    }
    for (a, b, reason, at) in decor {
        let other = if a == e.id { b } else { a };
        let name = entity_name(pool, &other).await?;
        out.push_str(&format!(
            "  - DECORRELATED from {} ({}) at {}{}\n",
            name,
            other,
            short(&at),
            reason.map(|r| format!(" — {r}")).unwrap_or_default()
        ));
    }
    if n_human == 0 {
        out.push_str("  (none)\n");
    }
    Ok(out)
}
