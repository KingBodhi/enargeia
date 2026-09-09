//! Blocking: candidate generation for the matcher. Comparing every mention against every
//! entity is O(N) per mention and does not survive a few thousand entities. Instead each
//! entity's canonical name and aliases are indexed under a few cheap keys (normalized name,
//! content tokens, phonetic codes) and a mention only meets the entities that share a key.

use anyhow::Result;
use sqlx::SqlitePool;

use crate::{
    matcher::{content_tokens, normalize, phonetic},
    models::WmEntity,
};

pub const MAX_CANDIDATES: usize = 50;
const MIN_TOKEN_LEN: usize = 3;

/// (key_type, key) pairs for one name.
pub fn keys_for(name: &str) -> Vec<(&'static str, String)> {
    let norm = normalize(name);
    if norm.is_empty() {
        return Vec::new();
    }
    let mut keys: Vec<(&'static str, String)> = vec![("norm", norm.clone())];
    for t in content_tokens(&norm) {
        if t.len() >= MIN_TOKEN_LEN {
            keys.push(("token", t.clone()));
            let p = phonetic(&t);
            if !p.is_empty() {
                keys.push(("phon", p));
            }
        }
    }
    keys.sort();
    keys.dedup();
    keys
}

/// Rebuilds the key set for one entity from its canonical name and aliases.
pub async fn index_entity(
    pool: &SqlitePool,
    entity_id: &str,
    canonical: &str,
    aliases: &[String],
) -> Result<()> {
    let mut conn = pool.acquire().await?;
    index_entity_on(&mut conn, entity_id, canonical, aliases).await
}

async fn index_entity_on(
    conn: &mut sqlx::SqliteConnection,
    entity_id: &str,
    canonical: &str,
    aliases: &[String],
) -> Result<()> {
    sqlx::query("DELETE FROM wm_entity_keys WHERE entity_id = ?")
        .bind(entity_id)
        .execute(&mut *conn)
        .await?;
    let mut all: Vec<(&'static str, String)> = keys_for(canonical);
    for a in aliases {
        all.extend(keys_for(a));
    }
    all.sort();
    all.dedup();
    for (kt, k) in all {
        sqlx::query(
            "INSERT OR IGNORE INTO wm_entity_keys (entity_id, key_type, key) VALUES (?, ?, ?)",
        )
        .bind(entity_id)
        .bind(kt)
        .bind(&k)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Entities sharing at least one key with the mention, most shared keys first.
pub async fn candidates_for(pool: &SqlitePool, mention: &str) -> Result<Vec<WmEntity>> {
    let keys = keys_for(mention);
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let clauses = keys
        .iter()
        .map(|_| "(key_type = ? AND key = ?)")
        .collect::<Vec<_>>()
        .join(" OR ");
    let sql = format!(
        "SELECT entity_id, COUNT(*) AS hits FROM wm_entity_keys WHERE {clauses} \
         GROUP BY entity_id ORDER BY hits DESC LIMIT {MAX_CANDIDATES}"
    );
    let mut q = sqlx::query_as::<_, (String, i64)>(&sql);
    for (kt, k) in &keys {
        q = q.bind(*kt).bind(k);
    }
    let ids: Vec<String> = q
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!("SELECT * FROM wm_entities WHERE id IN ({placeholders})");
    let mut q = sqlx::query_as::<_, WmEntity>(&sql);
    for id in &ids {
        q = q.bind(id);
    }
    let mut ents = q.fetch_all(pool).await?;
    // Preserve the hits ordering from the key query.
    ents.sort_by_key(|e| ids.iter().position(|i| *i == e.id).unwrap_or(usize::MAX));
    Ok(ents)
}

pub async fn reindex_all(pool: &SqlitePool) -> Result<usize> {
    let ents: Vec<WmEntity> = sqlx::query_as("SELECT * FROM wm_entities")
        .fetch_all(pool)
        .await?;
    let n = ents.len();
    // One transaction: per-row autocommit means one fsync per key and minutes for a few
    // thousand entities.
    let mut tx = pool.begin().await?;
    for e in ents {
        index_entity_on(&mut tx, &e.id, &e.canonical_name, &e.alias_list()).await?;
    }
    tx.commit().await?;
    Ok(n)
}

/// Builds the index if it is empty while entities exist (first run after the migration).
pub async fn ensure_index(pool: &SqlitePool) -> Result<()> {
    let (keys,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM wm_entity_keys")
        .fetch_one(pool)
        .await?;
    let (ents,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM wm_entities")
        .fetch_one(pool)
        .await?;
    if keys == 0 && ents > 0 {
        let n = reindex_all(pool).await?;
        tracing::info!(entities = n, "built blocking index");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_cover_norm_tokens_and_phonetics() {
        let keys = keys_for("Meta Platforms, Inc.");
        assert!(keys.contains(&("norm", "meta platforms inc".to_string())));
        assert!(keys.contains(&("token", "meta".to_string())));
        assert!(keys.contains(&("token", "platforms".to_string())));
        // "inc" is ignorable: no token key for it
        assert!(!keys.contains(&("token", "inc".to_string())));
        assert!(keys.iter().any(|(t, _)| *t == "phon"));
    }
}
