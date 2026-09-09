//! Best-effort article body fetch. Feed metadata is often a title or a one-line teaser;
//! NER needs paragraphs. This pulls the page, strips markup, and keeps the long text
//! blocks — no readability library, no headless browser.

use std::time::Duration;

const MAX_BODY_CHARS: usize = 20_000;
const MIN_BLOCK_CHARS: usize = 80;
/// Polite gap between page fetches from the same adapter run.
pub const FETCH_GAP: Duration = Duration::from_millis(250);

/// Returns extracted body text, or `None` if the page yielded nothing usable.
pub async fn fetch_article_text(client: &reqwest::Client, url: &str) -> Option<String> {
    let resp = match client
        .get(url)
        .timeout(Duration::from_secs(15))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            tracing::debug!(url, status = %r.status(), "body fetch: non-success status");
            return None;
        }
        Err(e) => {
            tracing::debug!(url, error = %e, "body fetch failed");
            return None;
        }
    };
    let is_html = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.contains("html") || ct.contains("xml"))
        .unwrap_or(true);
    if !is_html {
        return None;
    }
    let html = resp.text().await.ok()?;
    let text = html_to_text(&html);
    if text.chars().count() < MIN_BLOCK_CHARS {
        None
    } else {
        Some(text.chars().take(MAX_BODY_CHARS).collect())
    }
}

/// Strips `<script>`/`<style>`/`<nav>`-like containers and all tags, decodes a few common
/// entities, then keeps only text blocks long enough to be prose.
pub fn html_to_text(html: &str) -> String {
    let stripped = remove_containers(
        html,
        &[
            "script", "style", "noscript", "svg", "nav", "header", "footer", "aside",
        ],
    );
    let mut out = String::with_capacity(stripped.len() / 2);
    let mut in_tag = false;
    let mut tag_name = String::new();
    let mut reading_name = false;
    for c in stripped.chars() {
        match c {
            '<' => {
                in_tag = true;
                reading_name = true;
                tag_name.clear();
            }
            '>' if in_tag => {
                in_tag = false;
                let block = matches!(
                    tag_name
                        .trim_start_matches('/')
                        .to_ascii_lowercase()
                        .as_str(),
                    "p" | "div"
                        | "br"
                        | "li"
                        | "h1"
                        | "h2"
                        | "h3"
                        | "h4"
                        | "tr"
                        | "section"
                        | "article"
                        | "blockquote"
                );
                out.push(if block { '\n' } else { ' ' });
            }
            _ if in_tag => {
                if reading_name {
                    if c.is_alphanumeric() || c == '/' {
                        tag_name.push(c);
                    } else {
                        reading_name = false;
                    }
                }
            }
            _ => out.push(c),
        }
    }
    let decoded = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&rsquo;", "’")
        .replace("&lsquo;", "‘")
        .replace("&rdquo;", "”")
        .replace("&ldquo;", "“")
        .replace("&mdash;", "—")
        .replace("&ndash;", "–")
        .replace("&lt;", "<")
        .replace("&gt;", ">");

    decoded
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| l.chars().count() >= MIN_BLOCK_CHARS)
        .collect::<Vec<_>>()
        .join("\n")
}

fn remove_containers(html: &str, tags: &[&str]) -> String {
    let mut s = html.to_string();
    for tag in tags {
        loop {
            let lower = s.to_ascii_lowercase();
            let Some(start) = lower.find(&format!("<{tag}")) else {
                break;
            };
            let close = format!("</{tag}>");
            let end = match lower[start..].find(&close) {
                Some(rel) => start + rel + close.len(),
                None => {
                    // Unclosed container: drop from its start to the end of the document.
                    s.truncate(start);
                    break;
                }
            };
            s.replace_range(start..end, " ");
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_scripts_and_keeps_paragraphs() {
        let html = "<html><head><script>var x=1;</script><style>p{}</style></head><body>\
            <nav><a href='/'>Home</a></nav>\
            <p>This is a sufficiently long paragraph of prose about Recorded Future being acquired by Mastercard in a large deal announced today.</p>\
            <p>short</p>\
            <footer>© 2026</footer></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Recorded Future"));
        assert!(!text.contains("var x"));
        assert!(!text.contains("Home"));
        assert!(!text.contains("short"));
    }
}
