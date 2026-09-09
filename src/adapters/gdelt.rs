//! GDELT DOC 2.0 API adapter — the first `commercial_clean` source: no auth, 15-minute
//! underlying cadence, and terms that explicitly permit commercial use and redistribution.
//!
//! Limitation: `artlist` mode returns article metadata (title/url/domain/date), not body
//! text, so `RawItem::text` is thin until a body-fetch pass exists.

use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

use super::SourceAdapter;
use crate::models::RawItem;

const DOC_API_URL: &str = "https://api.gdeltproject.org/api/v2/doc/doc";
/// GDELT explicitly asks for this ("Please limit requests to one every 5 seconds").
const MIN_REQUEST_GAP: Duration = Duration::from_secs(5);

pub struct GdeltAdapter {
    pub queries: Vec<String>,
    pub timespan: String,
    pub max_records: u32,
}

impl GdeltAdapter {
    pub fn new(queries: Vec<String>) -> Self {
        Self {
            queries,
            timespan: "1d".to_string(),
            max_records: 50,
        }
    }
}

#[derive(Debug, Deserialize)]
struct DocResponse {
    #[serde(default)]
    articles: Vec<DocArticle>,
}

#[derive(Debug, Deserialize)]
struct DocArticle {
    url: Option<String>,
    title: Option<String>,
    domain: Option<String>,
    seendate: Option<String>,
}

#[async_trait]
impl SourceAdapter for GdeltAdapter {
    fn name(&self) -> &str {
        "gdelt"
    }

    fn license_class(&self) -> &str {
        "commercial_clean"
    }

    async fn fetch(&self) -> Result<Vec<RawItem>> {
        let client = reqwest::Client::builder()
            .user_agent(crate::USER_AGENT)
            .build()?;
        let mut items = Vec::new();

        for (i, query) in self.queries.iter().enumerate() {
            if i > 0 {
                tokio::time::sleep(MIN_REQUEST_GAP).await;
            }
            let resp = client
                .get(DOC_API_URL)
                .query(&[
                    ("query", query.as_str()),
                    ("mode", "artlist"),
                    ("format", "json"),
                    ("timespan", self.timespan.as_str()),
                    ("maxrecords", &self.max_records.to_string()),
                ])
                .send()
                .await
                .with_context(|| format!("GDELT request failed for query {query:?}"))?;

            let text = resp.text().await.unwrap_or_default();
            let parsed: DocResponse = match serde_json::from_str(&text) {
                Ok(p) => p,
                Err(_) => {
                    // GDELT returns a plain-text rate-limit notice (not JSON) when throttled.
                    tracing::warn!(query, response = %text.chars().take(200).collect::<String>(), "GDELT response not JSON - likely rate-limited");
                    continue;
                }
            };

            for article in parsed.articles {
                let Some(url) = article.url else { continue };
                let title = article.title.unwrap_or_default();
                if title.is_empty() {
                    continue;
                }
                items.push(RawItem {
                    source_type: "gdelt".to_string(),
                    source_ref: url,
                    title: Some(title.clone()),
                    // DOC artlist has no body text; title (+ domain/date for traceability) is
                    // what Tier 1/2 have to work with until a body-fetch follow-up exists.
                    text: format!("{title} ({})", article.domain.unwrap_or_default(),),
                    license_class: "commercial_clean".to_string(),
                });
                let _ = article.seendate; // kept on the struct for future provenance use, unused for now
            }
        }

        Ok(items)
    }
}
