//! Minimal LLM client for Tier 2 enrichment and `ask`. Local-first by default: the
//! reasoning tier must not require a cloud account.
//!
//! Provider selection (`ENARGEIA_LLM_PROVIDER`):
//! - `openai` — any OpenAI-compatible chat endpoint (Ollama, vLLM, LM Studio, OpenAI).
//!   `ENARGEIA_LLM_BASE_URL` (default `http://localhost:11434`), `OPENAI_API_KEY` optional.
//! - `anthropic` — Anthropic Messages API, `ANTHROPIC_API_KEY` required.
//!
//! If unset: a configured base URL implies `openai`; otherwise an `ANTHROPIC_API_KEY`
//! implies `anthropic`; otherwise `openai` against local Ollama.
//! Model: `ENARGEIA_LLM_MODEL` (defaults `qwen2.5:7b` / `claude-sonnet-5`).

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_OPENAI_BASE: &str = "http://localhost:11434";
const DEFAULT_OPENAI_MODEL: &str = "qwen2.5:7b";
const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-5";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    OpenAiCompat,
    Anthropic,
}

pub struct LlmClient {
    provider: Provider,
    api_key: Option<String>,
    base_url: String,
    model: String,
    http: reqwest::Client,
}

impl LlmClient {
    pub fn from_env() -> Result<Self> {
        let explicit = std::env::var("ENARGEIA_LLM_PROVIDER").ok();
        let base_url = std::env::var("ENARGEIA_LLM_BASE_URL").ok();
        let anthropic_key = std::env::var("ANTHROPIC_API_KEY").ok();

        let provider = match explicit.as_deref() {
            Some("anthropic") => Provider::Anthropic,
            Some("openai") => Provider::OpenAiCompat,
            Some(other) => {
                bail!("unknown ENARGEIA_LLM_PROVIDER {other:?} (expected openai|anthropic)")
            }
            None if base_url.is_some() => Provider::OpenAiCompat,
            None if anthropic_key.is_some() => Provider::Anthropic,
            None => Provider::OpenAiCompat,
        };

        let (api_key, base_url, default_model) = match provider {
            Provider::Anthropic => (
                Some(anthropic_key.context("ANTHROPIC_API_KEY not set")?),
                ANTHROPIC_URL.to_string(),
                DEFAULT_ANTHROPIC_MODEL,
            ),
            Provider::OpenAiCompat => (
                std::env::var("OPENAI_API_KEY").ok(),
                base_url
                    .unwrap_or_else(|| DEFAULT_OPENAI_BASE.to_string())
                    .trim_end_matches('/')
                    .to_string(),
                DEFAULT_OPENAI_MODEL,
            ),
        };
        let model =
            std::env::var("ENARGEIA_LLM_MODEL").unwrap_or_else(|_| default_model.to_string());

        Ok(Self {
            provider,
            api_key,
            base_url,
            model,
            http: reqwest::Client::new(),
        })
    }

    pub fn describe(&self) -> String {
        match self.provider {
            Provider::Anthropic => format!("anthropic · {}", self.model),
            Provider::OpenAiCompat => {
                format!("openai-compatible @ {} · {}", self.base_url, self.model)
            }
        }
    }

    /// One system + user turn. `want_json` asks providers that support it to constrain output
    /// to a JSON object; callers must still parse defensively.
    pub async fn complete(
        &self,
        system: &str,
        user: &str,
        max_tokens: u32,
        want_json: bool,
    ) -> Result<String> {
        match self.provider {
            Provider::Anthropic => self.complete_anthropic(system, user, max_tokens).await,
            Provider::OpenAiCompat => {
                self.complete_openai(system, user, max_tokens, want_json)
                    .await
            }
        }
    }

    async fn complete_anthropic(
        &self,
        system: &str,
        user: &str,
        max_tokens: u32,
    ) -> Result<String> {
        let body = json!({
            "model": self.model,
            "max_tokens": max_tokens,
            "system": system,
            "messages": [{"role": "user", "content": user}],
        });
        let resp = self
            .http
            .post(&self.base_url)
            .header("x-api-key", self.api_key.as_deref().unwrap_or_default())
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
        Ok(parsed["content"][0]["text"]
            .as_str()
            .context("anthropic response missing content[0].text")?
            .to_string())
    }

    async fn complete_openai(
        &self,
        system: &str,
        user: &str,
        max_tokens: u32,
        want_json: bool,
    ) -> Result<String> {
        let mut body = json!({
            "model": self.model,
            "max_tokens": max_tokens,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
        });
        if want_json {
            body["response_format"] = json!({"type": "json_object"});
        }
        let url = format!("{}/v1/chat/completions", self.base_url);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(self.api_key.as_deref().unwrap_or("local"))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("request to {url} failed"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("LLM API error {status}: {text}");
        }
        let parsed: Value = resp
            .json()
            .await
            .context("failed to parse chat completion response")?;
        Ok(parsed["choices"][0]["message"]["content"]
            .as_str()
            .context("response missing choices[0].message.content")?
            .to_string())
    }
}

/// Returns the first balanced `{ ... }` block in `s`, tolerating prose or stray characters
/// around it (small local models often append a trailing `;` or a sentence).
pub fn extract_json_object(s: &str) -> Option<&str> {
    let start = s.find('{')?;
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_object_with_trailing_junk() {
        let s = r#"{"entities":[{"a":"b}"}]};"#;
        assert_eq!(extract_json_object(s), Some(r#"{"entities":[{"a":"b}"}]}"#));
    }

    #[test]
    fn extracts_object_inside_prose() {
        let s = "Sure! Here you go: {\"x\": 1} hope that helps";
        assert_eq!(extract_json_object(s), Some("{\"x\": 1}"));
    }

    #[test]
    fn none_without_object() {
        assert_eq!(extract_json_object("no json here"), None);
    }
}
