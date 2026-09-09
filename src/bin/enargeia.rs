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
    Ask {
        question: String,
        /// Answer from the graph as it was at this time (RFC3339 or YYYY-MM-DD).
        #[arg(long)]
        as_of: Option<String>,
    },
    /// Show the provenance trace for an entity (id or name): mentions, evidence, relations
    /// with validity windows, human decisions.
    Why { entity: String },
    /// Target profile from the graph: identity, dated timeline of typed relations, network,
    /// sources, and a grounded LLM assessment (markdown).
    Dossier {
        entity: String,
        /// Profile the entity as it was at this time (RFC3339 or YYYY-MM-DD).
        #[arg(long)]
        as_of: Option<String>,
        /// Write to a file instead of stdout.
        #[arg(long)]
        out: Option<String>,
        /// Skip the LLM assessment section.
        #[arg(long)]
        no_llm: bool,
    },
    /// Record a human decision on a review candidate: confirm | reject | new.
    Decide {
        candidate_id: String,
        decision: String,
        #[arg(long)]
        reason: Option<String>,
        #[arg(long, default_value = "cli")]
        actor: String,
    },
    /// List candidates awaiting review, highest score first.
    Review {
        #[arg(long, default_value = "30")]
        limit: i64,
    },
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
    /// Measure the matcher against labeled pairs.
    Eval {
        #[command(subcommand)]
        action: EvalAction,
    },
    /// Rebuild the blocking index from all entities.
    Reindex,
    /// Run a mission file: ingest its sources, resolve, enrich, answer its questions into a brief.
    Mission {
        #[command(subcommand)]
        action: MissionAction,
    },
    /// Serve the HTTP API and the globe UI.
    Serve {
        #[arg(long, env = "ENARGEIA_BIND", default_value = "127.0.0.1:8787")]
        bind: String,
        /// Bearer token required for endpoints that change the graph or call the LLM.
        #[arg(long, env = "ENARGEIA_TOKEN")]
        token: Option<String>,
    },
    /// Geocoding: fetch/load the GeoNames gazetteer and geocode location entities.
    Geo {
        #[command(subcommand)]
        action: GeoAction,
    },
    /// Scoped API tokens for analysts and clients (see README "Clients and scoped tokens").
    Token {
        #[command(subcommand)]
        action: TokenAction,
    },
    /// Standing watch: ingest a mission's sources on a cadence, resolve, enrich a bounded
    /// number of items, write a digest, and escalate only what warrants it.
    Watch {
        /// Mission file whose sources to poll (omit to only re-resolve what is already ingested).
        #[arg(long)]
        mission: Option<String>,
        #[arg(long, default_value = "3600")]
        interval_secs: u64,
        #[arg(long, default_value = "40")]
        enrich_limit: usize,
        /// Run one cycle and exit.
        #[arg(long)]
        once: bool,
        /// Also poll a USGS feed (e.g. 2.5_day) for major and non-seismic events.
        #[arg(long)]
        quakes: Option<String>,
        #[arg(long, default_value = "watch")]
        out: String,
        /// Generic JSON webhook that receives a digest only when there are escalations.
        #[arg(long, env = "ENARGEIA_ALERT_WEBHOOK")]
        webhook: Option<String>,
    },
}

#[derive(Subcommand)]
enum GeoAction {
    /// Download the GeoNames cities15000 + countryInfo files (CC BY 4.0).
    Fetch,
    /// Load the downloaded gazetteer into the database.
    Load,
    /// Geocode live location-type entities that have no coordinates yet.
    Code {
        /// Re-geocode every location entity, not only the missing ones.
        #[arg(long)]
        all: bool,
    },
    /// Gazetteer and geocoding counts.
    Status,
}

#[derive(Subcommand)]
enum MissionAction {
    /// Run a mission TOML file end to end; writes missions/<name>/BRIEF.md and graph.json.
    Run {
        file: String,
        /// Skip fetching sources (answer from what is already in the database).
        #[arg(long)]
        skip_ingest: bool,
        /// Skip Tier 2 enrichment.
        #[arg(long)]
        skip_enrich: bool,
        /// Maximum source items to enrich with the LLM.
        #[arg(long, default_value = "120")]
        enrich_limit: usize,
        /// Output directory (default missions/<name>).
        #[arg(long)]
        out: Option<String>,
    },
}

#[derive(Subcommand)]
enum TokenAction {
    /// Create a scoped token; the plaintext is printed once and never stored.
    Create {
        /// Human label, e.g. "pcg-dashboard".
        #[arg(long)]
        name: String,
        /// operator | analyst | client
        #[arg(long, default_value = "client")]
        role: String,
        /// Maximum `ask` calls per UTC day (omit for unlimited).
        #[arg(long)]
        ask_daily_limit: Option<i64>,
        /// Cite only commercially clean sources for this token (default for clients).
        #[arg(long)]
        commercial_only: bool,
        /// Allow a client token to cite every source class (overrides the client default).
        #[arg(long)]
        any_source: bool,
    },
    /// List tokens (never shows plaintext).
    List,
    /// Revoke a token by id or name.
    Revoke { token: String },
}

#[derive(Subcommand)]
enum ModelsAction {
    /// Download the default GLiNER NER model into the model directory.
    Fetch {
        /// Also download the int8 export (select it with ENARGEIA_NER_ONNX=model_int8.onnx).
        #[arg(long)]
        int8: bool,
    },
}

#[derive(Subcommand)]
enum EvalAction {
    /// Score eval/labels.jsonl with the matcher and write eval/REPORT.md.
    Match {
        #[arg(long, default_value = "eval/labels.jsonl")]
        labels: String,
        #[arg(long, default_value = "eval/REPORT.md")]
        out: String,
    },
    /// Prove on a scratch database that a rejected merge never re-merges; writes
    /// eval/decorrelation_report.md.
    Decorrelation {
        #[arg(long, default_value = "eval/decorrelation_report.md")]
        out: String,
    },
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
        Command::Ask { question, as_of } => {
            let client = llm::LlmClient::from_env()?;
            let as_of = as_of.map(|t| {
                if t.len() == 10 {
                    format!("{t}T23:59:59+00:00")
                } else {
                    t
                }
            });
            let answer = context::ask(&pool, &client, &question, as_of.as_deref()).await?;
            println!("{answer}");
        }
        Command::Dossier {
            entity,
            as_of,
            out,
            no_llm,
        } => {
            let client = if no_llm {
                None
            } else {
                Some(llm::LlmClient::from_env()?)
            };
            let as_of = as_of.map(|t| {
                if t.len() == 10 {
                    format!("{t}T23:59:59+00:00")
                } else {
                    t
                }
            });
            let opts = enargeia::dossier::DossierOptions {
                as_of: as_of.as_deref(),
                license_filter: None,
            };
            let md = enargeia::dossier::dossier(&pool, client.as_ref(), &entity, &opts).await?;
            match out {
                Some(path) => {
                    std::fs::write(&path, &md)?;
                    println!("wrote {path}");
                }
                None => println!("{md}"),
            }
        }
        Command::Why { entity } => {
            print!("{}", enargeia::why::why(&pool, &entity).await?);
        }
        Command::Decide {
            candidate_id,
            decision,
            reason,
            actor,
        } => {
            let msg =
                resolve::apply_decision(&pool, &candidate_id, &decision, &actor, reason.as_deref())
                    .await?;
            println!("{msg}");
        }
        Command::Review { limit } => {
            // (candidate id, mention, label, entity name, entity type, score, feature_scores)
            type ReviewRow = (
                String,
                String,
                Option<String>,
                String,
                String,
                Option<f64>,
                Option<String>,
            );
            let rows: Vec<ReviewRow> = sqlx::query_as(
                "SELECT c.id, c.mention_text, c.mention_type_guess, e.canonical_name, e.entity_type, c.match_score, c.feature_scores \
                 FROM wm_extraction_candidates c JOIN wm_entities e ON e.id = c.best_match_entity_id \
                 WHERE c.status = 'pending_review' ORDER BY c.match_score DESC LIMIT ?",
            )
            .bind(limit)
            .fetch_all(&pool)
            .await?;
            if rows.is_empty() {
                println!("no candidates awaiting review");
            }
            for (id, mention, label, cand, ctype, score, features) in rows {
                let breakdown = features
                    .as_deref()
                    .and_then(|f| serde_json::from_str::<enargeia::matcher::MatchScore>(f).ok())
                    .map(|ms| {
                        ms.contributions
                            .iter()
                            .map(|(k, v)| format!("{k} {v:+.1}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                println!(
                    "{id}\n  \"{mention}\" [{}] → {cand} [{ctype}]  score {:.1}\n  {breakdown}\n  decide: enargeia decide {id} confirm|reject|new",
                    label.as_deref().unwrap_or("?"),
                    score.unwrap_or(0.0)
                );
            }
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
            action: ModelsAction::Fetch { int8 },
        } => {
            let dir = enargeia::extract::model_dir_from_env();
            enargeia::extract::fetch_model(&dir, int8).await?;
            println!("model ready at {}", dir.display());
        }
        Command::Eval {
            action: EvalAction::Match { labels, out },
        } => {
            let weights = enargeia::matcher::Weights::from_env();
            let outcome = enargeia::eval::run(std::path::Path::new(&labels), &weights)?;
            std::fs::write(&out, &outcome.report_md)?;
            println!(
                "{} pairs · baseline (JW≥0.87) P {:.3} R {:.3} F1 {:.3} · merge (≥{:.1}) P {:.3} R {:.3} F1 {:.3} · merge-or-review R {:.3} · review {:.1}%",
                outcome.pairs,
                outcome.baseline.precision(),
                outcome.baseline.recall(),
                outcome.baseline.f1(),
                weights.upper,
                outcome.probabilistic.precision(),
                outcome.probabilistic.recall(),
                outcome.probabilistic.f1(),
                outcome.merge_or_review.recall(),
                outcome.review_rate * 100.0
            );
            println!("wrote {out}");
        }
        Command::Eval {
            action: EvalAction::Decorrelation { out },
        } => {
            let scratch = "data/eval-decorrelation.sqlite";
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{scratch}{suffix}"));
            }
            let scratch_pool = db::connect(scratch).await?;
            let outcome = enargeia::eval::run_decorrelation(&scratch_pool).await?;
            std::fs::write(&out, &outcome.report_md)?;
            println!("{}", outcome.report_md);
            println!("wrote {out}");
            if !outcome.passed {
                anyhow::bail!("decorrelation evaluation FAILED");
            }
        }
        Command::Mission {
            action:
                MissionAction::Run {
                    file,
                    skip_ingest,
                    skip_enrich,
                    enrich_limit,
                    out,
                },
        } => {
            let mission = enargeia::mission::load(std::path::Path::new(&file))?;
            println!(
                "mission: {} ({} questions)",
                mission.name,
                mission.questions.len()
            );
            let opts = enargeia::mission::RunOptions {
                skip_ingest,
                skip_enrich,
                enrich_limit,
                out_dir: out.map(std::path::PathBuf::from),
            };
            let report = enargeia::mission::run(&pool, &mission, &opts).await?;
            println!(
                "mission complete: {} new items, {} resolved, {} enriched ({} errors), {} answers\nbrief: {}\ngraph: {}",
                report.new_items,
                report.resolved_items,
                report.enriched_items,
                report.enrich_errors,
                report.answers,
                report.brief_path.display(),
                report.graph_path.display()
            );
        }
        Command::Token { action } => match action {
            TokenAction::Create {
                name,
                role,
                ask_daily_limit,
                commercial_only,
                any_source,
            } => {
                let role = enargeia::auth::Role::parse(&role)?;
                let filter = if any_source {
                    None
                } else if commercial_only || role == enargeia::auth::Role::Client {
                    Some("commercial_clean")
                } else {
                    None
                };
                let (id, plain) =
                    enargeia::auth::create_token(&pool, &name, role, ask_daily_limit, filter)
                        .await?;
                println!("token id: {id}\nrole: {}\nask_daily_limit: {}\nlicense_filter: {}\n\n{plain}\n\nStore it now; it is not recoverable.", role.as_str(), ask_daily_limit.map(|n| n.to_string()).unwrap_or_else(|| "unlimited".into()), filter.unwrap_or("any"));
            }
            TokenAction::List => {
                for t in enargeia::auth::list_tokens(&pool).await? {
                    println!(
                        "{}  {:<20} {:<9} limit={:<9} filter={:<16} created={} {}{}",
                        t.id,
                        t.name,
                        t.role,
                        t.ask_daily_limit
                            .map(|n| n.to_string())
                            .unwrap_or_else(|| "unlimited".into()),
                        t.license_filter.as_deref().unwrap_or("any"),
                        t.created_at.get(..10).unwrap_or(&t.created_at),
                        t.last_used_at
                            .as_deref()
                            .map(|u| format!("last_used={} ", u.get(..16).unwrap_or(u)))
                            .unwrap_or_default(),
                        t.revoked_at
                            .as_deref()
                            .map(|r| format!("REVOKED {}", r.get(..10).unwrap_or(r)))
                            .unwrap_or_default()
                    );
                }
            }
            TokenAction::Revoke { token } => {
                let n = enargeia::auth::revoke_token(&pool, &token).await?;
                println!("revoked {n} token(s)");
            }
        },
        Command::Watch {
            mission,
            interval_secs,
            enrich_limit,
            once,
            quakes,
            out,
            webhook,
        } => {
            let m = match mission {
                Some(path) => Some(enargeia::mission::load(std::path::Path::new(&path))?),
                None => None,
            };
            let opts = enargeia::watch::WatchOptions {
                interval: std::time::Duration::from_secs(interval_secs),
                enrich_limit,
                once,
                quakes,
                out_dir: std::path::PathBuf::from(out),
                webhook,
            };
            enargeia::watch::run(&pool, m.as_ref(), &opts).await?;
        }
        Command::Serve { bind, token } => {
            enargeia::server::serve(pool, &bind, token).await?;
        }
        Command::Geo { action } => {
            let dir = enargeia::geo::gazetteer_dir();
            match action {
                GeoAction::Fetch => {
                    enargeia::geo::fetch_gazetteer(&dir).await?;
                }
                GeoAction::Load => {
                    let (p, n, c) = enargeia::geo::load_gazetteer(&pool, &dir).await?;
                    println!("loaded {p} places, {n} lookup names, {c} countries");
                }
                GeoAction::Code { all } => {
                    let s = enargeia::geo::geocode_entities(&pool, all).await?;
                    println!(
                        "geocoded {} of {} location entities",
                        s.geocoded, s.considered
                    );
                }
                GeoAction::Status => {
                    let (p, c, g) = enargeia::geo::status(&pool).await?;
                    println!("gazetteer: {p} places, {c} countries · geocoded entities: {g}");
                }
            }
        }
        Command::Reindex => {
            let n = enargeia::block::reindex_all(&pool).await?;
            println!("indexed {n} entities");
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
