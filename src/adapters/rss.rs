//! Minimal, tolerant RSS 2.0 / Atom adapter on quick-xml. No feed-parsing crate: the
//! surface we need (title, link, description/content, date, CDATA) is small and this keeps
//! the dependency tree short.

use anyhow::{Context, Result};
use async_trait::async_trait;
use quick_xml::{events::Event, Reader};

use super::{
    body::{fetch_article_text, FETCH_GAP},
    SourceAdapter,
};
use crate::models::{parse_feed_date, RawItem};

/// Feed descriptions shorter than this are treated as teasers and the linked page is fetched.
const TEASER_CHARS: usize = 400;

pub struct RssAdapter {
    pub feed_urls: Vec<String>,
    /// Fetch the linked page when the feed only carries a teaser.
    pub fetch_body: bool,
}

impl RssAdapter {
    pub fn new(feed_urls: Vec<String>) -> Self {
        Self {
            feed_urls,
            fetch_body: true,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct FeedEntry {
    pub title: String,
    pub link: String,
    pub text: String,
    pub date: String,
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
                    for entry in parsed {
                        let mut text = strip_tags(&entry.text);
                        if self.fetch_body
                            && !entry.link.is_empty()
                            && text.chars().count() < TEASER_CHARS
                        {
                            tokio::time::sleep(FETCH_GAP).await;
                            if let Some(page) = fetch_article_text(&client, &entry.link).await {
                                text = if text.is_empty() {
                                    page
                                } else {
                                    format!("{text}\n\n{page}")
                                };
                            }
                        }
                        items.push(RawItem {
                            source_type: "rss".to_string(),
                            source_ref: if entry.link.is_empty() {
                                url.clone()
                            } else {
                                entry.link
                            },
                            title: if entry.title.is_empty() {
                                None
                            } else {
                                Some(entry.title)
                            },
                            text,
                            // Per-publisher terms vary and aren't asserted here - default 'unknown'
                            // rather than falsely claiming clean commercial rights.
                            license_class: "unknown".to_string(),
                            published_at: parse_feed_date(&entry.date),
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

/// Returns one entry per RSS `<item>` or Atom `<entry>`.
fn parse_feed(xml: &str) -> Result<Vec<FeedEntry>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut out = Vec::new();
    let mut buf = Vec::new();

    let mut in_item = false;
    let mut cur_tag = String::new();
    let mut cur = FeedEntry::default();

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
                    cur = FeedEntry::default();
                }
                if in_item && local == "link" {
                    // Atom links carry href as an attribute instead of text content.
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"href" {
                            cur.link = String::from_utf8_lossy(&attr.value).to_string();
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
                push_field(&mut cur, &cur_tag, &text);
            }
            // Many RSS feeds wrap <description>/<content:encoded> in CDATA, which quick-xml
            // reports as a distinct event type from plain Text.
            Event::CData(t) => {
                if !in_item {
                    continue;
                }
                let text = String::from_utf8_lossy(t.as_ref()).to_string();
                push_field(&mut cur, &cur_tag, &text);
            }
            Event::End(e) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let local = name.rsplit(':').next().unwrap_or(&name).to_string();
                if local == "item" || local == "entry" {
                    in_item = false;
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => {}
        }
        buf.clear();
    }

    Ok(out)
}

fn push_field(cur: &mut FeedEntry, tag: &str, text: &str) {
    match tag {
        "title" => cur.title.push_str(text),
        "link" => cur.link.push_str(text),
        "description" | "summary" | "content" | "encoded" => cur.text.push_str(text),
        // RSS pubDate, Atom published/updated, Dublin Core dc:date (local name "date")
        "pubDate" | "published" | "updated" | "date" => {
            if cur.date.is_empty() {
                cur.date.push_str(text);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rss2_item() {
        let xml = r#"<rss><channel><item><title>Hello</title><link>http://x/1</link><description>World</description><pubDate>Tue, 08 Sep 2026 14:30:00 +0000</pubDate></item></channel></rss>"#;
        let parsed = parse_feed(xml).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].title, "Hello");
        assert_eq!(parsed[0].link, "http://x/1");
        assert_eq!(parsed[0].text, "World");
        assert!(parse_feed_date(&parsed[0].date).is_some());
    }

    #[test]
    fn parses_atom_entry() {
        let xml = r#"<feed><entry><title>Hi</title><link href="http://x/2"/><summary>Sum</summary><updated>2026-09-08T14:30:00Z</updated></entry></feed>"#;
        let parsed = parse_feed(xml).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].link, "http://x/2");
        assert_eq!(parsed[0].date, "2026-09-08T14:30:00Z");
    }

    #[test]
    fn parses_cdata_description() {
        let xml = r#"<rss><channel><item><title>T</title><link>http://x/3</link><description><![CDATA[<p>Hello <b>World</b></p>]]></description></item></channel></rss>"#;
        let parsed = parse_feed(xml).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].text, "<p>Hello <b>World</b></p>");
        assert_eq!(strip_tags(&parsed[0].text), "Hello World");
    }
}
