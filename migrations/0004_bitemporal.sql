-- Phase 3: bi-temporal edges (modeled on Graphiti's valid_at / invalid_at) and source
-- publication time.
--   first_seen / last_seen  = when Enargeia observed the relation (transaction time)
--   valid_at / invalid_at   = when the relation held in the world (valid time)
--   superseded_by           = the newer contradicting edge that invalidated this one
-- Exclusive relation types (e.g. ceo_of, headquartered_in) invalidate their predecessor
-- when a newer contradicting fact arrives; older facts arriving late are inserted already
-- invalidated, so processing order does not rewrite history.

ALTER TABLE wm_source_items ADD COLUMN published_at TEXT;

ALTER TABLE wm_edges ADD COLUMN valid_at TEXT;
ALTER TABLE wm_edges ADD COLUMN invalid_at TEXT;
ALTER TABLE wm_edges ADD COLUMN superseded_by TEXT;
UPDATE wm_edges SET valid_at = first_seen WHERE valid_at IS NULL;

CREATE INDEX idx_wm_edges_valid_from ON wm_edges(from_id, edge_type, invalid_at);
CREATE INDEX idx_wm_edges_valid_to ON wm_edges(to_id, edge_type, invalid_at);
