use anyhow::Result;
use clap::{Parser, Subcommand};
use enargeia::{
    adapters::{
        gdelt::GdeltAdapter, pulse_reader::PulseContentReader, rss::RssAdapter, SourceAdapter,
    },
    context, db, llm, llm_enrich, resolve,
};

#[derive(Parser)]
#[command(
    name = "enargeia",
    version,
    about = "Enargeia — entity-resolution engine for open-source intelligence"
)]
struct Cli {
    /// Path to the engine's SQLite database (created if missing).
    #[arg(long, env = "ENARGEIA_DB_PATH", default_value = "data/enargeia.sqlite")]
    db_path: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run all configured adapters once; dedup and store new items.
    Ingest {
        /// RSS/Atom feed URLs to pull.
        #[arg(long)]
        rss: Vec<String>,
        /// GDELT DOC 2.0 search queries (commercially clean source).
        #[arg(long)]
        gdelt: Vec<String>,
    },
    /// Tier 1: deterministic resolution over pending items (no LLM).
    Resolve {
        #[arg(long, default_value = "200")]
        limit: i64,
    },
    /// Tier 2: one batched LLM enrichment pass over items Tier 1 could not resolve.
    Enrich {
        #[arg(long, default_value = "20")]
        batch_size: i64,
    },
    /// Answer a question from the resolved graph, with source citations.
    Ask { question: String },
    /// Record that two entities are NOT the same; no future merge will re-propose the pair.
    Decorrelate {
        entity_a: String,
        entity_b: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Fold `absorb` into `keep`. Refuses if the pair was decorrelated.
    Merge { keep: String, absorb: String },
    /// Mark entities past their expiry as not live.
    Expire,
    /// Print counts: entities, edges, pending review, sources by license class.
    Status,
    /// Manage local models.
    Models {
        #[command(subcommand)]
        action: ModelsAction,
    },
}

#[derive(Subcommand)]
enum ModelsAction {
    /// Download the default GLiNER NER model into the model directory.
    Fetch,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    let pool = db::connect(&cli.db_path).await?;

    match cli.command {
        Command::Ingest { rss, gdelt } => {
            let mut adapters: Vec<Box<dyn SourceAdapter>> = Vec::new();
            if !rss.is_empty() {
                adapters.push(Box::new(RssAdapter::new(rss)));
            }
            if !gdelt.is_empty() {
                adapters.push(Box::new(GdeltAdapter::new(gdelt)));
            }
            let mut pulse_reader: Option<PulseContentReader> = None;
            if let Ok(pulse_path) = std::env::var("ENARGEIA_PULSE_DB_PATH") {
                let since = load_checkpoint(&pool, "pulse").await?;
                match PulseContentReader::connect(&pulse_path, since).await {
                    Ok(reader) => pulse_reader = Some(reader),
                    Err(e) => {
                        tracing::warn!(error = %e, "ENARGEIA_PULSE_DB_PATH set but could not connect - skipping")
                    }
                }
            }
            if adapters.is_empty() && pulse_reader.is_none() {
                eprintln!("No adapters configured - pass --rss <url> / --gdelt <query>, or set ENARGEIA_PULSE_DB_PATH.");
                return Ok(());
            }

            let mut total_new = 0usize;
            for adapter in &adapters {
                let items = adapter.fetch().await?;
                let mut new_for_adapter = 0usize;
                for item in &items {
                    if resolve::ingest_item(&pool, item).await?.is_some() {
                        new_for_adapter += 1;
                    }
                }
                println!(
                    "[{}] fetched {} items, {} new",
                    adapter.name(),
                    items.len(),
                    new_for_adapter
                );
                total_new += new_for_adapter;
            }
            if let Some(reader) = &pulse_reader {
                let items = reader.fetch().await?;
                let mut new_for_adapter = 0usize;
                for item in &items {
                    if resolve::ingest_item(&pool, item).await?.is_some() {
                        new_for_adapter += 1;
                    }
                }
                println!(
                    "[pulse] fetched {} items, {} new",
                    items.len(),
                    new_for_adapter
                );
                total_new += new_for_adapter;
                save_checkpoint(&pool, "pulse", &reader.last_cursor()).await?;
            }
            println!("ingest complete: {total_new} new source items");
        }
        Command::Resolve { limit } => {
            let stats = resolve::resolve_pending(&pool, limit).await?;
            println!(
                "resolved {} items: {} auto-merged mentions, {} new entities, {} flagged needs_llm, {} pending_review",
                stats.items_processed, stats.auto_merged, stats.new_entities, stats.needs_llm, stats.pending_review
            );
        }
        Command::Enrich { batch_size } => {
            let client = llm::LlmClient::from_env()?;
            println!("llm: {}", client.describe());
            let stats = llm_enrich::enrich_batch(&pool, &client, batch_size).await?;
            println!(
                "enriched {} items: {} entities updated, {} edges created, {} errors",
                stats.candidates_processed,
                stats.entities_updated,
                stats.edges_created,
                stats.errors
            );
        }
        Command::Ask { question } => {
            let client = llm::LlmClient::from_env()?;
            let answer = context::ask(&pool, &client, &question).await?;
            println!("{answer}");
        }
        Command::Decorrelate {
            entity_a,
            entity_b,
            reason,
        } => {
            resolve::decorrelate(&pool, &entity_a, &entity_b, reason.as_deref()).await?;
            println!(
                "decorrelated {entity_a} <-> {entity_b}; no future merge will re-propose this pair"
            );
        }
        Command::Merge { keep, absorb } => {
            resolve::merge_entities(&pool, &keep, &absorb).await?;
            println!("merged {absorb} into {keep}");
        }
        Command::Expire => {
            let n = resolve::expire_stale_entities(&pool).await?;
            println!("expired {n} stale entities (is_live=0)");
        }
        Command::Models {
            action: ModelsAction::Fetch,
        } => {
            let dir = enargeia::extract::model_dir_from_env();
            enargeia::extract::fetch_model(&dir).await?;
            println!("model ready at {}", dir.display());
        }
        Command::Status => {
            let (entities,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM wm_entities")
                .fetch_one(&pool)
                .await?;
            let (live,): (i64,) =
                sqlx::query_as("SELECT COUNT(*) FROM wm_entities WHERE is_live = 1")
                    .fetch_one(&pool)
                    .await?;
            let (edges,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM wm_edges")
                .fetch_one(&pool)
                .await?;
            let (decor,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM wm_decorrelations")
                .fetch_one(&pool)
                .await?;
            let candidates: Vec<(String, i64)> =
                sqlx::query_as("SELECT status, COUNT(*) FROM wm_extraction_candidates GROUP BY status ORDER BY status")
                    .fetch_all(&pool)
                    .await?;
            let sources: Vec<(String, String, i64)> = sqlx::query_as(
                "SELECT source_type, license_class, COUNT(*) FROM wm_source_items GROUP BY source_type, license_class ORDER BY 1,2",
            )
            .fetch_all(&pool)
            .await?;
            println!(
                "entities: {entities} ({live} live) · edges: {edges} · decorrelations: {decor}"
            );
            for (status, n) in candidates {
                println!("candidates.{status}: {n}");
            }
            for (st, lc, n) in sources {
                println!("sources.{st}.{lc}: {n}");
            }
        }
    }

    Ok(())
}

async fn load_checkpoint(pool: &sqlx::SqlitePool, adapter: &str) -> Result<String> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT cursor FROM wm_adapter_checkpoints WHERE adapter_name = ?")
            .bind(adapter)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(c,)| c).unwrap_or_default())
}

async fn save_checkpoint(pool: &sqlx::SqlitePool, adapter: &str, cursor: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO wm_adapter_checkpoints (adapter_name, cursor, updated_at) VALUES (?, ?, ?) \
         ON CONFLICT(adapter_name) DO UPDATE SET cursor = excluded.cursor, updated_at = excluded.updated_at",
    )
    .bind(adapter)
    .bind(cursor)
    .bind(enargeia::models::now())
    .execute(pool)
    .await?;
    Ok(())
}
