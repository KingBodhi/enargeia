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
    /// Transaction time: when Enargeia first/last observed the relation.
    pub first_seen: String,
    pub last_seen: String,
    /// Valid time: when the relation held in the world (from the source's publication date).
    pub valid_at: Option<String>,
    /// Set when a newer, contradicting relation of an exclusive type superseded this one.
    pub invalid_at: Option<String>,
    pub superseded_by: Option<String>,
}

impl WmEdge {
    pub fn source_ids(&self) -> Vec<String> {
        serde_json::from_str(&self.source_item_ids).unwrap_or_default()
    }
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
    /// Publication time reported by the source (RFC3339), when available.
    pub published_at: Option<String>,
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
    pub feature_scores: Option<String>,
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
    /// Publication time (RFC3339) if the source reports one.
    pub published_at: Option<String>,
}

/// Best-effort parse of the date formats feeds actually emit into RFC3339.
pub fn parse_feed_date(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(t.with_timezone(&Utc).to_rfc3339());
    }
    if let Ok(t) = chrono::DateTime::parse_from_rfc2822(s) {
        return Some(t.with_timezone(&Utc).to_rfc3339());
    }
    // RFC 2822 with a wrong or missing weekday (feeds get this wrong): drop the weekday.
    let no_weekday = s.split_once(',').map(|(_, rest)| rest.trim()).unwrap_or(s);
    for fmt in [
        "%d %b %Y %H:%M:%S %z",
        "%d %b %Y %H:%M %z",
        "%d %b %Y %H:%M:%S GMT",
    ] {
        if let Ok(t) = chrono::DateTime::parse_from_str(no_weekday, fmt) {
            return Some(t.with_timezone(&Utc).to_rfc3339());
        }
        if let Ok(t) = chrono::NaiveDateTime::parse_from_str(
            no_weekday,
            fmt.trim_end_matches(" %z").trim_end_matches(" GMT"),
        ) {
            return Some(t.and_utc().to_rfc3339());
        }
    }
    // GDELT: 20260908T143000Z
    if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%M%SZ") {
        return Some(t.and_utc().to_rfc3339());
    }
    // ISO without zone: 2026-09-08T14:30:00 / 2026-09-08 14:30:00
    for fmt in [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S%.f",
    ] {
        if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Some(t.and_utc().to_rfc3339());
        }
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(d.and_hms_opt(0, 0, 0)?.and_utc().to_rfc3339());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_feed_dates() {
        assert!(parse_feed_date("Tue, 08 Sep 2026 14:30:00 +0000").is_some());
        // wrong weekday, as some feeds emit
        assert!(parse_feed_date("Mon, 08 Sep 2026 14:30:00 +0000").is_some());
        assert!(parse_feed_date("08 Sep 2026 14:30:00 GMT").is_some());
        assert!(parse_feed_date("2026-09-08T14:30:00Z").is_some());
        assert_eq!(
            parse_feed_date("20260908T143000Z").as_deref(),
            Some("2026-09-08T14:30:00+00:00")
        );
        assert!(parse_feed_date("2026-09-08").is_some());
        assert!(parse_feed_date("not a date").is_none());
    }
}
