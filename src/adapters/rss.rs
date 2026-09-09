//! Minimal, tolerant RSS 2.0 / Atom adapter on quick-xml. No feed-parsing crate: the
//! surface we need (title, link, description/content, CDATA) is small and this keeps the
//! dependency tree short.

use anyhow::{Context, Result};
use async_trait::async_trait;
use quick_xml::{events::Event, Reader};

use super::SourceAdapter;
use crate::models::RawItem;

pub struct RssAdapter {
    pub feed_urls: Vec<String>,
}

impl RssAdapter {
    pub fn new(feed_urls: Vec<String>) -> Self {
        Self { feed_urls }
    }
}

#[async_trait]
impl SourceAdapter for RssAdapter {
    fn name(&self) -> &str {
        "rss"
    }

    async fn fetch(&self) -> Result<Vec<RawItem>> {
        let client = reqwest::Client::builder()
            .user_agent(crate::USER_AGENT)
            .build()?;
        let mut items = Vec::new();
        for url in &self.feed_urls {
            let body = match client.get(url).send().await {
                Ok(resp) => match resp.text().await {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(url, error = %e, "failed reading feed body");
                        continue;
                    }
                },
                Err(e) => {
                    tracing::warn!(url, error = %e, "failed fetching feed");
                    continue;
                }
            };
            match parse_feed(&body) {
                Ok(parsed) => {
                    for (title, link, text) in parsed {
                        items.push(RawItem {
                            source_type: "rss".to_string(),
                            source_ref: if link.is_empty() { url.clone() } else { link },
                            title: if title.is_empty() { None } else { Some(title) },
                            text: strip_tags(&text),
                            // Per-publisher terms vary and aren't asserted here - default 'unknown'
                            // rather than falsely claiming clean commercial rights.
                            license_class: "unknown".to_string(),
                        });
                    }
                }
                Err(e) => tracing::warn!(url, error = %e, "failed parsing feed"),
            }
        }
        Ok(items)
    }
}

/// Strips inline HTML markup that leaks in through CDATA-wrapped descriptions
/// (e.g. `<p>...</p>`, `<a href=...>`), which would otherwise pollute the Tier 1
/// mention regex with tag fragments like "Href" or "Div".
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Returns (title, link, description/summary text) tuples for RSS `<item>` or Atom `<entry>` elements.
fn parse_feed(xml: &str) -> Result<Vec<(String, String, String)>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut out = Vec::new();
    let mut buf = Vec::new();

    let mut in_item = false;
    let mut cur_tag = String::new();
    let mut title = String::new();
    let mut link = String::new();
    let mut desc = String::new();

    loop {
        match reader
            .read_event_into(&mut buf)
            .context("xml parse error")?
        {
            Event::Eof => break,
            Event::Start(e) | Event::Empty(e) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let local = name.rsplit(':').next().unwrap_or(&name).to_string();
                if local == "item" || local == "entry" {
                    in_item = true;
                    title.clear();
                    link.clear();
                    desc.clear();
                }
                if in_item && local == "link" {
                    // Atom links carry href as an attribute instead of text content.
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"href" {
                            link = String::from_utf8_lossy(&attr.value).to_string();
                        }
                    }
                }
                cur_tag = local;
            }
            Event::Text(t) => {
                if !in_item {
                    continue;
                }
                let text = t.unescape().unwrap_or_default().to_string();
                match cur_tag.as_str() {
                    "title" => title.push_str(&text),
                    "link" => link.push_str(&text),
                    "description" | "summary" | "content" => desc.push_str(&text),
                    _ => {}
                }
            }
            // Many RSS feeds wrap <description>/<content:encoded> in CDATA, which quick-xml
            // reports as a distinct event type from plain Text.
            Event::CData(t) => {
                if !in_item {
                    continue;
                }
                let text = String::from_utf8_lossy(t.as_ref()).to_string();
                match cur_tag.as_str() {
                    "title" => title.push_str(&text),
                    "link" => link.push_str(&text),
                    "description" | "summary" | "content" | "encoded" => desc.push_str(&text),
                    _ => {}
                }
            }
            Event::End(e) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let local = name.rsplit(':').next().unwrap_or(&name).to_string();
                if local == "item" || local == "entry" {
                    in_item = false;
                    out.push((title.clone(), link.clone(), desc.clone()));
                }
            }
            _ => {}
        }
        buf.clear();
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rss2_item() {
        let xml = r#"<rss><channel><item><title>Hello</title><link>http://x/1</link><description>World</description></item></channel></rss>"#;
        let parsed = parse_feed(xml).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].0, "Hello");
        assert_eq!(parsed[0].1, "http://x/1");
        assert_eq!(parsed[0].2, "World");
    }

    #[test]
    fn parses_atom_entry() {
        let xml = r#"<feed><entry><title>Hi</title><link href="http://x/2"/><summary>Sum</summary></entry></feed>"#;
        let parsed = parse_feed(xml).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].1, "http://x/2");
    }

    #[test]
    fn parses_cdata_description() {
        let xml = r#"<rss><channel><item><title>T</title><link>http://x/3</link><description><![CDATA[<p>Hello <b>World</b></p>]]></description></item></channel></rss>"#;
        let parsed = parse_feed(xml).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].2, "<p>Hello <b>World</b></p>");
        assert_eq!(strip_tags(&parsed[0].2), "Hello World");
    }
}
