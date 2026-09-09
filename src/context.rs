//! Context assembler: the "reason over the graph, not raw documents" hop. Resolves a
//! free-text question to seed entities, walks the currently-valid (or as-of-a-date) edges
//! to a bounded depth, and formats a numbered, source-cited context for the LLM to answer
//! from.

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::Result;
use sqlx::SqlitePool;

use crate::{
    llm::LlmClient,
    models::{WmEdge, WmEntity},
};

/// Multi-word names may seed via near-exact n-gram similarity (typos, spacing variants).
const NGRAM_MATCH_THRESHOLD: f64 = 0.9;
const MAX_SEED_MATCHES: usize = 6;
const DEFAULT_DEPTH: usize = 2;
/// Hub entities explode at depth 2; keep the slice small enough for a 7B model to reason over.
const MAX_EDGES: usize = 60;
const MAX_SOURCES: usize = 30;

/// Entities eligible as seeds: live ones for "now" questions; every entity for as-of
/// questions, since the point of a historical query is to see what has since gone stale.
async fn seed_pool(pool: &SqlitePool, as_of: Option<&str>) -> Result<Vec<WmEntity>> {
    let sql = if as_of.is_some() {
        "SELECT * FROM wm_entities"
    } else {
        "SELECT * FROM wm_entities WHERE is_live = 1"
    };
    Ok(sqlx::query_as::<_, WmEntity>(sql).fetch_all(pool).await?)
}

/// How well an entity name matches a question. Word-bounded containment wins outright.
/// Single-word names match only by containment (Jaro-Winkler's prefix bonus makes
/// "openai" ≈ "open" otherwise); multi-word names may also match a same-length question
/// n-gram at near-exact similarity.
fn name_question_score(name: &str, question_words: &[String], question_lower: &str) -> f64 {
    let n = name.trim().to_lowercase();
    if n.len() < 3 {
        return 0.0;
    }
    if contains_word_bounded(question_lower, &n) {
        return 1.0;
    }
    let k = n.split_whitespace().count();
    if k < 2 || question_words.len() < k {
        return 0.0;
    }
    question_words
        .windows(k)
        .map(|w| strsim::jaro_winkler(&n, &w.join(" ")))
        .fold(0.0_f64, f64::max)
}

fn contains_word_bounded(haystack: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(pos) = haystack[from..].find(needle) {
        let start = from + pos;
        let end = start + needle.len();
        let before_ok = start == 0
            || !haystack[..start]
                .chars()
                .next_back()
                .unwrap()
                .is_alphanumeric();
        let after_ok =
            end == haystack.len() || !haystack[end..].chars().next().unwrap().is_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
        if from >= haystack.len() {
            break;
        }
    }
    false
}

/// Seed entities for a question, best-scoring first.
pub async fn find_matching_entities(
    pool: &SqlitePool,
    question: &str,
    as_of: Option<&str>,
) -> Result<Vec<WmEntity>> {
    let entities = seed_pool(pool, as_of).await?;
    let question_lower = question.to_lowercase();
    let question_words: Vec<String> = question_lower
        .split(|c: char| !c.is_alphanumeric() && c != '\'' && c != '-')
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect();

    let mut scored: Vec<(f64, WmEntity)> = entities
        .into_iter()
        .map(|e| {
            let mut names = vec![e.canonical_name.clone()];
            names.extend(e.alias_list());
            let best = names
                .iter()
                .map(|n| name_question_score(n, &question_words, &question_lower))
                .fold(0.0_f64, f64::max);
            (best, e)
        })
        .filter(|(s, _)| *s >= NGRAM_MATCH_THRESHOLD)
        .collect();
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap()
            .then(b.1.canonical_name.len().cmp(&a.1.canonical_name.len()))
    });
    Ok(scored
        .into_iter()
        .take(MAX_SEED_MATCHES)
        .map(|(_, e)| e)
        .collect())
}

/// The `n` live entities (or all entities, for as-of questions) with the most valid edges.
pub async fn hub_entities(
    pool: &SqlitePool,
    as_of: Option<&str>,
    n: usize,
) -> Result<Vec<WmEntity>> {
    let live = if as_of.is_some() {
        ""
    } else {
        "AND is_live = 1"
    };
    let sql = format!(
        "SELECT * FROM wm_entities WHERE id IN (\
           SELECT id FROM (\
             SELECT from_id AS id FROM wm_edges WHERE invalid_at IS NULL \
             UNION ALL SELECT to_id FROM wm_edges WHERE invalid_at IS NULL) \
           GROUP BY id ORDER BY COUNT(*) DESC LIMIT ?) {live}"
    );
    Ok(sqlx::query_as::<_, WmEntity>(&sql)
        .bind(n as i64)
        .fetch_all(pool)
        .await?)
}

pub struct GraphSlice {
    pub entities: Vec<WmEntity>,
    pub edges: Vec<WmEdge>,
}

/// Edges valid at `as_of` (or valid now when `None`): bi-temporal filtering on valid time.
async fn load_edges(pool: &SqlitePool, as_of: Option<&str>) -> Result<Vec<WmEdge>> {
    Ok(match as_of {
        None => {
            sqlx::query_as::<_, WmEdge>("SELECT * FROM wm_edges WHERE invalid_at IS NULL")
                .fetch_all(pool)
                .await?
        }
        Some(t) => {
            sqlx::query_as::<_, WmEdge>(
                "SELECT * FROM wm_edges WHERE (valid_at IS NULL OR valid_at <= ?) AND (invalid_at IS NULL OR invalid_at > ?)",
            )
            .bind(t)
            .bind(t)
            .fetch_all(pool)
            .await?
        }
    })
}

pub(crate) async fn bfs(
    pool: &SqlitePool,
    seeds: &[String],
    depth: usize,
    as_of: Option<&str>,
) -> Result<GraphSlice> {
    let all_edges = load_edges(pool, as_of).await?;
    let mut visited: HashSet<String> = seeds.iter().cloned().collect();
    let mut frontier: VecDeque<(String, usize)> = seeds.iter().map(|s| (s.clone(), 0)).collect();
    let mut used: HashMap<String, (usize, WmEdge)> = HashMap::new();

    while let Some((id, d)) = frontier.pop_front() {
        if d >= depth {
            continue;
        }
        for edge in &all_edges {
            let neighbor = if edge.from_id == id {
                Some(edge.to_id.clone())
            } else if edge.to_id == id {
                Some(edge.from_id.clone())
            } else {
                None
            };
            if let Some(n) = neighbor {
                used.entry(edge.id.clone())
                    .or_insert_with(|| (d, edge.clone()));
                if visited.insert(n.clone()) {
                    frontier.push_back((n, d + 1));
                }
            }
        }
    }

    // Rank: typed relations before co-occurrence, closer to the seeds before farther, heavier
    // before lighter — then cap, so a hub entity yields a focused slice rather than everything.
    let mut ranked: Vec<(usize, WmEdge)> = used.into_values().collect();
    ranked.sort_by(|(da, a), (db, b)| {
        let ta = a.edge_type == "mentioned_with";
        let tb = b.edge_type == "mentioned_with";
        ta.cmp(&tb).then(da.cmp(db)).then(
            b.weight
                .partial_cmp(&a.weight)
                .unwrap_or(std::cmp::Ordering::Equal),
        )
    });
    let edges: Vec<WmEdge> = ranked.into_iter().take(MAX_EDGES).map(|(_, e)| e).collect();

    let mut keep: HashSet<String> = seeds.iter().cloned().collect();
    for e in &edges {
        keep.insert(e.from_id.clone());
        keep.insert(e.to_id.clone());
    }
    let ids: Vec<String> = keep.into_iter().collect();
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!("SELECT * FROM wm_entities WHERE id IN ({placeholders})");
    let mut q = sqlx::query_as::<_, WmEntity>(&sql);
    for id in &ids {
        q = q.bind(id);
    }
    let entities: Vec<WmEntity> = q.fetch_all(pool).await.unwrap_or_default();

    Ok(GraphSlice { entities, edges })
}

#[derive(Debug, Clone)]
pub(crate) struct SourceRef {
    pub(crate) number: usize,
    pub(crate) title: String,
    pub(crate) source_ref: String,
    pub(crate) license_class: String,
    pub(crate) published_at: Option<String>,
}

pub(crate) async fn load_sources(
    pool: &SqlitePool,
    slice: &GraphSlice,
) -> Result<HashMap<String, SourceRef>> {
    let mut ids: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    'outer: for e in &slice.edges {
        for id in e.source_ids() {
            if seen.insert(id.clone()) {
                ids.push(id);
                if ids.len() >= MAX_SOURCES {
                    break 'outer;
                }
            }
        }
    }
    let mut out = HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT id, COALESCE(title, ''), source_ref, license_class, published_at FROM wm_source_items WHERE id IN ({placeholders})"
    );
    let mut q = sqlx::query_as::<_, (String, String, String, String, Option<String>)>(&sql);
    for id in &ids {
        q = q.bind(id);
    }
    let rows = q.fetch_all(pool).await?;
    let by_id: HashMap<String, (String, String, String, Option<String>)> = rows
        .into_iter()
        .map(|(id, t, r, l, p)| (id, (t, r, l, p)))
        .collect();
    for (i, id) in ids.iter().enumerate() {
        if let Some((title, source_ref, license_class, published_at)) = by_id.get(id) {
            out.insert(
                id.clone(),
                SourceRef {
                    number: i + 1,
                    title: title.clone(),
                    source_ref: source_ref.clone(),
                    license_class: license_class.clone(),
                    published_at: published_at.clone(),
                },
            );
        }
    }
    Ok(out)
}

fn short_date(ts: &str) -> &str {
    ts.get(..10).unwrap_or(ts)
}

/// Returns (context for the model, numbered source list).
pub(crate) fn format_context(
    slice: &GraphSlice,
    sources: &HashMap<String, SourceRef>,
) -> (String, String) {
    let name = |id: &str| {
        slice
            .entities
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.canonical_name.as_str())
            .unwrap_or("?")
    };

    let mut ctx = String::new();
    ctx.push_str("RELATIONS (valid-from date; source numbers in brackets):\n");
    for edge in &slice.edges {
        let mut nums: Vec<usize> = edge
            .source_ids()
            .iter()
            .filter_map(|id| sources.get(id).map(|s| s.number))
            .collect();
        nums.sort_unstable();
        nums.dedup();
        let cite: String = nums
            .iter()
            .map(|n| format!("[{n}]"))
            .collect::<Vec<_>>()
            .join("");
        let since = edge
            .valid_at
            .as_deref()
            .map(|v| format!(" (since {})", short_date(v)))
            .unwrap_or_default();
        ctx.push_str(&format!(
            "- {} --[{}]--> {}{} {}\n",
            name(&edge.from_id),
            edge.edge_type,
            name(&edge.to_id),
            since,
            cite
        ));
    }
    ctx.push_str("\nENTITIES:\n");
    for e in &slice.entities {
        let aliases = e.alias_list();
        let alias_str = if aliases.is_empty() {
            String::new()
        } else {
            format!(" (aka {})", aliases.join(", "))
        };
        ctx.push_str(&format!(
            "- {} [{}]{}\n",
            e.canonical_name, e.entity_type, alias_str
        ));
    }

    let mut src_list: Vec<&SourceRef> = sources.values().collect();
    src_list.sort_by_key(|s| s.number);
    let mut src = String::new();
    for s in src_list {
        let title = if s.title.is_empty() {
            "(untitled)"
        } else {
            s.title.as_str()
        };
        let date = s
            .published_at
            .as_deref()
            .map(|d| format!("{} · ", short_date(d)))
            .unwrap_or_default();
        src.push_str(&format!(
            "[{}] {}{} — {} ({})\n",
            s.number, date, title, s.source_ref, s.license_class
        ));
    }
    (ctx, src)
}

pub(crate) const ASK_SYSTEM_PROMPT: &str =
    "You are an intelligence analyst answering from a resolved \
entity graph. Use ONLY the provided RELATIONS, ENTITIES and SOURCES. Cite sources by their \
bracketed numbers, e.g. [2], after each claim. Prefer typed relations (e.g. suing, acquired, \
founder_of) over co-occurrence (mentioned_with). If the context does not support an answer, \
say so plainly; never guess or use outside knowledge. Be concise and concrete.";

/// Number of `[n]` source citations in model output.
pub fn citation_count(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut n = 0;
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'[' && bytes[i + 1].is_ascii_digit() {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b']' {
                n += 1;
                i = j;
            }
        }
        i += 1;
    }
    n
}

/// Smaller models sometimes answer without citing. Output that cannot be traced to a source
/// is marked rather than passed off as sourced.
pub fn flag_uncited(text: &str, source_refs: &[&str]) -> String {
    let cites_url = source_refs.iter().any(|u| u.len() > 12 && text.contains(u));
    if citation_count(text) == 0 && !cites_url {
        format!(
            "⚠ UNVERIFIED — the model cited no sources; treat every claim below as unconfirmed.\n\n{text}"
        )
    } else {
        text.to_string()
    }
}

/// Restricts a slice to relations backed by at least one source of `license_class`, and
/// strips the other sources from those relations so they are never cited. Entities stay:
/// existence is not licensed, evidence is.
pub fn filter_slice_by_license(
    slice: &mut GraphSlice,
    license_of: &HashMap<String, String>,
    license_class: &str,
) {
    slice.edges.retain_mut(|e| {
        let allowed: Vec<String> = e
            .source_ids()
            .into_iter()
            .filter(|id| license_of.get(id).map(String::as_str) == Some(license_class))
            .collect();
        if allowed.is_empty() {
            return false;
        }
        e.source_item_ids = serde_json::to_string(&allowed).unwrap_or_else(|_| "[]".into());
        true
    });
}

pub(crate) async fn license_map(
    pool: &SqlitePool,
    slice: &GraphSlice,
) -> Result<HashMap<String, String>> {
    let ids: Vec<String> = slice
        .edges
        .iter()
        .flat_map(|e| e.source_ids())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let mut out = HashMap::new();
    for chunk in ids.chunks(500) {
        let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql =
            format!("SELECT id, license_class FROM wm_source_items WHERE id IN ({placeholders})");
        let mut q = sqlx::query_as::<_, (String, String)>(&sql);
        for id in chunk {
            q = q.bind(id);
        }
        out.extend(q.fetch_all(pool).await?);
    }
    Ok(out)
}

/// Answers a question from the graph as it is now, or as it was at `as_of` (RFC3339 or
/// YYYY-MM-DD). Returns the answer followed by a numbered source list.
pub async fn ask(
    pool: &SqlitePool,
    client: &LlmClient,
    question: &str,
    as_of: Option<&str>,
) -> Result<String> {
    ask_filtered(pool, client, question, as_of, None).await
}

/// `ask`, citing only sources of `license_filter` when one is given.
pub async fn ask_filtered(
    pool: &SqlitePool,
    client: &LlmClient,
    question: &str,
    as_of: Option<&str>,
    license_filter: Option<&str>,
) -> Result<String> {
    let mut seeds = find_matching_entities(pool, question, as_of).await?;
    let mut seeded_by_hubs = false;
    if seeds.is_empty() {
        // A question that names nothing ("which organizations were charged?") is a survey:
        // seed from the most-connected live entities instead of refusing.
        seeds = hub_entities(pool, as_of, MAX_SEED_MATCHES).await?;
        seeded_by_hubs = true;
    }
    if seeds.is_empty() {
        return Ok(
            "No entities in the graph match this question yet; ingest and resolve more content first."
                .to_string(),
        );
    }
    let seed_ids: Vec<String> = seeds.iter().map(|e| e.id.clone()).collect();
    let mut slice = bfs(pool, &seed_ids, DEFAULT_DEPTH, as_of).await?;
    if let Some(class) = license_filter {
        let licenses = license_map(pool, &slice).await?;
        filter_slice_by_license(&mut slice, &licenses, class);
        if slice.edges.is_empty() {
            return Ok(format!(
                "The graph has no relations about this backed by {class} sources; nothing can be cited under this token's license scope."
            ));
        }
    }
    let sources = load_sources(pool, &slice).await?;
    let (context, source_list) = format_context(&slice, &sources);
    let when = as_of
        .map(|t| format!("The graph is shown AS OF {t}; treat that as the present.\n"))
        .unwrap_or_default();
    let prompt = format!("{when}CONTEXT\n{context}\nSOURCES\n{source_list}\nQUESTION: {question}");
    let answer = client
        .complete(ASK_SYSTEM_PROMPT, &prompt, 1024, false)
        .await?;
    let refs: Vec<&str> = sources.values().map(|s| s.source_ref.as_str()).collect();
    let answer = flag_uncited(&answer, &refs);
    let seed_names: Vec<&str> = seeds.iter().map(|e| e.canonical_name.as_str()).collect();
    Ok(format!(
        "{answer}\n\n— graph slice{}: {} entities, {} edges; seeds{}: {}\nSources:\n{source_list}",
        as_of.map(|t| format!(" as of {t}")).unwrap_or_default(),
        slice.entities.len(),
        slice.edges.len(),
        if seeded_by_hubs {
            " (question named no entity; most-connected entities used)"
        } else {
            ""
        },
        seed_names.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(id: &str, sources: &[&str]) -> WmEdge {
        WmEdge {
            id: id.into(),
            from_id: "a".into(),
            to_id: "b".into(),
            edge_type: "suing".into(),
            weight: 1.0,
            metadata: "{}".into(),
            source_item_ids: serde_json::to_string(sources).unwrap(),
            first_seen: String::new(),
            last_seen: String::new(),
            valid_at: None,
            invalid_at: None,
            superseded_by: None,
        }
    }

    #[test]
    fn uncited_output_is_flagged() {
        assert_eq!(citation_count("A sued B [2]. C acquired D [10][3]."), 3);
        assert_eq!(citation_count("array[0] index [x] and [ 1 ]"), 1);
        assert!(flag_uncited("No citations here.", &[]).starts_with("⚠ UNVERIFIED"));
        assert_eq!(flag_uncited("Cited [1].", &[]), "Cited [1].");
        let url = "https://example.org/story/123";
        assert_eq!(
            flag_uncited(&format!("See {url}"), &[url]),
            format!("See {url}")
        );
    }

    #[test]
    fn license_filter_keeps_only_citable_evidence() {
        let mut slice = GraphSlice {
            entities: vec![],
            edges: vec![
                edge("e1", &["s_clean", "s_nc"]),
                edge("e2", &["s_nc"]),
                edge("e3", &["s_missing"]),
            ],
        };
        let licenses: HashMap<String, String> = [
            ("s_clean".to_string(), "commercial_clean".to_string()),
            ("s_nc".to_string(), "non_commercial_only".to_string()),
        ]
        .into_iter()
        .collect();
        filter_slice_by_license(&mut slice, &licenses, "commercial_clean");
        assert_eq!(slice.edges.len(), 1);
        assert_eq!(slice.edges[0].id, "e1");
        assert_eq!(slice.edges[0].source_ids(), vec!["s_clean".to_string()]);
    }

    fn words(q: &str) -> Vec<String> {
        q.to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn containment_beats_whole_question_similarity() {
        let q = "Who is Travis Kalanick and which companies is he connected to?";
        let s = name_question_score("Travis Kalanick", &words(q), &q.to_lowercase());
        assert_eq!(s, 1.0);
    }

    #[test]
    fn single_word_names_do_not_seed_by_prefix() {
        let q = "what did open source projects announce";
        let s = name_question_score("OpenAI", &words(q), &q.to_lowercase());
        assert_eq!(s, 0.0);
        let q2 = "what did OpenAI announce";
        assert_eq!(
            name_question_score("OpenAI", &words(q2), &q2.to_lowercase()),
            1.0
        );
    }

    #[test]
    fn multi_word_names_match_near_exact_variants() {
        let q = "who leads recorded futur now";
        let s = name_question_score("Recorded Future", &words(q), &q.to_lowercase());
        assert!(s >= NGRAM_MATCH_THRESHOLD, "{s}");
        let q2 = "and which companies is he connected to";
        let s2 = name_question_score("AI companies", &words(q2), &q2.to_lowercase());
        assert!(s2 < NGRAM_MATCH_THRESHOLD, "{s2}");
    }

    #[test]
    fn containment_is_word_bounded() {
        assert!(contains_word_bounded(
            "who is travis kalanick?",
            "travis kalanick"
        ));
        assert!(!contains_word_bounded("the pandl index", "and"));
        assert!(contains_word_bounded("a and b", "and"));
    }

    #[tokio::test]
    async fn as_of_filters_superseded_edges() {
        use sqlx::sqlite::SqlitePoolOptions;
        let pool = SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let alice = crate::resolve::create_entity(&pool, "Alice", "person", 0.9)
            .await
            .unwrap();
        let bob = crate::resolve::create_entity(&pool, "Bob", "person", 0.9)
            .await
            .unwrap();
        let acme = crate::resolve::create_entity(&pool, "Acme", "organization", 0.9)
            .await
            .unwrap();
        crate::resolve::link_entities_typed(
            &pool,
            &alice,
            &acme,
            "ceo_of",
            "s1",
            "2025-01-01T00:00:00+00:00",
        )
        .await
        .unwrap();
        crate::resolve::link_entities_typed(
            &pool,
            &bob,
            &acme,
            "ceo_of",
            "s2",
            "2026-06-01T00:00:00+00:00",
        )
        .await
        .unwrap();

        let now_edges = load_edges(&pool, None).await.unwrap();
        assert_eq!(now_edges.len(), 1);
        assert_eq!(now_edges[0].from_id, bob);

        let then = load_edges(&pool, Some("2025-12-31T00:00:00+00:00"))
            .await
            .unwrap();
        assert_eq!(then.len(), 1);
        assert_eq!(then[0].from_id, alice);
    }
}
