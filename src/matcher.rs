//! Multi-signal probabilistic matcher in the Fellegi–Sunter style.
//!
//! A single string metric cannot separate "Mistral" / "Mistral AI" (same entity) from
//! "CoinShares" / "Coinbase" (different) — both score ~0.9 on Jaro-Winkler. Production
//! entity resolution (Quantexa, Palantir's public descriptions) combines several comparison
//! functions with weighted evidence. Each feature below is bucketed into discrete levels and
//! each level carries a weight approximating log2(m/u): positive evidence for a match,
//! negative against. The total decides merge / review / new.
//!
//! Weights are hand-set priors; `enargeia eval match` measures them against
//! `eval/labels.jsonl` and writes the numbers to `eval/REPORT.md`.

use rphonetic::{DoubleMetaphone, Encoder};
use serde::{Deserialize, Serialize};

/// Tokens that add no identity: corporate suffixes, articles, generic org nouns. They are
/// kept in the normalized name (so exact/alias matching still works) but excluded from the
/// content-token features, so "Mistral" and "Mistral AI" share all content tokens.
pub const IGNORABLE_TOKENS: &[&str] = &[
    "inc",
    "incorporated",
    "corp",
    "corporation",
    "co",
    "company",
    "ltd",
    "limited",
    "llc",
    "plc",
    "sa",
    "ag",
    "gmbh",
    "pbc",
    "holdings",
    "group",
    "technologies",
    "technology",
    "labs",
    "lab",
    "ai",
    "research",
    "foundation",
    "the",
    "of",
    "and",
    "de",
    "la",
    "le",
    "les",
    "el",
];

pub fn normalize(s: &str) -> String {
    let lower = s.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    for c in lower.chars() {
        if c.is_alphanumeric() {
            out.push(c);
        } else if c == '&' {
            out.push_str(" and ");
        } else {
            out.push(' ');
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn tokens(norm: &str) -> Vec<String> {
    norm.split_whitespace().map(str::to_string).collect()
}

/// Tokens minus [`IGNORABLE_TOKENS`]; falls back to all tokens if nothing would remain.
pub fn content_tokens(norm: &str) -> Vec<String> {
    let all = tokens(norm);
    let content: Vec<String> = all
        .iter()
        .filter(|t| !IGNORABLE_TOKENS.contains(&t.as_str()))
        .cloned()
        .collect();
    if content.is_empty() {
        all
    } else {
        content
    }
}

/// Folds common Latin diacritics to ASCII and drops anything else; the phonetic encoder is
/// ASCII-only and panics on multibyte input.
pub fn ascii_fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let mapped: &str = match c {
            'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' | 'ā' => "a",
            'Á' | 'À' | 'Â' | 'Ä' | 'Ã' | 'Å' | 'Ā' => "A",
            'é' | 'è' | 'ê' | 'ë' | 'ē' | 'ě' => "e",
            'É' | 'È' | 'Ê' | 'Ë' | 'Ē' | 'Ě' => "E",
            'í' | 'ì' | 'î' | 'ï' | 'ī' => "i",
            'Í' | 'Ì' | 'Î' | 'Ï' | 'Ī' => "I",
            'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'ø' | 'ō' => "o",
            'Ó' | 'Ò' | 'Ô' | 'Ö' | 'Õ' | 'Ø' | 'Ō' => "O",
            'ú' | 'ù' | 'û' | 'ü' | 'ū' | 'ů' => "u",
            'Ú' | 'Ù' | 'Û' | 'Ü' | 'Ū' | 'Ů' => "U",
            'ñ' | 'ň' => "n",
            'Ñ' | 'Ň' => "N",
            'ç' | 'ć' | 'č' => "c",
            'Ç' | 'Ć' | 'Č' => "C",
            'ß' => "ss",
            'ł' => "l",
            'Ł' => "L",
            'ż' | 'ź' | 'ž' => "z",
            'Ż' | 'Ź' | 'Ž' => "Z",
            'ş' | 'š' => "s",
            'Ş' | 'Š' => "S",
            'ğ' => "g",
            'Ğ' => "G",
            'ý' | 'ÿ' => "y",
            'Ý' => "Y",
            'ř' => "r",
            'Ř' => "R",
            'ď' => "d",
            'Ď' => "D",
            'ť' => "t",
            'Ť' => "T",
            _ if c.is_ascii() => {
                out.push(c);
                continue;
            }
            _ => "",
        };
        out.push_str(mapped);
    }
    out
}

pub fn phonetic(token: &str) -> String {
    let folded = ascii_fold(token);
    if folded.is_empty() || !folded.chars().any(|c| c.is_ascii_alphabetic()) {
        return String::new();
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        DoubleMetaphone::default().encode(&folded)
    }))
    .unwrap_or_default()
}

/// "FTC", "U.S.", "a16z"-style short uppercase forms.
pub fn is_acronym(raw: &str) -> bool {
    let t = raw.trim();
    let letters = t.chars().filter(|c| c.is_alphabetic()).count();
    letters >= 2
        && t.len() <= 7
        && t.chars()
            .all(|c| c.is_uppercase() || c.is_ascii_digit() || c == '.' || c == '&')
}

fn initials(content: &[String]) -> String {
    content.iter().filter_map(|t| t.chars().next()).collect()
}

fn jaro_winkler_best(mention_norm: &str, names_norm: &[String]) -> f64 {
    names_norm
        .iter()
        .map(|n| strsim::jaro_winkler(mention_norm, n))
        .fold(0.0_f64, f64::max)
}

fn jaccard(a: &[String], b: &[String]) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let sa: std::collections::HashSet<&str> = a.iter().map(String::as_str).collect();
    let sb: std::collections::HashSet<&str> = b.iter().map(String::as_str).collect();
    let inter = sa.intersection(&sb).count() as f64;
    let union = sa.union(&sb).count() as f64;
    inter / union
}

/// 2 = every token on both sides has a phonetic counterpart, 1 = one side is a phonetic
/// subset of the other, 0 = neither.
fn phonetic_level(a: &[String], b: &[String]) -> u8 {
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    let pa: Vec<String> = a
        .iter()
        .map(|t| phonetic(t))
        .filter(|p| !p.is_empty())
        .collect();
    let pb: Vec<String> = b
        .iter()
        .map(|t| phonetic(t))
        .filter(|p| !p.is_empty())
        .collect();
    if pa.is_empty() || pb.is_empty() {
        return 0;
    }
    let a_in_b = pa.iter().all(|p| pb.contains(p));
    let b_in_a = pb.iter().all(|p| pa.contains(p));
    match (a_in_b, b_in_a) {
        (true, true) => 2,
        (true, false) | (false, true) => 1,
        _ => 0,
    }
}

/// Containment structure between the two content-token sequences.
/// 2 = strong: a multi-token side is a contiguous prefix/suffix of the other ("Federal
///     Reserve" ⊂ "US Federal Reserve"), or a single token is the *last* token ("Amodei" ⊂
///     "Dario Amodei" — surnames identify people).
/// 1 = weak: a single token is the *first* token ("Anthony" ⊂ "Anthony Ha" — first names are
///     ambiguous; "Google" ⊂ "Google Cloud"), or a non-edge subset.
/// 0 = not a subset.
fn subset_level(a: &[String], b: &[String]) -> u8 {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if short.is_empty() || short.len() == long.len() || !short.iter().all(|t| long.contains(t)) {
        return 0;
    }
    let n = short.len();
    let is_prefix = long[..n] == *short;
    let is_suffix = long[long.len() - n..] == *short;
    match (n, is_prefix, is_suffix) {
        (1, _, true) => 2,
        (1, true, false) => 1,
        (_, true, _) | (_, _, true) => 2,
        _ => 1,
    }
}

/// 0 = hard conflict (person vs. location), 1 = soft conflict (organization vs. product —
/// NER routinely labels a company and its eponymous product either way), 2 = unknown on
/// either side, 3 = agree.
fn type_level(mention_type: Option<&str>, entity_type: &str) -> u8 {
    let m = mention_type.map(str::to_lowercase).unwrap_or_default();
    let e = entity_type.to_lowercase();
    let known = |t: &str| !t.is_empty() && t != "unknown" && t != "other";
    if !known(&m) || !known(&e) {
        2
    } else if m == e {
        3
    } else if matches!(
        (m.as_str(), e.as_str()),
        ("organization", "product") | ("product", "organization")
    ) {
        1
    } else {
        0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Features {
    pub name_jw: f64,
    pub name_level: u8,
    pub alias_exact: bool,
    pub token_jaccard: f64,
    pub phonetic: u8,
    /// 0 hard conflict, 1 soft conflict (organization/product), 2 unknown, 3 agree
    pub type_agree: u8,
    /// Content tokens present on only one side.
    pub extra_tokens: usize,
    /// 2 = shorter side is a contiguous prefix/suffix of the longer, 1 = subset, 0 = neither
    pub token_subset: u8,
    /// 1 = acronym matches the other side's initials, -1 = acronym mismatch, 0 = not applicable
    pub acronym: i8,
    pub cooc: f64,
    pub corroboration: u32,
    pub recent: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchScore {
    pub total: f64,
    /// (feature, weight applied) — the "why" behind the total, persisted for review/provenance.
    pub contributions: Vec<(String, f64)>,
    pub features: Features,
}

/// Everything the matcher knows about an existing entity, including DB-derived priors.
pub struct Candidate<'a> {
    pub canonical: &'a str,
    pub aliases: &'a [String],
    pub entity_type: &'a str,
    /// Share of the mention's co-mentioned (already resolved) entities that link to this candidate.
    pub cooc: f64,
    /// Distinct source items that have resolved to this candidate.
    pub corroboration: u32,
    pub recent: bool,
}

/// Hand-set evidence weights (≈ log2(m/u)). Tune against `eval/labels.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Weights {
    pub name: [f64; 4],
    pub alias_exact: f64,
    pub jaccard: [f64; 3],
    pub phonetic: [f64; 3],
    pub type_agree: [f64; 4],
    pub extra_tokens: [f64; 3],
    pub token_subset: [f64; 3],
    pub acronym_match: f64,
    pub acronym_mismatch: f64,
    pub cooc: [f64; 3],
    pub corroboration: [f64; 4],
    pub recent: f64,
    /// Ceiling on the summed runtime priors (cooc + corroboration + recency). Context can
    /// confirm a plausible name match; it must never bridge the review→merge gap on its own.
    pub prior_cap: f64,
    pub upper: f64,
    pub lower: f64,
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            name: [-3.0, -0.5, 2.5, 4.5],
            alias_exact: 3.0,
            jaccard: [-1.0, 1.0, 2.0],
            phonetic: [-0.5, 0.5, 1.0],
            type_agree: [-6.0, -1.5, 0.0, 1.0],
            extra_tokens: [0.0, -1.5, -2.5],
            token_subset: [0.0, 1.0, 2.5],
            acronym_match: 6.0,
            acronym_mismatch: -3.0,
            cooc: [0.0, 1.5, 2.5],
            corroboration: [0.0, 0.3, 0.7, 1.0],
            recent: 0.3,
            prior_cap: 2.0,
            upper: 5.0,
            lower: 1.0,
        }
    }
}

impl Weights {
    pub fn from_env() -> Self {
        let mut w = Self::default();
        if let Some(v) = std::env::var("ENARGEIA_MATCH_UPPER")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            w.upper = v;
        }
        if let Some(v) = std::env::var("ENARGEIA_MATCH_LOWER")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            w.lower = v;
        }
        w
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Merge,
    Review,
    New,
}

pub fn decide(total: f64, w: &Weights) -> Decision {
    if total >= w.upper {
        Decision::Merge
    } else if total < w.lower {
        Decision::New
    } else {
        Decision::Review
    }
}

/// Acronym on one side vs. a multi-token name on the other: 1 if the name's initials equal
/// (or end with — "U.S. Securities and Exchange Commission" → SEC) the acronym, else -1.
fn acronym_level(
    m_norm: &str,
    mention_raw: &str,
    m_content: &[String],
    cand_raw: &str,
    c_content: &[String],
) -> i8 {
    let m_acr = is_acronym(mention_raw);
    let c_acr = is_acronym(cand_raw);
    if m_acr && c_content.len() >= 2 {
        let acr = m_norm.replace(' ', "");
        let ini = initials(c_content);
        return if ini == acr || ini.ends_with(&acr) {
            1
        } else {
            -1
        };
    }
    if c_acr && m_content.len() >= 2 {
        let acr = normalize(cand_raw).replace(' ', "");
        let ini = initials(m_content);
        return if ini == acr || ini.ends_with(&acr) {
            1
        } else {
            -1
        };
    }
    if m_acr || c_acr {
        -1
    } else {
        0
    }
}

pub fn score(
    mention: &str,
    mention_type: Option<&str>,
    cand: &Candidate,
    w: &Weights,
) -> MatchScore {
    let m_norm = normalize(mention);
    let mut names_norm: Vec<String> = vec![normalize(cand.canonical)];
    names_norm.extend(cand.aliases.iter().map(|a| normalize(a)));
    names_norm.retain(|n| !n.is_empty());

    let alias_exact = !m_norm.is_empty() && names_norm.contains(&m_norm);
    let name_jw = jaro_winkler_best(&m_norm, &names_norm);
    let name_level = if name_jw >= 0.95 {
        3
    } else if name_jw >= 0.87 {
        2
    } else if name_jw >= 0.75 {
        1
    } else {
        0
    };

    let m_content = content_tokens(&m_norm);
    let c_norm = normalize(cand.canonical);
    // Compare content tokens against the best-overlapping name (canonical or alias).
    let (c_content, token_jaccard) = names_norm
        .iter()
        .map(|n| {
            let ct = content_tokens(n);
            let j = jaccard(&m_content, &ct);
            (ct, j)
        })
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .unwrap_or_else(|| (content_tokens(&c_norm), 0.0));
    let extra_tokens = {
        let sa: std::collections::HashSet<&str> = m_content.iter().map(String::as_str).collect();
        let sb: std::collections::HashSet<&str> = c_content.iter().map(String::as_str).collect();
        sa.symmetric_difference(&sb).count()
    };
    let token_subset = subset_level(&m_content, &c_content);
    let phon = phonetic_level(&m_content, &c_content);
    let type_agree = type_level(mention_type, cand.entity_type);
    let acronym = if alias_exact {
        0
    } else {
        acronym_level(&m_norm, mention, &m_content, cand.canonical, &c_content)
    };

    let features = Features {
        name_jw,
        name_level,
        alias_exact,
        token_jaccard,
        phonetic: phon,
        type_agree,
        extra_tokens,
        token_subset,
        acronym,
        cooc: cand.cooc,
        corroboration: cand.corroboration,
        recent: cand.recent,
    };

    let mut contributions: Vec<(String, f64)> = Vec::new();
    fn push(c: &mut Vec<(String, f64)>, name: &str, v: f64) {
        if v != 0.0 {
            c.push((name.to_string(), v));
        }
    }

    if acronym == 1 {
        // Initials match: string similarity is meaningless here (FTC vs Federal Trade
        // Commission scores ~0 on every string metric) — the acronym evidence stands in.
        push(&mut contributions, "acronym_match", w.acronym_match);
    } else {
        // Jaro-Winkler's prefix bonus makes "circle" ≈ "circle k"; when the other side carries
        // extra content tokens the top similarity level is not trustworthy. Conversely, when
        // one side is contained in the other, a low similarity is explained by the structure,
        // so the string penalty is capped.
        let mut name_idx = name_level as usize;
        if extra_tokens > 0 {
            name_idx = name_idx.min(2);
        }
        if token_subset >= 1 {
            name_idx = name_idx.max(1);
        }
        push(&mut contributions, "name_jw", w.name[name_idx]);
        if alias_exact {
            push(&mut contributions, "alias_exact", w.alias_exact);
        }
        let jl = if token_jaccard >= 0.8 {
            2
        } else if token_jaccard >= 0.5 {
            1
        } else {
            0
        };
        push(&mut contributions, "token_jaccard", w.jaccard[jl]);
        push(&mut contributions, "phonetic", w.phonetic[phon as usize]);
        push(
            &mut contributions,
            "extra_tokens",
            w.extra_tokens[extra_tokens.min(2)],
        );
        push(
            &mut contributions,
            "token_subset",
            w.token_subset[token_subset as usize],
        );
        if acronym == -1 {
            push(&mut contributions, "acronym_mismatch", w.acronym_mismatch);
        }
    }
    push(
        &mut contributions,
        "type_agree",
        w.type_agree[type_agree as usize],
    );

    // Runtime priors are applied only when the string/type evidence alone is at least
    // review-worthy — context cannot rescue a name that does not match — and their sum is
    // capped so they cannot alone carry a review-band pair over the merge threshold.
    let evidence: f64 = contributions.iter().map(|(_, v)| v).sum();
    if evidence >= w.lower {
        let cl = if cand.cooc >= 0.5 {
            2
        } else if cand.cooc > 0.0 {
            1
        } else {
            0
        };
        let kl = if cand.corroboration >= 8 {
            3
        } else if cand.corroboration >= 3 {
            2
        } else if cand.corroboration >= 1 {
            1
        } else {
            0
        };
        let recent = if cand.recent { w.recent } else { 0.0 };
        let raw = w.cooc[cl] + w.corroboration[kl] + recent;
        let scale = if raw > w.prior_cap {
            w.prior_cap / raw
        } else {
            1.0
        };
        push(&mut contributions, "cooc", w.cooc[cl] * scale);
        push(
            &mut contributions,
            "corroboration",
            w.corroboration[kl] * scale,
        );
        push(&mut contributions, "recent", recent * scale);
    }

    let total = contributions.iter().map(|(_, v)| v).sum();
    MatchScore {
        total,
        contributions,
        features,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand<'a>(canonical: &'a str, aliases: &'a [String], t: &'a str) -> Candidate<'a> {
        Candidate {
            canonical,
            aliases,
            entity_type: t,
            cooc: 0.0,
            corroboration: 0,
            recent: false,
        }
    }

    fn prior_sum(ms: &MatchScore) -> f64 {
        ms.contributions
            .iter()
            .filter(|(n, _)| matches!(n.as_str(), "cooc" | "corroboration" | "recent"))
            .map(|(_, v)| v)
            .sum()
    }

    #[test]
    fn context_priors_confirm_but_never_rescue_or_carry() {
        let w = Weights::default();
        let strong_context = |canonical: &'static str, t: &'static str| Candidate {
            canonical,
            aliases: &[],
            entity_type: t,
            cooc: 1.0,
            corroboration: 20,
            recent: true,
        };
        // A name that does not match gets no help from context at all.
        let ms = score(
            "OpenAI",
            Some("organization"),
            &strong_context("Open Source Initiative", "organization"),
            &w,
        );
        assert_eq!(prior_sum(&ms), 0.0, "{:?}", ms.contributions);
        assert_eq!(decide(ms.total, &w), Decision::New);

        // A review-band name gets context, but never more than the cap.
        let ms = score(
            "Circle",
            Some("organization"),
            &strong_context("Circle K", "organization"),
            &w,
        );
        assert!(prior_sum(&ms) > 0.0);
        assert!(
            prior_sum(&ms) <= w.prior_cap + 1e-9,
            "{:?}",
            ms.contributions
        );
        let raw = w.cooc[2] + w.corroboration[3] + w.recent;
        assert!(raw > w.prior_cap, "test assumes the cap binds");
    }

    #[test]
    fn normalizes_punctuation_and_case() {
        assert_eq!(normalize("Rebecca.Bellan"), "rebecca bellan");
        assert_eq!(normalize("U.S."), "u s");
        assert_eq!(normalize("Meta Platforms, Inc."), "meta platforms inc");
        assert_eq!(normalize("AT&T"), "at and t");
    }

    #[test]
    fn phonetic_survives_non_ascii() {
        assert!(!phonetic("Hegotá").is_empty());
        assert_eq!(phonetic("Hegotá"), phonetic("Hegota"));
        assert_eq!(phonetic("北京"), "");
        assert_eq!(ascii_fold("Meléndez"), "Melendez");
    }

    #[test]
    fn company_and_eponymous_product_still_merge() {
        let w = Weights::default();
        let s = score(
            "Cursor",
            Some("product"),
            &cand("Cursor", &[], "organization"),
            &w,
        );
        assert_eq!(decide(s.total, &w), Decision::Merge, "{s:?}");
    }

    #[test]
    fn suffix_variant_merges_but_division_reviews() {
        let w = Weights::default();
        let s = score(
            "Mistral",
            Some("organization"),
            &cand("Mistral AI", &[], "organization"),
            &w,
        );
        assert_eq!(decide(s.total, &w), Decision::Merge, "{s:?}");
        let s = score(
            "Google",
            Some("organization"),
            &cand("Google Cloud", &[], "organization"),
            &w,
        );
        assert_ne!(decide(s.total, &w), Decision::New, "{s:?}");
    }

    #[test]
    fn similar_strings_different_entities_are_new() {
        let w = Weights::default();
        for (m, c) in [
            ("CoinShares", "Coinbase"),
            ("Christopher Waller", "Christopher Woolard"),
            ("Cognizant", "Cognition"),
            ("Metaplanet", "Meta"),
        ] {
            let s = score(m, Some("organization"), &cand(c, &[], "organization"), &w);
            assert_ne!(decide(s.total, &w), Decision::Merge, "{m} vs {c}: {s:?}");
        }
    }

    #[test]
    fn acronyms() {
        let w = Weights::default();
        let s = score(
            "FTC",
            Some("organization"),
            &cand("Federal Trade Commission", &[], "organization"),
            &w,
        );
        assert_eq!(decide(s.total, &w), Decision::Merge, "{s:?}");
        let s = score(
            "U.S. Securities and Exchange Commission",
            Some("organization"),
            &cand("SEC", &[], "organization"),
            &w,
        );
        assert_eq!(decide(s.total, &w), Decision::Merge, "{s:?}");
        let s = score(
            "CFTC",
            Some("organization"),
            &cand("FTC", &[], "organization"),
            &w,
        );
        assert_eq!(decide(s.total, &w), Decision::New, "{s:?}");
        let s = score("ZCSH", Some("product"), &cand("Zcash", &[], "product"), &w);
        assert_ne!(decide(s.total, &w), Decision::Merge, "{s:?}");
    }

    #[test]
    fn uppercase_alias_is_not_an_acronym_mismatch() {
        let w = Weights::default();
        let aliases = vec!["META".to_string()];
        let s = score(
            "Meta Platforms Inc.",
            Some("organization"),
            &cand("Meta", &aliases, "organization"),
            &w,
        );
        assert!(s.features.acronym >= 0, "{s:?}");
        assert_ne!(decide(s.total, &w), Decision::New, "{s:?}");
    }

    #[test]
    fn surname_and_prefix_variants_reach_review() {
        let w = Weights::default();
        let s = score(
            "Amodei",
            Some("person"),
            &cand("Dario Amodei", &[], "person"),
            &w,
        );
        assert_ne!(decide(s.total, &w), Decision::New, "{s:?}");
        let s = score(
            "US Federal Reserve",
            Some("organization"),
            &cand("Federal Reserve", &[], "organization"),
            &w,
        );
        assert_ne!(decide(s.total, &w), Decision::New, "{s:?}");
    }

    #[test]
    fn exact_name_with_hard_type_conflict_is_not_auto_merged() {
        let w = Weights::default();
        let s = score(
            "Mercury",
            Some("product"),
            &cand("Mercury", &[], "location"),
            &w,
        );
        assert_ne!(decide(s.total, &w), Decision::Merge, "{s:?}");
    }

    #[test]
    fn dotted_handle_matches_person() {
        let w = Weights::default();
        let s = score(
            "rebecca.bellan",
            Some("person"),
            &cand("Rebecca Bellan", &[], "person"),
            &w,
        );
        assert_eq!(decide(s.total, &w), Decision::Merge, "{s:?}");
        let s = score(
            "Anthony",
            Some("person"),
            &cand("Anthony Ha", &[], "person"),
            &w,
        );
        assert_ne!(decide(s.total, &w), Decision::Merge, "{s:?}");
    }

    #[test]
    fn alias_resolves_short_form() {
        let w = Weights::default();
        let aliases = vec!["Andreessen Horowitz".to_string()];
        let s = score(
            "Andreessen Horowitz",
            Some("organization"),
            &cand("a16z", &aliases, "organization"),
            &w,
        );
        assert_eq!(decide(s.total, &w), Decision::Merge, "{s:?}");
    }
}
