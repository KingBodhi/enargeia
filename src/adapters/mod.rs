pub mod body;
pub mod celestrak;
pub mod gdelt;
pub mod pulse_reader;
pub mod rss;
pub mod usgs;

use anyhow::Result;
use async_trait::async_trait;

use crate::models::RawItem;

#[async_trait]
pub trait SourceAdapter: Send + Sync {
    fn name(&self) -> &str;

    /// `commercial_clean` | `non_commercial_only` | `unknown`. Most "free" feeds in this
    /// space are non-commercial-only (ACLED, GTD, Cloudflare Radar, Global Fishing Watch),
    /// so the class is asserted per adapter, never assumed. See DATA-ATTRIBUTION.md.
    fn license_class(&self) -> &str {
        "unknown"
    }

    async fn fetch(&self) -> Result<Vec<RawItem>>;
}
