pub mod adapters;
pub mod block;
pub mod context;
pub mod db;
pub mod eval;
pub mod extract;
pub mod llm;
pub mod llm_enrich;
pub mod matcher;
pub mod models;
pub mod resolve;
pub mod why;

pub const USER_AGENT: &str = concat!(
    "enargeia/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/KingBodhi/enargeia)"
);
