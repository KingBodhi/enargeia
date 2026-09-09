//! Mention extraction — the first hop of Tier 1. Zero-shot NER via GLiNER (local ONNX,
//! CPU by default) when a model is present; a capitalized-span regex as the no-model
//! fallback. Both are free per item; neither calls an LLM.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use anyhow::{anyhow, bail, Context, Result};
use gliner::model::{input::text::TextInput, params::Parameters, pipeline::span::SpanMode, GLiNER};
use orp::params::RuntimeParameters;
use regex::Regex;

pub const DEFAULT_MODEL_DIR: &str = "models/gliner_small-v2.1";
pub const DEFAULT_LABELS: &[&str] = &[
    "person",
    "organization",
    "location",
    "event",
    "product",
    "vessel",
    "aircraft",
    "satellite",
];
const DEFAULT_THRESHOLD: f32 = 0.5;
/// GLiNER small handles roughly 384 tokens; chunk well under that and batch the chunks.
const CHUNK_CHARS: usize = 1200;

#[derive(Debug, Clone)]
pub struct Mention {
    pub text: String,
    /// NER label when the extractor produces one (GLiNER); `None` for the regex fallback.
    pub label: Option<String>,
    /// Character offsets into the source text, best effort, for provenance.
    pub start: usize,
    pub end: usize,
    pub score: f32,
}

pub trait MentionExtractor: Send + Sync {
    fn name(&self) -> &str;
    fn extract(&self, text: &str) -> Result<Vec<Mention>>;
}

pub fn labels_from_env() -> Vec<String> {
    match std::env::var("ENARGEIA_NER_LABELS") {
        Ok(s) if !s.trim().is_empty() => s
            .split(',')
            .map(|l| l.trim().to_lowercase())
            .filter(|l| !l.is_empty())
            .collect(),
        _ => DEFAULT_LABELS.iter().map(|s| s.to_string()).collect(),
    }
}

pub fn model_dir_from_env() -> PathBuf {
    std::env::var("ENARGEIA_MODEL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_MODEL_DIR))
}

/// Which ONNX export to run: `model.onnx` (fp32, default) or a quantized sibling such as
/// `model_int8.onnx` — set `ENARGEIA_NER_ONNX`. The int8 export is a CPU speed/quality trade;
/// `enargeia models fetch --int8` downloads it.
pub fn onnx_file_from_env() -> String {
    std::env::var("ENARGEIA_NER_ONNX").unwrap_or_else(|_| "model.onnx".to_string())
}

/// HuggingFace repo holding the ONNX export of `urchade/gliner_small-v2.1` (Apache-2.0).
pub const MODEL_REPO: &str = "onnx-community/gliner_small-v2.1";
const MODEL_FILES: &[&str] = &["tokenizer.json", "onnx/model.onnx"];

/// Downloads the default GLiNER model into `dir` (skips files already present); with `int8`
/// also fetches the quantized export.
pub async fn fetch_model(dir: &Path, int8: bool) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let client = reqwest::Client::builder()
        .user_agent(crate::USER_AGENT)
        .build()?;
    let mut files: Vec<&str> = MODEL_FILES.to_vec();
    if int8 {
        files.push("onnx/model_int8.onnx");
    }
    for rel in files {
        let dest = dir.join(rel);
        if dest.exists() {
            println!("exists: {}", dest.display());
            continue;
        }
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let url = format!("https://huggingface.co/{MODEL_REPO}/resolve/main/{rel}");
        println!("downloading {url}");
        let mut resp = client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("request failed: {url}"))?
            .error_for_status()
            .with_context(|| format!("download failed: {url}"))?;
        let tmp = dest.with_extension("part");
        let mut file = tokio::fs::File::create(&tmp).await?;
        let mut written = 0u64;
        while let Some(chunk) = resp.chunk().await? {
            written += chunk.len() as u64;
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        tokio::fs::rename(&tmp, &dest).await?;
        println!("saved {} ({} MB)", dest.display(), written / 1_000_000);
    }
    Ok(())
}

/// Picks GLiNER when a model directory is present (or `ENARGEIA_EXTRACTOR=gliner` forces it),
/// otherwise the regex fallback — with a warning, because the fallback is much noisier.
pub fn default_extractor() -> Result<Box<dyn MentionExtractor>> {
    let forced = std::env::var("ENARGEIA_EXTRACTOR").ok();
    if forced.as_deref() == Some("regex") {
        return Ok(Box::new(RegexExtractor));
    }
    let dir = model_dir_from_env();
    let present =
        dir.join("tokenizer.json").exists() && dir.join("onnx").join(onnx_file_from_env()).exists();
    if !present {
        if forced.as_deref() == Some("gliner") {
            bail!(
                "ENARGEIA_EXTRACTOR=gliner but no model at {}; run `enargeia models fetch`",
                dir.display()
            );
        }
        tracing::warn!(
            dir = %dir.display(),
            "no GLiNER model found; using regex extractor (lower precision). Run `enargeia models fetch`."
        );
        return Ok(Box::new(RegexExtractor));
    }
    let threshold = std::env::var("ENARGEIA_NER_THRESHOLD")
        .ok()
        .and_then(|s| s.parse::<f32>().ok())
        .unwrap_or(DEFAULT_THRESHOLD);
    Ok(Box::new(GlinerExtractor::load(
        &dir,
        labels_from_env(),
        threshold,
    )?))
}

// ---------------------------------------------------------------------------------------------
// Regex fallback

fn mention_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Runs of 1-4 capitalized words. Deliberately permissive; the matcher and Tier 2 filter.
        Regex::new(r"\b([A-Z][a-zA-Z0-9&\-]*(?:\s+[A-Z][a-zA-Z0-9&\-]*){0,3})\b").unwrap()
    })
}

pub struct RegexExtractor;

impl MentionExtractor for RegexExtractor {
    fn name(&self) -> &str {
        "regex"
    }

    fn extract(&self, text: &str) -> Result<Vec<Mention>> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for cap in mention_regex().captures_iter(text) {
            let m = cap.get(1).unwrap();
            let t = m.as_str().trim();
            if t.split_whitespace().count() == 1 && t.len() < 3 {
                continue;
            }
            if seen.insert(t.to_lowercase()) {
                out.push(Mention {
                    text: t.to_string(),
                    label: None,
                    start: text[..m.start()].chars().count(),
                    end: text[..m.end()].chars().count(),
                    score: 0.3,
                });
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------------------------
// GLiNER

pub struct GlinerExtractor {
    model: GLiNER<SpanMode>,
    labels: Vec<String>,
}

impl GlinerExtractor {
    pub fn load(model_dir: &Path, labels: Vec<String>, threshold: f32) -> Result<Self> {
        let tokenizer = model_dir.join("tokenizer.json");
        let onnx = model_dir.join("onnx").join(onnx_file_from_env());
        for p in [&tokenizer, &onnx] {
            if !p.exists() {
                bail!("missing model file {}", p.display());
            }
        }
        let params = Parameters::default().with_threshold(threshold);
        let model = GLiNER::<SpanMode>::new(
            params,
            RuntimeParameters::default(),
            tokenizer.to_str().context("non-UTF8 tokenizer path")?,
            onnx.to_str().context("non-UTF8 model path")?,
        )
        .map_err(|e| anyhow!("failed to load GLiNER model: {e}"))?;
        tracing::info!(dir = %model_dir.display(), labels = ?labels, threshold, "GLiNER loaded");
        Ok(Self { model, labels })
    }
}

impl MentionExtractor for GlinerExtractor {
    fn name(&self) -> &str {
        "gliner"
    }

    fn extract(&self, text: &str) -> Result<Vec<Mention>> {
        let chunks = chunk_text(text, CHUNK_CHARS);
        if chunks.is_empty() {
            return Ok(Vec::new());
        }
        let texts: Vec<&str> = chunks.iter().map(|(_, s)| s.as_str()).collect();
        let labels: Vec<&str> = self.labels.iter().map(String::as_str).collect();
        let input =
            TextInput::from_str(&texts, &labels).map_err(|e| anyhow!("GLiNER input error: {e}"))?;
        let output = self
            .model
            .inference(input)
            .map_err(|e| anyhow!("GLiNER inference error: {e}"))?;

        // Keep one mention per distinct surface form, at its best score.
        let mut best: HashMap<String, Mention> = HashMap::new();
        for spans in output.spans {
            for span in spans {
                let (s, e) = span.offsets();
                let base = chunks[span.sequence()].0;
                let m = Mention {
                    text: clean_mention(span.text()),
                    label: Some(span.class().to_string()),
                    start: base + s,
                    end: base + e,
                    score: span.probability(),
                };
                if m.text.is_empty() || is_generic(&m.text) {
                    continue;
                }
                let key = m.text.to_lowercase();
                match best.get(&key) {
                    Some(prev) if prev.score >= m.score => {}
                    _ => {
                        best.insert(key, m);
                    }
                }
            }
        }
        let mut out: Vec<Mention> = best.into_values().collect();
        out.sort_by_key(|m| m.start);
        Ok(out)
    }
}

/// A span with no capital letter and no digit ("users", "data breach", "frontier lab") is a
/// common noun the model labeled as an entity, not a name. Named entities in running text
/// are capitalized or contain digits (a16z, H100) in every language this targets.
fn is_generic(text: &str) -> bool {
    !text.chars().any(|c| c.is_uppercase() || c.is_ascii_digit())
}

/// Trims a raw NER span: leading conjunctions/determiners the model sometimes includes
/// ("and Replit", "the Fed"), surrounding quotes/punctuation, and possessive suffixes.
fn clean_mention(raw: &str) -> String {
    const LEADING: &[&str] = &[
        "and ", "or ", "the ", "a ", "an ", "of ", "at ", "in ", "by ",
    ];
    let mut s = raw
        .trim()
        .trim_matches(|c: char| !c.is_alphanumeric() && c != '$' && c != '&')
        .to_string();
    loop {
        let lower = s.to_lowercase();
        match LEADING.iter().find(|p| lower.starts_with(*p)) {
            Some(p) if s.len() > p.len() => s = s[p.len()..].trim_start().to_string(),
            _ => break,
        }
    }
    for suffix in ["'s", "’s"] {
        if let Some(stripped) = s.strip_suffix(suffix) {
            s = stripped.to_string();
        }
    }
    s.trim().to_string()
}

/// Splits on paragraph and sentence boundaries into chunks of at most `max_chars`
/// characters, returning (char offset of chunk start, chunk text).
fn chunk_text(text: &str, max_chars: usize) -> Vec<(usize, String)> {
    let mut chunks: Vec<(usize, String)> = Vec::new();
    let mut current = String::new();
    let mut current_start = 0usize;
    let mut pos = 0usize;

    for sentence in split_sentences(text) {
        let len = sentence.chars().count();
        if !current.is_empty() && current.chars().count() + len > max_chars {
            chunks.push((current_start, std::mem::take(&mut current)));
        }
        if current.is_empty() {
            current_start = pos;
        }
        current.push_str(sentence);
        pos += len;
    }
    if !current.trim().is_empty() {
        chunks.push((current_start, current));
    }
    chunks.retain(|(_, c)| !c.trim().is_empty());
    chunks
}

fn split_sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        let boundary = matches!(b, b'.' | b'!' | b'?' | b'\n')
            && (i + 1 >= bytes.len() || bytes[i + 1].is_ascii_whitespace());
        if boundary {
            out.push(&text[start..=i]);
            start = i + 1;
        }
        i += 1;
    }
    if start < bytes.len() {
        out.push(&text[start..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regex_extracts_capitalized_spans() {
        let m = RegexExtractor
            .extract("Recorded Future acquired by Mastercard, said Babel Street.")
            .unwrap();
        let texts: Vec<&str> = m.iter().map(|x| x.text.as_str()).collect();
        assert!(texts.contains(&"Recorded Future"));
        assert!(texts.contains(&"Mastercard"));
    }

    #[test]
    fn generic_nouns_are_filtered() {
        assert!(is_generic("users"));
        assert!(is_generic("data breach"));
        assert!(!is_generic("a16z"));
        assert!(!is_generic("OpenAI"));
        assert!(!is_generic("iPhone"));
    }

    #[test]
    fn cleans_leading_conjunctions_and_possessives() {
        assert_eq!(clean_mention("and Replit"), "Replit");
        assert_eq!(clean_mention("the Federal Reserve"), "Federal Reserve");
        assert_eq!(clean_mention("OpenAI's"), "OpenAI");
        assert_eq!(clean_mention("\"Anthropic\","), "Anthropic");
        assert_eq!(clean_mention("$LAPTOP"), "$LAPTOP");
    }

    #[test]
    fn chunks_respect_limit_and_offsets() {
        let text = "Alpha one. Beta two. Gamma three. Delta four.";
        let chunks = chunk_text(text, 22);
        assert!(chunks.len() >= 2);
        assert_eq!(chunks[0].0, 0);
        for (off, c) in &chunks {
            assert!(c.chars().count() <= 22, "chunk too long: {c:?}");
            let expected: String = text.chars().skip(*off).take(c.chars().count()).collect();
            assert_eq!(&expected, c);
        }
    }
}
