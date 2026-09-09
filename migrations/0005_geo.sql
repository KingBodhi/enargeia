-- Phase 6: geocoding. Gazetteer from GeoNames (CC BY 4.0, attribution in DATA-ATTRIBUTION.md)
-- and per-entity coordinates for location-type entities.

CREATE TABLE wm_geo_places (
    geonameid INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    ascii TEXT NOT NULL,
    lat REAL NOT NULL,
    lon REAL NOT NULL,
    country TEXT,           -- ISO-3166 alpha-2
    feature_code TEXT,      -- PPLC = capital, PPLA = admin seat, PPL = populated place...
    population INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_wm_geo_places_country_code ON wm_geo_places(country, feature_code);

-- Every lookup name (name, ascii name, alternate names) normalized -> place.
CREATE TABLE wm_geo_names (
    name_norm TEXT NOT NULL,
    geonameid INTEGER NOT NULL REFERENCES wm_geo_places(geonameid) ON DELETE CASCADE
);
CREATE INDEX idx_wm_geo_names_norm ON wm_geo_names(name_norm);

CREATE TABLE wm_geo_countries (
    iso TEXT PRIMARY KEY,
    name_norm TEXT NOT NULL,
    name TEXT NOT NULL,
    capital TEXT
);
CREATE INDEX idx_wm_geo_countries_name ON wm_geo_countries(name_norm);

CREATE TABLE wm_geo (
    entity_id TEXT PRIMARY KEY REFERENCES wm_entities(id) ON DELETE CASCADE,
    lat REAL NOT NULL,
    lon REAL NOT NULL,
    geo_confidence REAL NOT NULL,
    geonameid INTEGER,
    place_name TEXT,
    geocoded_at TEXT NOT NULL
);
