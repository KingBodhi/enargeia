-- Phase 8: scoped API tokens, so a second tenant (a client) can use the engine without
-- being an operator. Tokens are stored hashed; the plaintext is shown once at creation.
--
--   role            operator | analyst | client
--   ask_daily_limit NULL = unlimited
--   license_filter  NULL = every source; 'commercial_clean' = cite only commercially clean
--                   sources (the honest default for paying clients)

CREATE TABLE wm_api_tokens (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    role TEXT NOT NULL,
    ask_daily_limit INTEGER,
    license_filter TEXT,
    created_at TEXT NOT NULL,
    revoked_at TEXT,
    last_used_at TEXT
);

CREATE TABLE wm_api_usage (
    token_id TEXT NOT NULL REFERENCES wm_api_tokens(id) ON DELETE CASCADE,
    day TEXT NOT NULL,            -- YYYY-MM-DD (UTC)
    asks INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (token_id, day)
);
