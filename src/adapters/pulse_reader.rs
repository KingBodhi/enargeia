//! Optional, read-only adapter over pcg-cc-mcp's existing Pulse Engine content.
//!
//! Only activates if `PULSE_DB_PATH` is set. Opens that SQLite file with its own pool,
//! never writes to it, and uses runtime `sqlx::query` (not `query!`) so this crate never
//! needs pcg-cc-mcp's schema at compile time — keeps the "don't decide where this lives
//! yet" decoupling real, not just aspirational.

use std::{str::FromStr, sync::Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    Row, SqlitePool,
};

use super::SourceAdapter;
use crate::models::RawItem;

pub struct PulseContentReader {
    pool: SqlitePool,
    /// in-memory cursor for this process run; persistent checkpointing happens in wm-cli
    /// via `wm_adapter_checkpoints`, passed in as the starting value.
    since: Mutex<String>,
}

impl PulseContentReader {
    pub async fn connect(pulse_db_path: &str, since_collected_at: String) -> Result<Self> {
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{pulse_db_path}"))
            .context("invalid PULSE_DB_PATH")?
            .read_only(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(opts)
            .await
            .context("failed to open pcg-cc-mcp pulse DB read-only")?;
        Ok(Self {
            pool,
            since: Mutex::new(since_collected_at),
        })
    }

    pub fn last_cursor(&self) -> String {
        self.since.lock().unwrap().clone()
    }
}

#[async_trait]
impl SourceAdapter for PulseContentReader {
    fn name(&self) -> &str {
        "pulse"
    }

    async fn fetch(&self) -> Result<Vec<RawItem>> {
        let since = self.last_cursor();
        let rows = sqlx::query(
            "SELECT id, url, title, COALESCE(body, summary, '') as text, collected_at \
             FROM pulse_content_items \
             WHERE collected_at > ? \
             ORDER BY collected_at ASC \
             LIMIT 200",
        )
        .bind(&since)
        .fetch_all(&self.pool)
        .await
        .context("query against pulse_content_items failed")?;

        let mut items = Vec::new();
        let mut max_seen = since;
        for row in rows {
            let id: String = row.try_get("id")?;
            let url: String = row.try_get("url")?;
            let title: String = row.try_get("title")?;
            let text: String = row.try_get("text")?;
            let collected_at: String = row.try_get("collected_at")?;
            if collected_at > max_seen {
                max_seen = collected_at.clone();
            }
            if text.trim().is_empty() {
                continue;
            }
            items.push(RawItem {
                source_type: "pulse".to_string(),
                source_ref: if url.is_empty() { id } else { url },
                title: if title.is_empty() { None } else { Some(title) },
                text,
                license_class: "unknown".to_string(),
            });
        }
        *self.since.lock().unwrap() = max_seen;
        Ok(items)
    }
}
