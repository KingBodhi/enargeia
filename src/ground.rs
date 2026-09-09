//! Relation grounding. Mention grounding (the name occurs in the text) stops invented
//! entities; it does nothing about invented *relations between real entities* — a 7B model
//! will happily emit "Samsung acquired OpenAI" from an article that mentions both. A typed
//! relation is accepted only when the two names occur near each other in the source text
//! and, for consequential relation types, a lexical cue for that relation sits in the same
//! window; and the entity types make sense for the relation.

/// Characters between the two mentions within which they count as "near".
const PROXIMITY: usize = 400;

fn cues(rel: &str) -> Option<&'static [&'static str]> {
    Some(match rel {
        "acquired" | "acquired_by" | "merged_with" => &[
            "acqui", "buy", "bought", "purchas", "takeover", "merg", "deal to", "sold to",
        ],
        "ceo_of" => &["ceo", "chief executive"],
        "cfo_of" => &["cfo", "chief financial"],
        "cto_of" => &["cto", "chief technology", "chief technical"],
        "chairman_of" | "chair_of" => &["chair"],
        "founder_of" | "cofounder_of" | "co_founder_of" => &["found", "started", "created"],
        "suing" | "sued_by" | "sues" | "lawsuit_against" => &[
            "sue",
            "suit",
            "litigat",
            "court",
            "complaint",
            "legal action",
            "plaintiff",
            "defendant",
        ],
        "invested_in" | "investor_in" | "funded" | "funded_by" | "backed_by" => &[
            "invest",
            "fund",
            "raised",
            "round",
            "backed",
            "stake",
            "capital",
            "valuation",
        ],
        "partner_of" | "partnered_with" | "partners_with" => &[
            "partner",
            "collaborat",
            "team up",
            "teamed up",
            "alliance",
            "agreement",
            "deal",
            "joint",
        ],
        "headquartered_in" | "based_in" => &["headquart", "based", "hq", "offices"],
        "competes_with" | "competitor_of" | "rival_of" => &["compet", "rival"],
        "launched" | "launches" => &["launch", "releas", "unveil", "introduc", "announc"],
        "indicted" | "charged" | "sanctioned" | "fined" => &[
            "indict", "charg", "sanction", "fine", "penalt", "settle", "plead", "convict",
        ],
        _ => return None,
    })
}

/// Person-only subjects: someone holds the role.
const ROLE_RELATIONS: &[&str] = &[
    "ceo_of",
    "cfo_of",
    "cto_of",
    "chairman_of",
    "chair_of",
    "founder_of",
    "cofounder_of",
    "co_founder_of",
    "employed_by",
    "works_at",
    "member_of",
];
/// Object cannot be a person.
const ORG_OBJECT_RELATIONS: &[&str] = &[
    "acquired",
    "acquired_by",
    "merged_with",
    "invested_in",
    "headquartered_in",
    "based_in",
    "ceo_of",
    "cfo_of",
    "cto_of",
    "chairman_of",
    "founder_of",
];

fn types_ok(rel: &str, type_a: &str, type_b: &str) -> bool {
    let person = |t: &str| t == "person";
    let placeish = |t: &str| t == "location" || t == "event";
    if ROLE_RELATIONS.contains(&rel) && !(person(type_a) || type_a == "other") {
        return false;
    }
    if ORG_OBJECT_RELATIONS.contains(&rel) && person(type_b) {
        return false;
    }
    if matches!(
        rel,
        "acquired" | "acquired_by" | "merged_with" | "invested_in"
    ) && (placeish(type_a) || placeish(type_b) || person(type_a))
    {
        return false;
    }
    if matches!(rel, "headquartered_in" | "based_in" | "located_in")
        && type_b != "location"
        && type_b != "other"
    {
        return false;
    }
    true
}

fn occurrences(hay: &str, needle: &str) -> Vec<usize> {
    let mut out = Vec::new();
    if needle.len() < 2 {
        return out;
    }
    let mut from = 0;
    while let Some(p) = hay[from..].find(needle) {
        out.push(from + p);
        from += p + needle.len();
        if from >= hay.len() {
            break;
        }
    }
    out
}

/// Closest pair of occurrences as (a_pos, b_pos), if within `PROXIMITY`.
fn nearest_pair(text_lower: &str, a: &str, b: &str) -> Option<(usize, usize)> {
    let oa = occurrences(text_lower, a);
    let ob = occurrences(text_lower, b);
    let mut best: Option<(usize, usize, usize)> = None;
    for &x in &oa {
        for &y in &ob {
            let gap = x.abs_diff(y);
            if gap <= PROXIMITY && best.is_none_or(|(_, _, g)| gap < g) {
                best = Some((x, y, gap));
            }
        }
    }
    best.map(|(x, y, _)| (x, y))
}

/// Acquisitions and mergers are claimed in one sentence ("X acquired Y"); a role can sit a
/// sentence away from the organization it refers to ("…the Braintrust team. For founder and
/// CEO Ankur Goyal…"), so other cued relations search both sentences.
fn same_sentence_required(rel: &str) -> bool {
    matches!(
        rel,
        "acquired" | "acquired_by" | "merged_with" | "suing" | "sued_by"
    )
}

fn floor_char(text: &str, mut i: usize) -> usize {
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char(text: &str, mut i: usize) -> usize {
    while i < text.len() && !text.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// `a` and `b` are the surface forms (any alias that occurs works; pass the one that does).
/// `text_lower` is the lowercased source text; names are lowercased here.
pub fn relation_grounded(
    text_lower: &str,
    a: &str,
    b: &str,
    rel: &str,
    type_a: &str,
    type_b: &str,
) -> bool {
    if !types_ok(rel, type_a, type_b) {
        return false;
    }
    let a = a.trim().to_lowercase();
    let b = b.trim().to_lowercase();
    if a.is_empty() || b.is_empty() || a == b {
        return false;
    }
    let Some((pa, pb)) = nearest_pair(text_lower, &a, &b) else {
        return false;
    };
    match cues(rel) {
        None => true,
        Some(words) => {
            let (la, ha) = sentence_bounds(text_lower, pa);
            let (lb, hb) = sentence_bounds(text_lower, pb);
            if same_sentence_required(rel) && la != lb {
                return false;
            }
            let lo = floor_char(text_lower, la.min(lb));
            let hi = ceil_char(text_lower, ha.max(hb).min(text_lower.len()));
            let window = &text_lower[lo..hi];
            words.iter().any(|w| window.contains(w))
        }
    }
}

/// Byte range of the sentence containing `pos`: from the previous terminator (`.`, `!`, `?`
/// followed by whitespace, or a newline) to the next one.
fn sentence_bounds(text: &str, pos: usize) -> (usize, usize) {
    let bytes = text.as_bytes();
    let is_end = |i: usize| -> bool {
        match bytes[i] {
            b'\n' => true,
            b'.' | b'!' | b'?' => i + 1 >= bytes.len() || bytes[i + 1].is_ascii_whitespace(),
            _ => false,
        }
    };
    let mut lo = pos;
    while lo > 0 {
        if is_end(lo - 1) {
            break;
        }
        lo -= 1;
    }
    let mut hi = pos;
    while hi < bytes.len() && !is_end(hi) {
        hi += 1;
    }
    (lo, hi.min(bytes.len()))
}

/// Picks the first of `names` that occurs in the text (for auditing stored edges, where only
/// canonical names and aliases are known, not the original mention).
pub fn occurring_name<'a>(text_lower: &str, names: &[&'a str]) -> Option<&'a str> {
    names
        .iter()
        .copied()
        .find(|n| n.len() >= 2 && text_lower.contains(&n.to_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "OpenAI said on Tuesday that Sam Altman, its chief executive, will testify. \
Separately, Samsung Electronics reported quarterly earnings that beat estimates. \
Later in the piece: Rockset, the analytics database startup, was acquired by OpenAI last year for an undisclosed sum.";

    fn t() -> String {
        TEXT.to_lowercase()
    }

    #[test]
    fn accepts_cued_nearby_relations() {
        assert!(relation_grounded(
            &t(),
            "Sam Altman",
            "OpenAI",
            "ceo_of",
            "person",
            "organization"
        ));
        assert!(relation_grounded(
            &t(),
            "OpenAI",
            "Rockset",
            "acquired",
            "organization",
            "organization"
        ));
    }

    #[test]
    fn rejects_uncued_or_far_or_ill_typed() {
        // Both grounded, but nothing says Samsung bought anything.
        assert!(!relation_grounded(
            &t(),
            "Samsung Electronics",
            "OpenAI",
            "acquired",
            "organization",
            "organization"
        ));
        // An organization cannot be someone's CEO.
        assert!(!relation_grounded(
            &t(),
            "Microsoft",
            "OpenAI",
            "ceo_of",
            "organization",
            "organization"
        ));
        // A person cannot be acquired.
        assert!(!relation_grounded(
            &t(),
            "OpenAI",
            "Sam Altman",
            "acquired",
            "organization",
            "person"
        ));
        // Not in the text at all.
        assert!(!relation_grounded(
            &t(),
            "Anthropic",
            "OpenAI",
            "partner_of",
            "organization",
            "organization"
        ));
    }

    #[test]
    fn role_may_sit_one_sentence_from_its_organization() {
        let t = "In one month, half of the Braintrust team moved to Codex. For founder and CEO Ankur Goyal, the biggest change is speed.".to_lowercase();
        assert!(relation_grounded(
            &t,
            "Ankur Goyal",
            "Braintrust",
            "ceo_of",
            "person",
            "organization"
        ));
        // But an acquisition claimed across a sentence boundary is not accepted.
        let t2 =
            "Samsung Electronics reported earnings. Rockset was acquired by OpenAI.".to_lowercase();
        assert!(!relation_grounded(
            &t2,
            "Samsung Electronics",
            "OpenAI",
            "acquired",
            "organization",
            "organization"
        ));
        assert!(relation_grounded(
            &t2,
            "OpenAI",
            "Rockset",
            "acquired",
            "organization",
            "organization"
        ));
    }

    #[test]
    fn uncued_relation_types_need_only_proximity() {
        assert!(relation_grounded(
            &t(),
            "Sam Altman",
            "OpenAI",
            "related_to",
            "person",
            "organization"
        ));
        assert_eq!(
            occurring_name(&t(), &["Rockset Inc", "Rockset"]),
            Some("Rockset")
        );
    }
}
