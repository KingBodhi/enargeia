//! USGS earthquake feeds (GeoJSON, public domain, no key). Besides earthquakes the feed
//! classifies `explosion`, `quarry blast`, `mining explosion` and similar — the non-seismic
//! event types are the interesting ones for a watch.

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};

pub const DEFAULT_FEED: &str = "2.5_day";
pub const FEEDS: &[&str] = &[
    "all_hour",
    "all_day",
    "1.0_day",
    "2.5_day",
    "4.5_day",
    "2.5_week",
    "4.5_week",
    "significant_month",
];

#[derive(Debug, Clone, Serialize)]
pub struct QuakeEvent {
    pub id: String,
    pub mag: Option<f64>,
    pub place: String,
    pub time: String,
    pub event_type: String,
    pub title: String,
    pub url: String,
    pub lat: f64,
    pub lon: f64,
    pub depth_km: f64,
    pub status: String,
}

impl QuakeEvent {
    pub fn is_non_seismic(&self) -> bool {
        self.event_type != "earthquake"
    }
}

pub fn check_feed(feed: &str) -> Result<()> {
    if FEEDS.contains(&feed) {
        Ok(())
    } else {
        Err(anyhow!(
            "unknown USGS feed {feed:?}; expected one of {FEEDS:?}"
        ))
    }
}

pub async fn fetch(feed: &str) -> Result<Vec<QuakeEvent>> {
    check_feed(feed)?;
    let client = reqwest::Client::builder()
        .user_agent(crate::USER_AGENT)
        .build()?;
    let url = format!("https://earthquake.usgs.gov/earthquakes/feed/v1.0/summary/{feed}.geojson");
    let v: Value = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("USGS request failed: {url}"))?
        .error_for_status()?
        .json()
        .await?;
    Ok(parse(&v))
}

pub fn parse(v: &Value) -> Vec<QuakeEvent> {
    let mut out = Vec::new();
    for f in v["features"].as_array().cloned().unwrap_or_default() {
        let p = &f["properties"];
        let coords = f["geometry"]["coordinates"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let (Some(lon), Some(lat)) = (
            coords.first().and_then(Value::as_f64),
            coords.get(1).and_then(Value::as_f64),
        ) else {
            continue;
        };
        let time_ms = p["time"].as_i64().unwrap_or(0);
        let time = chrono::DateTime::from_timestamp_millis(time_ms)
            .map(|t| t.to_rfc3339())
            .unwrap_or_default();
        out.push(QuakeEvent {
            id: f["id"].as_str().unwrap_or("").to_string(),
            mag: p["mag"].as_f64(),
            place: p["place"].as_str().unwrap_or("").to_string(),
            time,
            event_type: p["type"].as_str().unwrap_or("earthquake").to_string(),
            title: p["title"].as_str().unwrap_or("").to_string(),
            url: p["url"].as_str().unwrap_or("").to_string(),
            lat,
            lon,
            depth_km: coords.get(2).and_then(Value::as_f64).unwrap_or(0.0),
            status: p["status"].as_str().unwrap_or("").to_string(),
        });
    }
    out
}

pub fn to_geojson(
    events: &[QuakeEvent],
    feed: &str,
    fetched_at: chrono::DateTime<chrono::Utc>,
) -> Value {
    let feats: Vec<Value> = events
        .iter()
        .map(|e| {
            json!({
                "type": "Feature",
                "geometry": {"type": "Point", "coordinates": [e.lon, e.lat]},
                "properties": e,
            })
        })
        .collect();
    json!({
        "type": "FeatureCollection",
        "properties": {"feed": feed, "at": fetched_at.to_rfc3339(), "count": feats.len(), "source": "USGS", "license_class": "commercial_clean"},
        "features": feats
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_feed_and_flags_non_seismic() {
        let v: Value = serde_json::json!({
            "type": "FeatureCollection",
            "features": [
                {"type": "Feature", "id": "us1",
                 "properties": {"mag": 4.7, "place": "139 km NNE of Hihifo, Tonga", "time": 1788956294599i64,
                                "type": "earthquake", "title": "M 4.7 - Tonga", "url": "https://x/us1", "status": "reviewed"},
                 "geometry": {"type": "Point", "coordinates": [-173.1193, -14.8771, 10]}},
                {"type": "Feature", "id": "nv2",
                 "properties": {"mag": 1.9, "place": "Nevada", "time": 1788956294599i64,
                                "type": "explosion", "title": "M 1.9 Explosion - Nevada", "url": "https://x/nv2", "status": "automatic"},
                 "geometry": {"type": "Point", "coordinates": [-115.0, 37.0, 0]}},
                {"type": "Feature", "id": "bad", "properties": {}, "geometry": {"type": "Point", "coordinates": []}}
            ]
        });
        let events = parse(&v);
        assert_eq!(events.len(), 2, "malformed geometry is skipped");
        assert_eq!(events[0].lat, -14.8771);
        assert_eq!(events[0].depth_km, 10.0);
        assert!(events[0].time.starts_with("2026-09-"));
        assert!(!events[0].is_non_seismic());
        assert!(events[1].is_non_seismic());
        assert!(check_feed("2.5_day").is_ok());
        assert!(check_feed("../etc").is_err());
    }
}
