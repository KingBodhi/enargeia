use anyhow::Result;
use clap::{Parser, Subcommand};
use entity_resolver::{
    adapters::{
        gdelt::GdeltAdapter, pulse_reader::PulseContentReader, rss::RssAdapter, SourceAdapter,
    },
    context, db, llm, llm_enrich, resolve,
};

#[derive(Parser)]
#[command(name = "wm-cli", about = "Pythia World Model - entity resolution CLI")]
struct Cli {
    /// Path to this crate's own SQLite DB (never pcg-cc-mcp's).
    #[arg(
        long,
        env = "WM_DB_PATH",
        default_value = "crates/entity-resolver/data/worldmodel.sqlite"
    )]
    db_path: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run all configured adapters once, dedup and store new items.
    Ingest {
        /// RSS/Atom feed URLs to pull.
        #[arg(long)]
        rss: Vec<String>,
        /// GDELT DOC 2.0 search queries (commercial_clean license - see feed licensing map).
        #[arg(long)]
        gdelt: Vec<String>,
    },
    /// Run the Tier 1 deterministic resolution pass over pending items.
    Resolve {
        #[arg(long, default_value = "200")]
        limit: i64,
    },
    /// Run one Tier 2 LLM enrichment batch over items Tier 1 couldn't confidently resolve.
    Enrich {
        #[arg(long, default_value = "20")]
        batch_size: i64,
    },
    /// Ask a question, answered from the resolved graph with source citations.
    Ask { question: String },
    /// Persist a human "these are NOT the same entity" decision. No future merge will
    /// re-propose collapsing this pair.
    Decorrelate {
        entity_a: String,
        entity_b: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Fold `absorb` into `keep` (edges/candidates repointed, aliases merged, absorb deleted).
    /// Refuses if this pair was previously decorrelated.
    Merge { keep: String, absorb: String },
    /// Flip is_live=0 on entities whose expiry_time has passed without a fresh source touch.
    Expire,
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
            if let Ok(pulse_path) = std::env::var("PULSE_DB_PATH") {
                let since = load_pulse_checkpoint(&pool).await?;
                match PulseContentReader::connect(&pulse_path, since).await {
                    Ok(reader) => pulse_reader = Some(reader),
                    Err(e) => {
                        tracing::warn!(error = %e, "PULSE_DB_PATH set but could not connect - skipping")
                    }
                }
            }
            if adapters.is_empty() && pulse_reader.is_none() {
                eprintln!("No adapters configured - pass --rss <url> and/or set PULSE_DB_PATH.");
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
                save_pulse_checkpoint(&pool, &reader.last_cursor()).await?;
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
            let client = llm::AnthropicClient::from_env()?;
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
            let client = llm::AnthropicClient::from_env()?;
            let answer = context::ask(&pool, &client, &question).await?;
            println!("{answer}");
        }
        Command::Decorrelate {
            entity_a,
            entity_b,
            reason,
        } => {
            resolve::decorrelate(&pool, &entity_a, &entity_b, reason.as_deref()).await?;
            println!("decorrelated {entity_a} <-> {entity_b} - no future merge will re-propose this pair");
        }
        Command::Merge { keep, absorb } => {
            resolve::merge_entities(&pool, &keep, &absorb).await?;
            println!("merged {absorb} into {keep}");
        }
        Command::Expire => {
            let n = resolve::expire_stale_entities(&pool).await?;
            println!("expired {n} stale entities (is_live=0)");
        }
    }

    Ok(())
}

async fn load_pulse_checkpoint(pool: &sqlx::SqlitePool) -> Result<String> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT cursor FROM wm_adapter_checkpoints WHERE adapter_name = 'pulse'")
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(c,)| c).unwrap_or_default())
}

async fn save_pulse_checkpoint(pool: &sqlx::SqlitePool, cursor: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO wm_adapter_checkpoints (adapter_name, cursor, updated_at) VALUES ('pulse', ?, ?) \
         ON CONFLICT(adapter_name) DO UPDATE SET cursor = excluded.cursor, updated_at = excluded.updated_at",
    )
    .bind(cursor)
    .bind(entity_resolver::models::now())
    .execute(pool)
    .await?;
    Ok(())
}
