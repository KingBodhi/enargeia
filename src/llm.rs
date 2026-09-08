//! Minimal, direct Anthropic client. Deliberately NOT pcg-cc-mcp's
//! `services::services::workflow_llm::WorkflowLLMService` — that service requires a
//! `SqlitePool` wired to pcg-cc-mcp's `pcg_router_models` table for provider routing, which
//! would silently re-couple this crate to pcg-cc-mcp's live DB. A ~40-line direct client on
//! `ANTHROPIC_API_KEY` keeps Tier 2 truly standalone. See plan `synchronous-nibbling-pie.md`.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const DEFAULT_MODEL: &str = "claude-sonnet-5";
const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicClient {
    api_key: String,
    http: reqwest::Client,
    model: String,
}

impl AnthropicClient {
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .context("ANTHROPIC_API_KEY not set - required for Tier 2 (wm-cli enrich / ask)")?;
        let model = std::env::var("WM_LLM_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        Ok(Self {
            api_key,
            http: reqwest::Client::new(),
            model,
        })
    }

    /// Sends a single system+user turn, returns the raw text of the first content block.
    pub async fn complete(&self, system: &str, user: &str, max_tokens: u32) -> Result<String> {
        let body = json!({
            "model": self.model,
            "max_tokens": max_tokens,
            "system": system,
            "messages": [{"role": "user", "content": user}],
        });

        let resp = self
            .http
            .post(API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body)
            .send()
            .await
            .context("anthropic request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("anthropic API error {status}: {text}");
        }

        let parsed: Value = resp
            .json()
            .await
            .context("failed to parse anthropic response")?;
        let text = parsed["content"][0]["text"]
            .as_str()
            .context("anthropic response missing content[0].text")?
            .to_string();
        Ok(text)
    }
}
