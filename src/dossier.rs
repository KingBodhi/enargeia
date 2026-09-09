//! `enargeia dossier <entity>`: a target profile assembled from the graph — identity,
//! a dated timeline of typed relations, the co-mention network, sources, and (optionally) a
//! grounded assessment written by the LLM from that same evidence and nothing else.
//!
//! Scope by construction: everything here traces to an ingested public source. The
//! assessment prompt forbids outside knowledge, so a dossier on a person is what the public
//! record says about their professional conduct — not a background check.

use std::collections::HashMap;

use anyhow::Result;
use sqlx::SqlitePool;

use crate::{
    context::{self, GraphSlice, SourceRef},
    llm::LlmClient,
    models::WmEdge,
    why,
};

const NETWORK_LIMIT: usize = 15;

#[derive(Debug, Default, Clone)]
pub struct DossierOptions<'a> {
    pub as_of: Option<&'a str>,
    /// Cite only sources of this license class (client tokens).
    pub license_filter: Option<&'a str>,
}

fn short(ts: &str) -> &str {
    ts.get(..10).unwrap_or(ts)
}

fn cite(edge: &WmEdge, sources: &HashMap<String, SourceRef>) -> String {
    let mut nums: Vec<usize> = edge
        .source_ids()
        .iter()
        .filter_map(|id| sources.get(id).map(|s| s.number))
        .collect();
    nums.sort_unstable();
    nums.dedup();
    nums.iter()
        .map(|n| format!("[{n}]"))
        .collect::<Vec<_>>()
        .join("")
}

/// Builds the evidence sections. Returns the markdown body and the context/source text the
/// assessment is written from, so both come from exactly the same slice.
async fn evidence(
    pool: &SqlitePool,
    entity_id: &str,
    opts: &DossierOptions<'_>,
) -> Result<(String, GraphSlice, HashMap<String, SourceRef>)> {
    let mut slice = context::bfs(pool, &[entity_id.to_string()], 1, opts.as_of).await?;
    if let Some(class) = opts.license_filter {
        let licenses = context::license_map(pool, &slice).await?;
        context::filter_slice_by_license(&mut slice, &licenses, class);
    }
    let sources = context::load_sources(pool, &slice).await?;
    let name = |id: &str| -> String {
        slice
            .entities
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.canonical_name.clone())
            .unwrap_or_else(|| "?".into())
    };

    let mut md = String::new();

    // Timeline: typed relations in valid-time order; superseded ones shown as closed.
    let mut typed: Vec<&WmEdge> = slice
        .edges
        .iter()
        .filter(|e| e.edge_type != "mentioned_with")
        .collect();
    typed.sort_by(|a, b| a.valid_at.cmp(&b.valid_at));
    md.push_str(&format!(
        "## Timeline ({} typed relations)\n\n",
        typed.len()
    ));
    if typed.is_empty() {
        md.push_str("_no typed relations yet — run Tier 2 enrichment on this entity's sources_\n");
    }
    for e in &typed {
        let (subject, object, arrow) = if e.from_id == entity_id {
            (name(&e.from_id), name(&e.to_id), "→")
        } else {
            (name(&e.from_id), name(&e.to_id), "←")
        };
        let when = match (&e.valid_at, &e.invalid_at) {
            (Some(v), None) => format!("{} –", short(v)),
            (Some(v), Some(i)) => format!("{} – {} (superseded)", short(v), short(i)),
            (None, Some(i)) => format!("– {}", short(i)),
            (None, None) => "undated".into(),
        };
        md.push_str(&format!(
            "- **{when}** {subject} {arrow} `{}` {object} {}\n",
            e.edge_type,
            cite(e, &sources)
        ));
    }

    // Network: who this entity is mentioned with, heaviest first.
    let mut cooc: Vec<&WmEdge> = slice
        .edges
        .iter()
        .filter(|e| e.edge_type == "mentioned_with")
        .collect();
    cooc.sort_by(|a, b| {
        b.weight
            .partial_cmp(&a.weight)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    md.push_str(&format!(
        "\n## Network (top {} of {} co-mentions)\n\n",
        cooc.len().min(NETWORK_LIMIT),
        cooc.len()
    ));
    for e in cooc.iter().take(NETWORK_LIMIT) {
        let other = if e.from_id == entity_id {
            &e.to_id
        } else {
            &e.from_id
        };
        let etype = slice
            .entities
            .iter()
            .find(|x| &x.id == other)
            .map(|x| x.entity_type.as_str())
            .unwrap_or("?");
        md.push_str(&format!(
            "- {} [{etype}] · {:.0} shared source(s) {}\n",
            name(other),
            e.weight,
            cite(e, &sources)
        ));
    }

    Ok((md, slice, sources))
}

const ASSESSMENT_PROMPT: &str = "You are an intelligence analyst writing the ASSESSMENT \
section of a dossier on the SUBJECT, from a resolved entity graph. Use ONLY the provided \
RELATIONS, ENTITIES and SOURCES; cite sources by bracketed number after each claim. Cover: \
what the subject is, what it has done recently (dated), who it is connected to and how, and \
what is unclear or contested in the evidence. Do not speculate, do not use outside knowledge, \
do not characterize private life. 150–300 words.";

/// Full dossier as markdown. With a client the assessment section is generated from the
/// same evidence; without one it is omitted (no LLM call).
pub async fn dossier(
    pool: &SqlitePool,
    client: Option<&LlmClient>,
    query: &str,
    opts: &DossierOptions<'_>,
) -> Result<String> {
    let e = why::find_entity(pool, query).await?;
    let (body, slice, sources) = evidence(pool, &e.id, opts).await?;
    let aliases = e.alias_list();
    let geo: Option<(f64, f64, Option<String>)> =
        sqlx::query_as("SELECT lat, lon, place_name FROM wm_geo WHERE entity_id = ?")
            .bind(&e.id)
            .fetch_optional(pool)
            .await?;

    let mut md = String::new();
    md.push_str(&format!("# Dossier: {}\n\n", e.canonical_name));
    md.push_str(&format!(
        "**Type** {} · **Aliases** {} · **Confidence** {:.2} · **Live** {} · **First seen** {} · **Last seen** {}{}{}\n\n",
        e.entity_type,
        if aliases.is_empty() { "—".to_string() } else { aliases.join(", ") },
        e.confidence,
        e.is_live,
        short(&e.first_seen),
        short(&e.last_seen),
        geo.map(|(lat, lon, place)| format!(
            " · **Location** {:.3}, {:.3}{}",
            lat,
            lon,
            place.map(|p| format!(" ({p})")).unwrap_or_default()
        ))
        .unwrap_or_default(),
        opts.as_of
            .map(|t| format!(" · **As of** {t}"))
            .unwrap_or_default()
    ));
    md.push_str(&body);

    if let Some(client) = client {
        let (ctx, src) = context::format_context(&slice, &sources);
        if !slice.edges.is_empty() {
            let prompt = format!(
                "SUBJECT: {}\n{}CONTEXT\n{ctx}\nSOURCES\n{src}",
                e.canonical_name,
                opts.as_of
                    .map(|t| format!("The graph is shown AS OF {t}; treat that as the present.\n"))
                    .unwrap_or_default()
            );
            let text = client
                .complete(ASSESSMENT_PROMPT, &prompt, 700, false)
                .await?;
            md.push_str(&format!(
                "\n## Assessment ({})\n\n{}\n",
                client.describe(),
                text.trim()
            ));
        }
    }

    let mut list: Vec<&SourceRef> = sources.values().collect();
    list.sort_by_key(|s| s.number);
    md.push_str(&format!("\n## Sources ({})\n\n", list.len()));
    for s in list {
        md.push_str(&format!(
            "{}. {}{} — {} ({})\n",
            s.number,
            s.published_at
                .as_deref()
                .map(|d| format!("{} · ", short(d)))
                .unwrap_or_default(),
            if s.title.is_empty() {
                "(untitled)"
            } else {
                &s.title
            },
            s.source_ref,
            s.license_class
        ));
    }
    md.push_str(&format!(
        "\n---\n_{} entities · {} relations in scope{} · public sources only; professional conduct only. `enargeia why \"{}\"` for the full provenance trace._\n",
        slice.entities.len(),
        slice.edges.len(),
        opts.license_filter
            .map(|c| format!(" · citing {c} sources only"))
            .unwrap_or_default(),
        e.canonical_name
    ));
    Ok(md)
}
