//! `enargeia serve`: the HTTP surface everything else (the globe, the PCG dashboard, agents)
//! talks to. One process, one SQLite file. GET endpoints are open; endpoints that change the
//! graph or spend LLM calls require `Authorization: Bearer <ENARGEIA_TOKEN>` when a token is
//! configured.

use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::Result;
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use tokio::sync::Mutex;
use tower_http::cors::CorsLayer;

use crate::{
    adapters::{celestrak, usgs},
    block, context, geo, llm,
    matcher::Weights,
    models::{WmEdge, WmEntity},
    resolve, why,
};

const UI_HTML: &str = include_str!("../ui/index.html");

struct SatCache {
    group: String,
    fetched_at: std::time::Instant,
    elements: Vec<sgp4::Elements>,
}

struct QuakeCache {
    feed: String,
    fetched_at: std::time::Instant,
    events: Vec<usgs::QuakeEvent>,
}

const QUAKE_CACHE_TTL: Duration = Duration::from_secs(5 * 60);

pub struct AppState {
    pool: SqlitePool,
    token: Option<String>,
    sats: Mutex<Option<SatCache>>,
    quakes: Mutex<Option<QuakeCache>>,
}

type Shared = Arc<AppState>;
type ApiResult = std::result::Result<Json<Value>, (StatusCode, String)>;

fn err(status: StatusCode, e: impl std::fmt::Display) -> (StatusCode, String) {
    (status, e.to_string())
}

fn internal(e: impl std::fmt::Display) -> (StatusCode, String) {
    err(StatusCode::INTERNAL_SERVER_ERROR, e)
}

fn authorize(
    state: &AppState,
    headers: &HeaderMap,
) -> std::result::Result<(), (StatusCode, String)> {
    let Some(expected) = &state.token else {
        return Ok(());
    };
    let got = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if got == expected {
        Ok(())
    } else {
        Err(err(
            StatusCode::UNAUTHORIZED,
            "missing or invalid bearer token",
        ))
    }
}

fn entity_json(e: &WmEntity) -> Value {
    json!({
        "id": e.id, "name": e.canonical_name, "entity_type": e.entity_type, "aliases": e.alias_list(),
        "confidence": e.confidence, "is_live": e.is_live, "first_seen": e.first_seen, "last_seen": e.last_seen,
        "expiry_time": e.expiry_time,
    })
}

fn edge_json(e: &WmEdge, names: &HashMap<String, String>) -> Value {
    json!({
        "id": e.id, "from": e.from_id, "to": e.to_id,
        "from_name": names.get(&e.from_id), "to_name": names.get(&e.to_id),
        "edge_type": e.edge_type, "weight": e.weight, "valid_at": e.valid_at, "invalid_at": e.invalid_at,
        "superseded_by": e.superseded_by, "sources": e.source_ids(),
    })
}

async fn names_for(pool: &SqlitePool, ids: &[String]) -> Result<HashMap<String, String>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!("SELECT id, canonical_name FROM wm_entities WHERE id IN ({placeholders})");
    let mut q = sqlx::query_as::<_, (String, String)>(&sql);
    for id in ids {
        q = q.bind(id);
    }
    Ok(q.fetch_all(pool).await?.into_iter().collect())
}

async fn index() -> Html<&'static str> {
    Html(UI_HTML)
}

async fn health() -> Json<Value> {
    Json(json!({"ok": true, "version": env!("CARGO_PKG_VERSION")}))
}

async fn status(State(state): State<Shared>) -> ApiResult {
    let p = &state.pool;
    let (entities,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM wm_entities")
        .fetch_one(p)
        .await
        .map_err(internal)?;
    let (live,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM wm_entities WHERE is_live = 1")
        .fetch_one(p)
        .await
        .map_err(internal)?;
    let (edges,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM wm_edges WHERE invalid_at IS NULL")
        .fetch_one(p)
        .await
        .map_err(internal)?;
    let (review,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM wm_extraction_candidates WHERE status = 'pending_review'",
    )
    .fetch_one(p)
    .await
    .map_err(internal)?;
    let (items,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM wm_source_items")
        .fetch_one(p)
        .await
        .map_err(internal)?;
    let (geocoded,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM wm_geo")
        .fetch_one(p)
        .await
        .map_err(internal)?;
    let lic: Vec<(String, i64)> =
        sqlx::query_as("SELECT license_class, COUNT(*) FROM wm_source_items GROUP BY 1")
            .fetch_all(p)
            .await
            .map_err(internal)?;
    Ok(Json(json!({
        "entities": entities, "live_entities": live, "valid_edges": edges, "pending_review": review,
        "source_items": items, "geocoded": geocoded,
        "sources_by_license": lic.into_iter().collect::<HashMap<_, _>>(),
    })))
}

async fn entities(
    State(state): State<Shared>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult {
    let p = &state.pool;
    let limit: i64 = q
        .get("limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(50)
        .clamp(1, 500);
    let rows: Vec<WmEntity> = if let Some(text) = q.get("q").filter(|s| !s.trim().is_empty()) {
        let mut cands = block::candidates_for(p, text).await.map_err(internal)?;
        cands.truncate(limit as usize);
        cands
    } else {
        let mut sql = String::from("SELECT * FROM wm_entities WHERE 1=1");
        if q.get("live").map(|v| v != "false").unwrap_or(true) {
            sql.push_str(" AND is_live = 1");
        }
        if let Some(t) = q.get("type") {
            sql.push_str(" AND entity_type = ?");
            sql.push_str(" ORDER BY last_seen DESC LIMIT ?");
            sqlx::query_as::<_, WmEntity>(&sql)
                .bind(t)
                .bind(limit)
                .fetch_all(p)
                .await
                .map_err(internal)?
        } else {
            sql.push_str(" ORDER BY last_seen DESC LIMIT ?");
            sqlx::query_as::<_, WmEntity>(&sql)
                .bind(limit)
                .fetch_all(p)
                .await
                .map_err(internal)?
        }
    };
    Ok(Json(
        json!({"entities": rows.iter().map(entity_json).collect::<Vec<_>>()}),
    ))
}

async fn entity(State(state): State<Shared>, Path(id): Path<String>) -> ApiResult {
    let p = &state.pool;
    let e = why::find_entity(p, &id)
        .await
        .map_err(|e| err(StatusCode::NOT_FOUND, e))?;
    let edges: Vec<WmEdge> = sqlx::query_as("SELECT * FROM wm_edges WHERE (from_id = ? OR to_id = ?) AND invalid_at IS NULL ORDER BY (edge_type = 'mentioned_with'), weight DESC LIMIT 200")
        .bind(&e.id)
        .bind(&e.id)
        .fetch_all(p)
        .await
        .map_err(internal)?;
    let mut ids: Vec<String> = edges
        .iter()
        .flat_map(|x| [x.from_id.clone(), x.to_id.clone()])
        .collect();
    ids.sort();
    ids.dedup();
    let names = names_for(p, &ids).await.map_err(internal)?;
    let geo: Option<(f64, f64, f64, Option<String>)> = sqlx::query_as(
        "SELECT lat, lon, geo_confidence, place_name FROM wm_geo WHERE entity_id = ?",
    )
    .bind(&e.id)
    .fetch_optional(p)
    .await
    .map_err(internal)?;
    let mut v = entity_json(&e);
    v["edges"] = Value::Array(edges.iter().map(|x| edge_json(x, &names)).collect());
    v["geo"] = geo
        .map(
            |(lat, lon, c, place)| json!({"lat": lat, "lon": lon, "confidence": c, "place": place}),
        )
        .unwrap_or(Value::Null);
    Ok(Json(v))
}

async fn entity_why(State(state): State<Shared>, Path(id): Path<String>) -> impl IntoResponse {
    match why::why(&state.pool, &id).await {
        Ok(text) => (StatusCode::OK, text),
        Err(e) => (StatusCode::NOT_FOUND, e.to_string()),
    }
}

async fn edges(State(state): State<Shared>, Query(q): Query<HashMap<String, String>>) -> ApiResult {
    let p = &state.pool;
    let limit: i64 = q
        .get("limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(200)
        .clamp(1, 2000);
    let rows: Vec<WmEdge> = match (q.get("entity"), q.get("as_of")) {
        (Some(id), None) => sqlx::query_as("SELECT * FROM wm_edges WHERE (from_id = ? OR to_id = ?) AND invalid_at IS NULL LIMIT ?")
            .bind(id).bind(id).bind(limit).fetch_all(p).await.map_err(internal)?,
        (Some(id), Some(t)) => sqlx::query_as("SELECT * FROM wm_edges WHERE (from_id = ? OR to_id = ?) AND (valid_at IS NULL OR valid_at <= ?) AND (invalid_at IS NULL OR invalid_at > ?) LIMIT ?")
            .bind(id).bind(id).bind(t).bind(t).bind(limit).fetch_all(p).await.map_err(internal)?,
        (None, _) => sqlx::query_as("SELECT * FROM wm_edges WHERE invalid_at IS NULL AND edge_type <> 'mentioned_with' ORDER BY weight DESC LIMIT ?")
            .bind(limit).fetch_all(p).await.map_err(internal)?,
    };
    let mut ids: Vec<String> = rows
        .iter()
        .flat_map(|x| [x.from_id.clone(), x.to_id.clone()])
        .collect();
    ids.sort();
    ids.dedup();
    let names = names_for(p, &ids).await.map_err(internal)?;
    Ok(Json(
        json!({"edges": rows.iter().map(|x| edge_json(x, &names)).collect::<Vec<_>>()}),
    ))
}

async fn candidates(
    State(state): State<Shared>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult {
    let status = q
        .get("status")
        .cloned()
        .unwrap_or_else(|| "pending_review".to_string());
    let limit: i64 = q
        .get("limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(50)
        .clamp(1, 500);
    type Row = (
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<f64>,
        Option<String>,
        String,
        String,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT c.id, c.mention_text, c.mention_type_guess, e.canonical_name, e.entity_type, c.match_score, c.feature_scores, c.source_item_id, c.created_at \
         FROM wm_extraction_candidates c LEFT JOIN wm_entities e ON e.id = c.best_match_entity_id \
         WHERE c.status = ? ORDER BY c.match_score DESC LIMIT ?",
    )
    .bind(&status)
    .bind(limit)
    .fetch_all(&state.pool)
    .await
    .map_err(internal)?;
    let out: Vec<Value> = rows
        .into_iter()
        .map(|(id, mention, label, cand, ctype, score, features, source, created)| {
            json!({
                "id": id, "mention": mention, "label": label, "candidate": cand, "candidate_type": ctype,
                "score": score, "features": features.and_then(|f| serde_json::from_str::<Value>(&f).ok()),
                "source_item_id": source, "created_at": created,
            })
        })
        .collect();
    Ok(Json(json!({"status": status, "candidates": out})))
}

#[derive(Deserialize)]
struct DecisionBody {
    decision: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    actor: Option<String>,
}

async fn decide(
    State(state): State<Shared>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<DecisionBody>,
) -> ApiResult {
    authorize(&state, &headers)?;
    let msg = resolve::apply_decision(
        &state.pool,
        &id,
        &body.decision,
        body.actor.as_deref().unwrap_or("api"),
        body.reason.as_deref(),
    )
    .await
    .map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    Ok(Json(json!({"ok": true, "result": msg})))
}

#[derive(Deserialize)]
struct DecorrelateBody {
    a: String,
    b: String,
    #[serde(default)]
    reason: Option<String>,
}

async fn decorrelate(
    State(state): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<DecorrelateBody>,
) -> ApiResult {
    authorize(&state, &headers)?;
    resolve::decorrelate(&state.pool, &body.a, &body.b, body.reason.as_deref())
        .await
        .map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct AskBody {
    question: String,
    #[serde(default)]
    as_of: Option<String>,
}

async fn ask(
    State(state): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<AskBody>,
) -> ApiResult {
    authorize(&state, &headers)?;
    let client = llm::LlmClient::from_env().map_err(internal)?;
    let answer = context::ask(&state.pool, &client, &body.question, body.as_of.as_deref())
        .await
        .map_err(internal)?;
    Ok(Json(
        json!({"question": body.question, "as_of": body.as_of, "answer": answer, "llm": client.describe()}),
    ))
}

async fn geo_features(State(state): State<Shared>) -> ApiResult {
    Ok(Json(geo::features(&state.pool).await.map_err(internal)?))
}

async fn satellites(
    State(state): State<Shared>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult {
    let group = q
        .get("group")
        .cloned()
        .unwrap_or_else(|| celestrak::DEFAULT_GROUP.to_string());
    celestrak::check_group(&group).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    let limit: usize = q
        .get("limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(3000)
        .clamp(1, 20000);

    let mut cache = state.sats.lock().await;
    let stale = match cache.as_ref() {
        Some(c) => c.group != group || c.fetched_at.elapsed() > celestrak::CACHE_TTL,
        None => true,
    };
    if stale {
        let elements = celestrak::fetch_group(&group)
            .await
            .map_err(|e| err(StatusCode::BAD_GATEWAY, e))?;
        *cache = Some(SatCache {
            group: group.clone(),
            fetched_at: std::time::Instant::now(),
            elements,
        });
    }
    let elements = &cache.as_ref().unwrap().elements;
    let at = chrono::Utc::now();
    let mut sats = celestrak::positions(elements, at);
    sats.truncate(limit);
    Ok(Json(celestrak::to_geojson(&sats, &group, at)))
}

async fn weights() -> Json<Value> {
    Json(serde_json::to_value(Weights::from_env()).unwrap_or(Value::Null))
}

async fn quakes(
    State(state): State<Shared>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult {
    let feed = q
        .get("feed")
        .cloned()
        .unwrap_or_else(|| usgs::DEFAULT_FEED.to_string());
    usgs::check_feed(&feed).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    let mut cache = state.quakes.lock().await;
    let stale = match cache.as_ref() {
        Some(c) => c.feed != feed || c.fetched_at.elapsed() > QUAKE_CACHE_TTL,
        None => true,
    };
    if stale {
        let events = usgs::fetch(&feed)
            .await
            .map_err(|e| err(StatusCode::BAD_GATEWAY, e))?;
        *cache = Some(QuakeCache {
            feed: feed.clone(),
            fetched_at: std::time::Instant::now(),
            events,
        });
    }
    let c = cache.as_ref().unwrap();
    Ok(Json(usgs::to_geojson(&c.events, &feed, chrono::Utc::now())))
}

pub fn router(state: Shared) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/health", get(health))
        .route("/api/status", get(status))
        .route("/api/weights", get(weights))
        .route("/api/entities", get(entities))
        .route("/api/entities/{id}", get(entity))
        .route("/api/entities/{id}/why", get(entity_why))
        .route("/api/edges", get(edges))
        .route("/api/candidates", get(candidates))
        .route("/api/candidates/{id}/decision", post(decide))
        .route("/api/decorrelate", post(decorrelate))
        .route("/api/ask", post(ask))
        .route("/api/geo", get(geo_features))
        .route("/api/live/satellites", get(satellites))
        .route("/api/live/quakes", get(quakes))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

pub async fn serve(pool: SqlitePool, bind: &str, token: Option<String>) -> Result<()> {
    let state = Arc::new(AppState {
        pool,
        token,
        sats: Mutex::new(None),
        quakes: Mutex::new(None),
    });
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    println!("enargeia serving on http://{bind}  (globe at /, API under /api)");
    axum::serve(listener, app).await?;
    Ok(())
}
