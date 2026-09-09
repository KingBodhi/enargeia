-- Phase 2: probabilistic matcher support.
--   wm_entity_keys      blocking index over canonical names + aliases (token / phonetic / normalized)
--   feature_scores      per-candidate feature breakdown (JSON) for review UI + provenance
--   wm_decisions        every human decision on a candidate, with actor + reason (audit trail)

CREATE TABLE wm_entity_keys (
    entity_id TEXT NOT NULL REFERENCES wm_entities(id) ON DELETE CASCADE,
    key_type TEXT NOT NULL,   -- 'norm' (whole normalized name) | 'token' (each token) | 'phon' (phonetic of head token)
    key TEXT NOT NULL,
    PRIMARY KEY (entity_id, key_type, key)
);
CREATE INDEX idx_wm_entity_keys_lookup ON wm_entity_keys(key_type, key);

ALTER TABLE wm_extraction_candidates ADD COLUMN feature_scores TEXT; -- JSON: {feature: {level, weight}}, total

CREATE TABLE wm_decisions (
    id TEXT PRIMARY KEY,
    candidate_id TEXT NOT NULL REFERENCES wm_extraction_candidates(id) ON DELETE CASCADE,
    decision TEXT NOT NULL,   -- confirm | reject | new
    actor TEXT NOT NULL,      -- 'cli' | user identifier
    reason TEXT,
    decided_at TEXT NOT NULL
);
CREATE INDEX idx_wm_decisions_candidate ON wm_decisions(candidate_id);
