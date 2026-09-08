pub mod gdelt;
pub mod pulse_reader;
pub mod rss;

use anyhow::Result;
use async_trait::async_trait;

use crate::models::RawItem;

#[async_trait]
pub trait SourceAdapter: Send + Sync {
    fn name(&self) -> &str;

    /// 'commercial_clean' | 'non_commercial_only' | 'unknown'. See the feed licensing map in
    /// memory `osint_defense_landscape_2026_09_08` - most "free" feeds in this space are
    /// non-commercial-only (ACLED, GTD, Cloudflare Radar, Global Fishing Watch), so this is
    /// asserted per-adapter, not assumed, and gates any future commercial output.
    fn license_class(&self) -> &str {
        "unknown"
    }

    async fn fetch(&self) -> Result<Vec<RawItem>>;
}
