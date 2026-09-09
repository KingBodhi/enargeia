//! Missions: a question set plus the sources to answer it from, as a TOML file. `mission run`
//! ingests, resolves, enriches, answers each question from the graph, and writes a sourced
//! `BRIEF.md` with a `graph.json` export beside it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::{
    adapters::{gdelt::GdeltAdapter, rss::RssAdapter, SourceAdapter},
    context, llm, llm_enrich,
    models::{WmEdge, WmEntity},
    resolve,
};

#[derive(Debug, Deserialize)]
pub struct Mission {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Who the mission is about; kept in the brief as the standing scope statement.
    #[serde(default)]
    pub target_class: String,
    #[serde(default)]
    pub rss: Vec<String>,
    #[serde(default)]
    pub gdelt: Vec<String>,
    #[serde(default)]
    pub questions: Vec<String>,
    #[serde(default)]
    pub as_of: Option<String>,
}

pub fn load(path: &Path) -> Result<Mission> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

pub struct RunOptions {
    pub skip_ingest: bool,
    pub skip_enrich: bool,
    pub enrich_limit: usize,
    pub out_dir: Option<PathBuf>,
}

#[derive(Debug, Default)]
pub struct MissionReport {
    pub new_items: usize,
    pub resolved_items: usize,
    pub enriched_items: usize,
    pub enrich_errors: usize,
    pub answers: usize,
    pub brief_path: PathBuf,
    pub graph_path: PathBuf,
}

#[derive(Serialize)]
struct GraphExport<'a> {
    mission: &'a str,
    generated_at: String,
    entities: Vec<ExportEntity>,
    edges: Vec<ExportEdge>,
}

#[derive(Serialize)]
struct ExportEntity {
    id: String,
    name: String,
    entity_type: String,
    aliases: Vec<String>,
    confidence: f64,
    is_live: bool,
}

#[derive(Serialize)]
struct ExportEdge {
    from: String,
    to: String,
    edge_type: String,
    weight: f64,
    valid_at: Option<String>,
    invalid_at: Option<String>,
    sources: Vec<String>,
}

pub async fn run(pool: &SqlitePool, mission: &Mission, opts: &RunOptions) -> Result<MissionReport> {
    let mut report = MissionReport::default();
    let out_dir = opts
        .out_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("missions").join(&mission.name));
    std::fs::create_dir_all(&out_dir)?;

    if !opts.skip_ingest {
        let mut adapters: Vec<Box<dyn SourceAdapter>> = Vec::new();
        if !mission.rss.is_empty() {
            adapters.push(Box::new(RssAdapter::new(mission.rss.clone())));
        }
        if !mission.gdelt.is_empty() {
            adapters.push(Box::new(GdeltAdapter::new(mission.gdelt.clone())));
        }
        for adapter in &adapters {
            let items = adapter.fetch().await?;
            let mut new_here = 0;
            for item in &items {
                if resolve::ingest_item(pool, item).await?.is_some() {
                    new_here += 1;
                }
            }
            println!(
                "[{}] fetched {} items, {} new",
                adapter.name(),
                items.len(),
                new_here
            );
            report.new_items += new_here;
        }
    }

    let stats = resolve::resolve_pending(pool, 100_000).await?;
    report.resolved_items = stats.items_processed;
    println!(
        "resolved {} items: {} auto-merged, {} new entities, {} review",
        stats.items_processed, stats.auto_merged, stats.new_entities, stats.pending_review
    );

    let client = llm::LlmClient::from_env()?;
    if !opts.skip_enrich {
        println!("llm: {}", client.describe());
        while report.enriched_items < opts.enrich_limit {
            let batch = (opts.enrich_limit - report.enriched_items).min(10) as i64;
            let s = llm_enrich::enrich_batch(pool, &client, batch).await?;
            if s.candidates_processed == 0 && s.errors == 0 {
                break;
            }
            report.enriched_items += s.candidates_processed;
            report.enrich_errors += s.errors;
            println!(
                "enriched {} (total {}) · {} entities updated · {} edges · {} errors",
                s.candidates_processed,
                report.enriched_items,
                s.entities_updated,
                s.edges_created,
                s.errors
            );
            if s.candidates_processed == 0 {
                break;
            }
        }
    }

    // Brief
    let mut md = String::new();
    md.push_str(&format!("# {} — intelligence brief\n\n", mission.name));
    md.push_str(&format!(
        "Generated {} by `enargeia mission run`. {}\n\n",
        chrono::Utc::now().format("%Y-%m-%d %H:%M UTC"),
        if mission.description.is_empty() {
            String::new()
        } else {
            mission.description.clone()
        }
    ));
    if !mission.target_class.is_empty() {
        md.push_str(&format!("**Scope:** {}\n\n", mission.target_class));
    }
    let (n_items,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM wm_source_items")
        .fetch_one(pool)
        .await?;
    let (n_ent,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM wm_entities WHERE is_live = 1")
        .fetch_one(pool)
        .await?;
    let (n_edges,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM wm_edges WHERE invalid_at IS NULL")
            .fetch_one(pool)
            .await?;
    let (n_typed,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM wm_edges WHERE invalid_at IS NULL AND edge_type <> 'mentioned_with'",
    )
    .fetch_one(pool)
    .await?;
    let lic: Vec<(String, i64)> = sqlx::query_as(
        "SELECT license_class, COUNT(*) FROM wm_source_items GROUP BY 1 ORDER BY 2 DESC",
    )
    .fetch_all(pool)
    .await?;
    md.push_str(&format!(
        "**Corpus:** {n_items} source items ({}); **graph:** {n_ent} live entities, {n_edges} valid relations ({n_typed} typed).\n\n",
        lic.iter().map(|(l, n)| format!("{n} {l}")).collect::<Vec<_>>().join(", ")
    ));
    md.push_str("Every claim below cites its source by number; the graph slice used for each answer is noted. Answers come only from the resolved graph — if the sources did not support a claim, the answer says so.\n\n");

    for (i, q) in mission.questions.iter().enumerate() {
        println!("asking {}/{}: {q}", i + 1, mission.questions.len());
        let answer = context::ask(pool, &client, q, mission.as_of.as_deref()).await?;
        md.push_str(&format!("## {}. {q}\n\n{answer}\n\n", i + 1));
        report.answers += 1;
    }

    // Appendix: what the graph knows.
    let by_type: Vec<(String, i64)> =
        sqlx::query_as("SELECT entity_type, COUNT(*) FROM wm_entities WHERE is_live = 1 GROUP BY 1 ORDER BY 2 DESC")
            .fetch_all(pool)
            .await?;
    md.push_str("## Appendix A — entity counts by type\n\n");
    for (t, n) in by_type {
        md.push_str(&format!("- {t}: {n}\n"));
    }
    let top: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT e.canonical_name, e.entity_type, COUNT(DISTINCT c.source_item_id) AS n \
         FROM wm_entities e JOIN wm_extraction_candidates c ON c.best_match_entity_id = e.id \
         WHERE c.status IN ('auto_merged','resolved','confirmed') GROUP BY e.id ORDER BY n DESC LIMIT 25",
    )
    .fetch_all(pool)
    .await?;
    md.push_str("\n## Appendix B — most corroborated entities (distinct sources)\n\n");
    for (name, t, n) in top {
        md.push_str(&format!("- {name} [{t}] — {n}\n"));
    }
    let typed: Vec<(String, String, String, f64, Option<String>)> = sqlx::query_as(
        "SELECT a.canonical_name, e.edge_type, b.canonical_name, e.weight, e.valid_at FROM wm_edges e \
         JOIN wm_entities a ON a.id = e.from_id JOIN wm_entities b ON b.id = e.to_id \
         WHERE e.invalid_at IS NULL AND e.edge_type <> 'mentioned_with' ORDER BY e.weight DESC LIMIT 60",
    )
    .fetch_all(pool)
    .await?;
    md.push_str("\n## Appendix C — typed relations (current)\n\n");
    for (a, t, b, w, v) in typed {
        md.push_str(&format!(
            "- {a} —{t}→ {b} (weight {w:.0}{})\n",
            v.as_deref()
                .map(|d| format!(", since {}", d.get(..10).unwrap_or(d)))
                .unwrap_or_default()
        ));
    }

    let brief_path = out_dir.join("BRIEF.md");
    std::fs::write(&brief_path, &md)?;

    // Graph export
    let entities: Vec<WmEntity> = sqlx::query_as("SELECT * FROM wm_entities WHERE is_live = 1")
        .fetch_all(pool)
        .await?;
    let edges: Vec<WmEdge> = sqlx::query_as("SELECT * FROM wm_edges WHERE invalid_at IS NULL")
        .fetch_all(pool)
        .await?;
    let export = GraphExport {
        mission: &mission.name,
        generated_at: chrono::Utc::now().to_rfc3339(),
        entities: entities
            .iter()
            .map(|e| ExportEntity {
                id: e.id.clone(),
                name: e.canonical_name.clone(),
                entity_type: e.entity_type.clone(),
                aliases: e.alias_list(),
                confidence: e.confidence,
                is_live: e.is_live,
            })
            .collect(),
        edges: edges
            .iter()
            .map(|e| ExportEdge {
                from: e.from_id.clone(),
                to: e.to_id.clone(),
                edge_type: e.edge_type.clone(),
                weight: e.weight,
                valid_at: e.valid_at.clone(),
                invalid_at: e.invalid_at.clone(),
                sources: e.source_ids(),
            })
            .collect(),
    };
    let graph_path = out_dir.join("graph.json");
    std::fs::write(&graph_path, serde_json::to_string_pretty(&export)?)?;

    report.brief_path = brief_path;
    report.graph_path = graph_path;
    Ok(report)
}
