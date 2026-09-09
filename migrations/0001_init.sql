-- World-model entity graph schema. Self-contained: one SQLite file, no external FKs.

CREATE TABLE wm_entities (
    id TEXT PRIMARY KEY,
    entity_type TEXT NOT NULL,
    canonical_name TEXT NOT NULL,
    aliases TEXT NOT NULL DEFAULT '[]',       -- JSON array of strings
    external_ids TEXT NOT NULL DEFAULT '{}',  -- JSON object
    confidence REAL NOT NULL DEFAULT 0.5,
    metadata TEXT NOT NULL DEFAULT '{}',      -- JSON object
    first_seen TEXT NOT NULL,
    last_seen TEXT NOT NULL
);
CREATE INDEX idx_wm_entities_canonical_name ON wm_entities(canonical_name);
CREATE INDEX idx_wm_entities_type ON wm_entities(entity_type);

CREATE TABLE wm_edges (
    id TEXT PRIMARY KEY,
    from_id TEXT NOT NULL REFERENCES wm_entities(id) ON DELETE CASCADE,
    to_id TEXT NOT NULL REFERENCES wm_entities(id) ON DELETE CASCADE,
    edge_type TEXT NOT NULL,
    weight REAL NOT NULL DEFAULT 1.0,
    metadata TEXT NOT NULL DEFAULT '{}',
    source_item_ids TEXT NOT NULL DEFAULT '[]', -- JSON array of wm_source_items.id
    first_seen TEXT NOT NULL,
    last_seen TEXT NOT NULL
);
CREATE INDEX idx_wm_edges_from ON wm_edges(from_id);
CREATE INDEX idx_wm_edges_to ON wm_edges(to_id);

CREATE TABLE wm_source_items (
    id TEXT PRIMARY KEY,
    source_type TEXT NOT NULL,     -- 'rss' | 'pulse' | future adapters
    source_ref TEXT NOT NULL,      -- URL or upstream id
    content_hash TEXT NOT NULL UNIQUE,
    title TEXT,
    raw_text TEXT NOT NULL,
    ingested_at TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending' -- pending | resolved | needs_llm
);
CREATE INDEX idx_wm_source_items_status ON wm_source_items(status);

CREATE TABLE wm_extraction_candidates (
    id TEXT PRIMARY KEY,
    source_item_id TEXT NOT NULL REFERENCES wm_source_items(id) ON DELETE CASCADE,
    mention_text TEXT NOT NULL,
    mention_type_guess TEXT,
    best_match_entity_id TEXT REFERENCES wm_entities(id),
    match_score REAL,
    status TEXT NOT NULL DEFAULT 'pending_review', -- auto_merged | pending_review | needs_llm | resolved
    created_at TEXT NOT NULL
);
CREATE INDEX idx_wm_candidates_status ON wm_extraction_candidates(status);
CREATE INDEX idx_wm_candidates_source ON wm_extraction_candidates(source_item_id);

-- checkpoint bookkeeping for optional adapters (e.g. PulseContentReader's last-seen cursor)
CREATE TABLE wm_adapter_checkpoints (
    adapter_name TEXT PRIMARY KEY,
    cursor TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
