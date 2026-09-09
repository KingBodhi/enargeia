//! The standing watch: a recurring cycle that ingests a mission's sources, resolves, enriches
//! a bounded number of items, and reports what changed — with one named, low-noise
//! escalation path for the few things that warrant interrupting someone. Modeled on how a
//! command center runs: a fixed cadence and a pre-agreed channel, not ad-hoc polling.

use std::{path::PathBuf, time::Duration};

use anyhow::Result;
use serde::Serialize;
use sqlx::SqlitePool;

use crate::{
    adapters::usgs,
    llm, llm_enrich,
    mission::{self, Mission},
    models::now,
    resolve,
};

/// Relation types whose appearance is worth an escalation on its own.
const ESCALATE_TYPES: &[&str] = &[
    "suing",
    "sued_by",
    "acquired",
    "acquired_by",
    "merged_with",
    "ceo_of",
    "cfo_of",
    "resigned_from",
    "fired",
    "indicted",
    "charged",
    "sanctioned",
    "banned",
    "hacked",
    "breached",
];
const ESCALATE_QUAKE_MAG: f64 = 6.0;

pub struct WatchOptions {
    pub interval: Duration,
    pub enrich_limit: usize,
    pub once: bool,
    pub quakes: Option<String>,
    pub out_dir: PathBuf,
    pub webhook: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct EdgeLine {
    pub from: String,
    pub edge_type: String,
    pub to: String,
    pub weight: f64,
    pub valid_at: Option<String>,
}

#[derive(Debug, Serialize, Default)]
pub struct Digest {
    pub started: String,
    pub ended: String,
    pub new_items: usize,
    pub resolved_items: usize,
    pub enriched_items: usize,
    pub ungrounded: usize,
    pub new_entities: i64,
    pub expired: usize,
    pub geocoded: usize,
    pub review_pending: i64,
    pub new_typed_edges: Vec<EdgeLine>,
    pub escalations: Vec<String>,
    pub info: Vec<String>,
    pub quakes: Vec<usgs::QuakeEvent>,
}

pub async fn run_cycle(
    pool: &SqlitePool,
    mission: Option<&Mission>,
    opts: &WatchOptions,
) -> Result<Digest> {
    let mut d = Digest {
        started: now(),
        ..Default::default()
    };

    if let Some(m) = mission {
        d.new_items = mission::ingest_sources(pool, m).await?;
    }
    let stats = resolve::resolve_pending(pool, 100_000).await?;
    d.resolved_items = stats.items_processed;

    if opts.enrich_limit > 0 {
        match llm::LlmClient::from_env() {
            Ok(client) => {
                while d.enriched_items < opts.enrich_limit {
                    let batch = (opts.enrich_limit - d.enriched_items).min(10) as i64;
                    let s = llm_enrich::enrich_batch(pool, &client, batch).await?;
                    d.enriched_items += s.candidates_processed;
                    d.ungrounded += s.ungrounded;
                    if s.candidates_processed == 0 {
                        break;
                    }
                }
            }
            Err(e) => d.info.push(format!("enrichment skipped: {e}")),
        }
    }

    // Housekeeping the cadence owns: liveness decay and coordinates for new locations.
    d.expired = resolve::expire_stale_entities(pool).await?;
    match crate::geo::geocode_entities(pool, false).await {
        Ok(g) => d.geocoded = g.geocoded,
        Err(e) => d.info.push(format!("geocoding skipped: {e}")),
    }

    let (new_entities,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM wm_entities WHERE first_seen >= ?")
            .bind(&d.started)
            .fetch_one(pool)
            .await?;
    d.new_entities = new_entities;
    let (review,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM wm_extraction_candidates WHERE status = 'pending_review'",
    )
    .fetch_one(pool)
    .await?;
    d.review_pending = review;

    let edges: Vec<(String, String, String, f64, Option<String>)> = sqlx::query_as(
        "SELECT a.canonical_name, e.edge_type, b.canonical_name, e.weight, e.valid_at FROM wm_edges e \
         JOIN wm_entities a ON a.id = e.from_id JOIN wm_entities b ON b.id = e.to_id \
         WHERE e.first_seen >= ? AND e.edge_type <> 'mentioned_with' AND e.invalid_at IS NULL \
         ORDER BY e.weight DESC LIMIT 200",
    )
    .bind(&d.started)
    .fetch_all(pool)
    .await?;
    for (from, edge_type, to, weight, valid_at) in edges {
        let line = EdgeLine {
            from,
            edge_type,
            to,
            weight,
            valid_at,
        };
        let text = format!(
            "{} —{}→ {} (weight {:.0}{})",
            line.from,
            line.edge_type,
            line.to,
            line.weight,
            line.valid_at
                .as_deref()
                .map(|v| format!(", since {}", v.get(..10).unwrap_or(v)))
                .unwrap_or_default()
        );
        if ESCALATE_TYPES.contains(&line.edge_type.as_str()) || line.weight >= 2.0 {
            d.escalations.push(text);
        }
        d.new_typed_edges.push(line);
    }

    if let Some(feed) = &opts.quakes {
        match usgs::fetch(feed).await {
            Ok(events) => {
                for e in &events {
                    if e.is_non_seismic() {
                        d.escalations.push(format!(
                            "USGS non-seismic event: {} ({}) — {}",
                            e.title, e.event_type, e.url
                        ));
                    } else if e.mag.unwrap_or(0.0) >= ESCALATE_QUAKE_MAG {
                        d.escalations
                            .push(format!("USGS major earthquake: {} — {}", e.title, e.url));
                    }
                }
                d.quakes = events;
            }
            Err(e) => d.info.push(format!("USGS feed unavailable: {e}")),
        }
    }

    d.ended = now();
    Ok(d)
}

pub fn render(d: &Digest) -> String {
    let mut md = String::new();
    md.push_str(&format!(
        "# Watch digest — {}\n\n",
        d.ended.get(..16).unwrap_or(&d.ended)
    ));
    md.push_str(&format!(
        "cycle {} → {} · {} new items · {} resolved · {} enriched ({} ungrounded rejected) · {} new entities · {} expired · {} geocoded · {} awaiting review\n\n",
        d.started.get(..16).unwrap_or(&d.started),
        d.ended.get(..16).unwrap_or(&d.ended),
        d.new_items,
        d.resolved_items,
        d.enriched_items,
        d.ungrounded,
        d.new_entities,
        d.expired,
        d.geocoded,
        d.review_pending
    ));
    md.push_str(&format!("## ESCALATE ({})\n\n", d.escalations.len()));
    if d.escalations.is_empty() {
        md.push_str("_nothing warrants interruption this cycle_\n");
    }
    for e in &d.escalations {
        md.push_str(&format!("- {e}\n"));
    }
    md.push_str(&format!(
        "\n## New typed relations ({})\n\n",
        d.new_typed_edges.len()
    ));
    for e in d.new_typed_edges.iter().take(60) {
        md.push_str(&format!(
            "- {} —{}→ {} (weight {:.0})\n",
            e.from, e.edge_type, e.to, e.weight
        ));
    }
    if !d.quakes.is_empty() {
        md.push_str(&format!(
            "\n## USGS events in feed ({})\n\n",
            d.quakes.len()
        ));
        for q in d.quakes.iter().take(20) {
            md.push_str(&format!(
                "- {} · {} · {}\n",
                q.title,
                q.event_type,
                q.time.get(..16).unwrap_or(&q.time)
            ));
        }
    }
    if !d.info.is_empty() {
        md.push_str("\n## Notes\n\n");
        for i in &d.info {
            md.push_str(&format!("- {i}\n"));
        }
    }
    md
}

async fn post_webhook(url: &str, d: &Digest, md: &str) {
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "source": "enargeia",
        "kind": "watch_digest",
        "ended": d.ended,
        "escalations": d.escalations,
        "new_typed_edges": d.new_typed_edges.len(),
        "review_pending": d.review_pending,
        "digest_md": md,
    });
    match client.post(url).json(&body).send().await {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => tracing::warn!(status = %r.status(), "alert webhook rejected the digest"),
        Err(e) => tracing::warn!(error = %e, "alert webhook unreachable"),
    }
}

pub async fn run(pool: &SqlitePool, mission: Option<&Mission>, opts: &WatchOptions) -> Result<()> {
    std::fs::create_dir_all(&opts.out_dir)?;
    loop {
        let d = run_cycle(pool, mission, opts).await?;
        let md = render(&d);
        let stamp = d.ended.replace([':', '-'], "");
        let path = opts
            .out_dir
            .join(format!("digest-{}.md", stamp.get(..15).unwrap_or(&stamp)));
        std::fs::write(&path, &md)?;
        println!("{md}\nwrote {}", path.display());
        if let Some(url) = &opts.webhook {
            // Only escalations interrupt anyone; routine digests stay on disk.
            if !d.escalations.is_empty() {
                post_webhook(url, &d, &md).await;
            }
        }
        if opts.once {
            return Ok(());
        }
        tokio::time::sleep(opts.interval).await;
    }
}
