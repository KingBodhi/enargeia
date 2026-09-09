//! Geocoding for location-type entities against a GeoNames gazetteer (cities15000 +
//! countryInfo; CC BY 4.0). Countries resolve to their capital; places resolve by exact
//! normalized name (name, ASCII name, or any alternate name), preferring population.

use std::{io::Read, path::Path};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::{matcher::normalize, models::now};

const CITIES_URL: &str = "https://download.geonames.org/export/dump/cities15000.zip";
const COUNTRIES_URL: &str = "https://download.geonames.org/export/dump/countryInfo.txt";

pub async fn fetch_gazetteer(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let client = reqwest::Client::builder()
        .user_agent(crate::USER_AGENT)
        .build()?;

    let cities_txt = dir.join("cities15000.txt");
    if !cities_txt.exists() {
        println!("downloading {CITIES_URL}");
        let bytes = client
            .get(CITIES_URL)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        let mut archive =
            zip::ZipArchive::new(std::io::Cursor::new(bytes)).context("opening cities15000.zip")?;
        let mut file = archive
            .by_name("cities15000.txt")
            .context("cities15000.txt missing from zip")?;
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        std::fs::write(&cities_txt, text)?;
        println!("saved {}", cities_txt.display());
    } else {
        println!("exists: {}", cities_txt.display());
    }

    let countries_txt = dir.join("countryInfo.txt");
    if !countries_txt.exists() {
        println!("downloading {COUNTRIES_URL}");
        let text = client
            .get(COUNTRIES_URL)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        std::fs::write(&countries_txt, text)?;
        println!("saved {}", countries_txt.display());
    } else {
        println!("exists: {}", countries_txt.display());
    }
    Ok(())
}

/// Loads the gazetteer files into the database (idempotent: clears and reloads).
pub async fn load_gazetteer(pool: &SqlitePool, dir: &Path) -> Result<(usize, usize, usize)> {
    let cities = std::fs::read_to_string(dir.join("cities15000.txt"))
        .context("cities15000.txt not found; run `enargeia geo fetch`")?;
    let countries = std::fs::read_to_string(dir.join("countryInfo.txt"))
        .context("countryInfo.txt not found; run `enargeia geo fetch`")?;

    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM wm_geo_names")
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM wm_geo_places")
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM wm_geo_countries")
        .execute(&mut *tx)
        .await?;

    let mut n_places = 0usize;
    let mut n_names = 0usize;
    for line in cities.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 15 {
            continue;
        }
        let Ok(id) = f[0].parse::<i64>() else {
            continue;
        };
        let (Ok(lat), Ok(lon)) = (f[4].parse::<f64>(), f[5].parse::<f64>()) else {
            continue;
        };
        let population = f[14].parse::<i64>().unwrap_or(0);
        sqlx::query(
            "INSERT INTO wm_geo_places (geonameid, name, ascii, lat, lon, country, feature_code, population) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(f[1])
        .bind(f[2])
        .bind(lat)
        .bind(lon)
        .bind(f[8])
        .bind(f[7])
        .bind(population)
        .execute(&mut *tx)
        .await?;
        n_places += 1;

        let mut names: Vec<String> = vec![normalize(f[1]), normalize(f[2])];
        for alt in f[3].split(',') {
            let n = normalize(alt);
            if n.len() >= 3 {
                names.push(n);
            }
        }
        names.sort();
        names.dedup();
        for n in names {
            if n.is_empty() {
                continue;
            }
            sqlx::query("INSERT INTO wm_geo_names (name_norm, geonameid) VALUES (?, ?)")
                .bind(&n)
                .bind(id)
                .execute(&mut *tx)
                .await?;
            n_names += 1;
        }
    }

    let mut n_countries = 0usize;
    for line in countries.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 6 {
            continue;
        }
        sqlx::query(
            "INSERT INTO wm_geo_countries (iso, name_norm, name, capital) VALUES (?, ?, ?, ?)",
        )
        .bind(f[0])
        .bind(normalize(f[4]))
        .bind(f[4])
        .bind(f[5])
        .execute(&mut *tx)
        .await?;
        n_countries += 1;
    }
    tx.commit().await?;
    Ok((n_places, n_names, n_countries))
}

/// Common alternate spellings for countries the gazetteer names formally.
fn country_synonym(norm: &str) -> Option<&'static str> {
    Some(match norm {
        "us" | "usa" | "u s" | "u s a" | "united states of america" | "america" => "united states",
        "uk" | "u k" | "britain" | "great britain" => "united kingdom",
        "uae" | "u a e" => "united arab emirates",
        "russia" => "russia",
        "south korea" | "korea" => "south korea",
        "north korea" => "north korea",
        "czechia" | "czech republic" => "czechia",
        "holland" | "the netherlands" => "netherlands",
        _ => return None,
    })
}

struct Hit {
    lat: f64,
    lon: f64,
    confidence: f64,
    geonameid: Option<i64>,
    place: String,
}

async fn lookup_country(pool: &SqlitePool, norm: &str) -> Result<Option<Hit>> {
    let key = country_synonym(norm).unwrap_or(norm);
    let row: Option<(String, String, Option<String>)> =
        sqlx::query_as("SELECT iso, name, capital FROM wm_geo_countries WHERE name_norm = ?")
            .bind(key)
            .fetch_optional(pool)
            .await?;
    let Some((iso, name, capital)) = row else {
        return Ok(None);
    };
    // Capital city coordinates stand in for the country.
    let cap: Option<(i64, f64, f64, String)> = sqlx::query_as(
        "SELECT geonameid, lat, lon, name FROM wm_geo_places WHERE country = ? AND feature_code = 'PPLC' ORDER BY population DESC LIMIT 1",
    )
    .bind(&iso)
    .fetch_optional(pool)
    .await?;
    let cap = match cap {
        Some(c) => Some(c),
        None => {
            let capital = capital.unwrap_or_default();
            sqlx::query_as(
                "SELECT p.geonameid, p.lat, p.lon, p.name FROM wm_geo_places p JOIN wm_geo_names n ON n.geonameid = p.geonameid \
                 WHERE p.country = ? AND n.name_norm = ? ORDER BY p.population DESC LIMIT 1",
            )
            .bind(&iso)
            .bind(normalize(&capital))
            .fetch_optional(pool)
            .await?
        }
    };
    Ok(cap.map(|(id, lat, lon, cname)| Hit {
        lat,
        lon,
        confidence: 0.9,
        geonameid: Some(id),
        place: format!("{name} (capital: {cname})"),
    }))
}

async fn lookup_place(pool: &SqlitePool, norm: &str) -> Result<Option<Hit>> {
    let rows: Vec<(i64, f64, f64, String, Option<String>, i64)> = sqlx::query_as(
        "SELECT DISTINCT p.geonameid, p.lat, p.lon, p.name, p.country, p.population FROM wm_geo_places p \
         JOIN wm_geo_names n ON n.geonameid = p.geonameid WHERE n.name_norm = ? ORDER BY p.population DESC LIMIT 5",
    )
    .bind(norm)
    .fetch_all(pool)
    .await?;
    let Some((id, lat, lon, name, country, _)) = rows.first().cloned() else {
        return Ok(None);
    };
    let ambiguous = rows.len() > 1;
    Ok(Some(Hit {
        lat,
        lon,
        confidence: if ambiguous { 0.6 } else { 0.8 },
        geonameid: Some(id),
        place: format!(
            "{name}{}",
            country.map(|c| format!(", {c}")).unwrap_or_default()
        ),
    }))
}

async fn resolve_name(pool: &SqlitePool, raw: &str) -> Result<Option<Hit>> {
    let norm = normalize(raw);
    if norm.is_empty() {
        return Ok(None);
    }
    if let Some(h) = lookup_country(pool, &norm).await? {
        return Ok(Some(h));
    }
    if let Some(h) = lookup_place(pool, &norm).await? {
        return Ok(Some(h));
    }
    // "Austin, Texas" / "Paris, France": try the leading segment with reduced confidence.
    if let Some((head, _)) = raw.split_once(',') {
        let head_norm = normalize(head);
        if head_norm.len() >= 3 {
            if let Some(mut h) = lookup_place(pool, &head_norm).await? {
                h.confidence *= 0.8;
                return Ok(Some(h));
            }
        }
    }
    Ok(None)
}

#[derive(Debug, Default)]
pub struct GeocodeStats {
    pub considered: usize,
    pub geocoded: usize,
}

/// Geocodes live location-type entities that do not have coordinates yet.
pub async fn geocode_entities(pool: &SqlitePool, all: bool) -> Result<GeocodeStats> {
    let mut stats = GeocodeStats::default();
    let sql = if all {
        "SELECT id, canonical_name, aliases FROM wm_entities WHERE entity_type = 'location' AND is_live = 1"
    } else {
        "SELECT e.id, e.canonical_name, e.aliases FROM wm_entities e LEFT JOIN wm_geo g ON g.entity_id = e.id \
         WHERE e.entity_type = 'location' AND e.is_live = 1 AND g.entity_id IS NULL"
    };
    let rows: Vec<(String, String, String)> = sqlx::query_as(sql).fetch_all(pool).await?;
    for (id, name, aliases_json) in rows {
        stats.considered += 1;
        let mut names = vec![name.clone()];
        names.extend(serde_json::from_str::<Vec<String>>(&aliases_json).unwrap_or_default());
        let mut best: Option<Hit> = None;
        for n in &names {
            if let Some(h) = resolve_name(pool, n).await? {
                if best
                    .as_ref()
                    .map(|b| h.confidence > b.confidence)
                    .unwrap_or(true)
                {
                    best = Some(h);
                }
            }
        }
        if let Some(h) = best {
            sqlx::query(
                "INSERT INTO wm_geo (entity_id, lat, lon, geo_confidence, geonameid, place_name, geocoded_at) VALUES (?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(entity_id) DO UPDATE SET lat = excluded.lat, lon = excluded.lon, geo_confidence = excluded.geo_confidence, \
                 geonameid = excluded.geonameid, place_name = excluded.place_name, geocoded_at = excluded.geocoded_at",
            )
            .bind(&id)
            .bind(h.lat)
            .bind(h.lon)
            .bind(h.confidence)
            .bind(h.geonameid)
            .bind(&h.place)
            .bind(now())
            .execute(pool)
            .await?;
            stats.geocoded += 1;
        }
    }
    Ok(stats)
}

/// (entity id, name, type, confidence, lat, lon, geo confidence, place)
type GeoRow = (String, String, String, f64, f64, f64, f64, Option<String>);

/// GeoJSON FeatureCollection of geocoded live entities.
pub async fn features(pool: &SqlitePool) -> Result<Value> {
    let rows: Vec<GeoRow> = sqlx::query_as(
        "SELECT e.id, e.canonical_name, e.entity_type, e.confidence, g.lat, g.lon, g.geo_confidence, g.place_name \
         FROM wm_geo g JOIN wm_entities e ON e.id = g.entity_id WHERE e.is_live = 1",
    )
    .fetch_all(pool)
    .await?;
    let feats: Vec<Value> = rows
        .into_iter()
        .map(|(id, name, t, conf, lat, lon, gconf, place)| {
            json!({
                "type": "Feature",
                "geometry": {"type": "Point", "coordinates": [lon, lat]},
                "properties": {"id": id, "name": name, "entity_type": t, "confidence": conf, "geo_confidence": gconf, "place": place}
            })
        })
        .collect();
    Ok(json!({"type": "FeatureCollection", "features": feats}))
}

pub async fn status(pool: &SqlitePool) -> Result<(i64, i64, i64)> {
    let (places,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM wm_geo_places")
        .fetch_one(pool)
        .await?;
    let (countries,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM wm_geo_countries")
        .fetch_one(pool)
        .await?;
    let (geocoded,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM wm_geo")
        .fetch_one(pool)
        .await?;
    Ok((places, countries, geocoded))
}

pub fn gazetteer_dir() -> std::path::PathBuf {
    std::env::var("ENARGEIA_GEONAMES_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("data/geonames"))
}

pub fn ensure_err(msg: &str) -> anyhow::Error {
    anyhow!("{msg}")
}
