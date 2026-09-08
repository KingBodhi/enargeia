-- Closes two gaps found by comparing our design against Anduril Lattice's public schema
-- (see memory osint_defense_landscape_2026_09_08 / [[pythia_worldmodel_digital_twin]]):
--   1. sticky decorrelation - a human "these are NOT the same entity" decision must persist
--      so the resolver never re-proposes a rejected merge.
--   2. entity liveness/expiry - facts shouldn't live forever with equal weight in a world model
--      tracking a changing reality.
-- Also adds license_class to source items so commercial output can be gated per-feed from day one
-- (feed licensing map: GDELT/FIRMS/USGS clean, ACLED/GTD/Cloudflare Radar/GFW poisoned for commercial use).

ALTER TABLE wm_entities ADD COLUMN is_live INTEGER NOT NULL DEFAULT 1;
ALTER TABLE wm_entities ADD COLUMN expiry_time TEXT; -- NULL = no expiry; else ISO8601, refreshed on touch
ALTER TABLE wm_entities ADD COLUMN last_source_update_time TEXT;

CREATE INDEX idx_wm_entities_live_expiry ON wm_entities(is_live, expiry_time);

CREATE TABLE wm_decorrelations (
    id TEXT PRIMARY KEY,
    entity_a_id TEXT NOT NULL REFERENCES wm_entities(id) ON DELETE CASCADE,
    entity_b_id TEXT NOT NULL REFERENCES wm_entities(id) ON DELETE CASCADE,
    reason TEXT,
    decided_at TEXT NOT NULL,
    -- canonical ordering (entity_a_id < entity_b_id) enforced in application code so lookups
    -- don't need to check both orderings twice; UNIQUE still guards against duplicate decisions.
    UNIQUE(entity_a_id, entity_b_id)
);
CREATE INDEX idx_wm_decorrelations_a ON wm_decorrelations(entity_a_id);
CREATE INDEX idx_wm_decorrelations_b ON wm_decorrelations(entity_b_id);

ALTER TABLE wm_source_items ADD COLUMN license_class TEXT NOT NULL DEFAULT 'unknown';
-- license_class values: 'commercial_clean' | 'non_commercial_only' | 'unknown'
CREATE INDEX idx_wm_source_items_license ON wm_source_items(license_class);
