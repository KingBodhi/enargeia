use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub fn now() -> String {
    Utc::now().to_rfc3339()
}

pub fn new_id() -> String {
    Uuid::new_v4().to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct WmEntity {
    pub id: String,
    pub entity_type: String,
    pub canonical_name: String,
    pub aliases: String, // JSON array
    pub external_ids: String,
    pub confidence: f64,
    pub metadata: String,
    pub first_seen: String,
    pub last_seen: String,
    /// Lattice-inspired liveness: entities decay unless refreshed by a fresh source touch.
    pub is_live: bool,
    /// NULL = no expiry (Lattice's `noExpiry`). Otherwise ISO8601; refreshed on every touch.
    pub expiry_time: Option<String>,
    pub last_source_update_time: Option<String>,
}

impl WmEntity {
    pub fn alias_list(&self) -> Vec<String> {
        serde_json::from_str(&self.aliases).unwrap_or_default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct WmEdge {
    pub id: String,
    pub from_id: String,
    pub to_id: String,
    pub edge_type: String,
    pub weight: f64,
    pub metadata: String,
    pub source_item_ids: String,
    pub first_seen: String,
    pub last_seen: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct WmSourceItem {
    pub id: String,
    pub source_type: String,
    pub source_ref: String,
    pub content_hash: String,
    pub title: Option<String>,
    pub raw_text: String,
    pub ingested_at: String,
    pub status: String,
    /// 'commercial_clean' | 'non_commercial_only' | 'unknown' - set per-adapter at ingest time.
    pub license_class: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct WmExtractionCandidate {
    pub id: String,
    pub source_item_id: String,
    pub mention_text: String,
    pub mention_type_guess: Option<String>,
    pub best_match_entity_id: Option<String>,
    pub match_score: Option<f64>,
    pub status: String,
    pub created_at: String,
}

/// A raw item as produced by any [`crate::adapters::SourceAdapter`], before dedup/persistence.
#[derive(Debug, Clone)]
pub struct RawItem {
    pub source_type: String,
    pub source_ref: String,
    pub title: Option<String>,
    pub text: String,
    /// 'commercial_clean' | 'non_commercial_only' | 'unknown' - see [`crate::adapters::SourceAdapter::license_class`].
    pub license_class: String,
}
