use dashmap::DashMap;
use deadpool_postgres::{Config as PgConfig, ManagerConfig, Pool, RecyclingMethod, Runtime};
use serde::{Deserialize, Serialize};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio_postgres::NoTls;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Item {
    pub id: u32,
    pub name: String,
    pub category: String,
    pub price: u32,
    pub quantity: u32,
    pub active: bool,
    pub tags: Vec<String>,
    pub rating: Rating,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Rating {
    pub score: u32,
    pub count: u32,
}

/// State each handler reads from `conn.shared_state`.
///
/// All fields are shared across every reuseport worker via `server().with_shared_state` (one
/// init). `dataset` and `crud_cache` are `Arc`-wrapped (cross-worker cache hits satisfy the CRUD
/// spec's "in-process cache" rule). `pg` is one shared pool: connections are created lazily on
/// whichever worker runtime first grows the pool and are safely reused from any worker, because
/// each `current_thread` runtime drives its own reactor so a connection's driver task is woken by
/// socket readiness no matter which worker holds it.
pub struct AppState {
    pub dataset: Arc<Vec<Item>>,
    pub crud_cache: Arc<DashMap<i32, CacheEntry>>,
    pub pg: Option<Pool>,
}

pub struct CacheEntry {
    pub body: Vec<u8>,
    pub expires: Instant,
}

pub const CRUD_CACHE_TTL: Duration = Duration::from_millis(200);

/// Cross-worker pieces. Built once in main, cloned (cheaply, Arc) into each worker's `AppState`.
#[derive(Clone)]
pub struct SharedState {
    pub dataset: Arc<Vec<Item>>,
    pub crud_cache: Arc<DashMap<i32, CacheEntry>>,
}

impl SharedState {
    pub fn init() -> Self {
        let dataset_path =
            std::env::var("DATASET_PATH").unwrap_or_else(|_| "/data/dataset.json".into());
        let dataset: Vec<Item> = std::fs::read(&dataset_path)
            .ok()
            .and_then(|bytes| sonic_rs::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            dataset: Arc::new(dataset),
            crud_cache: Arc::new(DashMap::new()),
        }
    }
}

/// Build the single shared postgres pool. Returns `None` when `DATABASE_URL` is unset.
///
/// One pool is shared across every reuseport worker (init-once via `with_shared_state`).
/// Connections are created lazily on whichever worker runtime first grows the pool; reusing them
/// from other workers is safe because each `current_thread` worker runtime drives its own I/O
/// reactor, so a connection's driver task is woken by socket readiness regardless of which worker
/// checks it out (verified 2026-05-29 — the earlier per-worker design was cautionary).
pub fn build_pg_pool() -> Option<Pool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let mut cfg = PgConfig::new();
    cfg.url = Some(url);
    cfg.manager = Some(ManagerConfig {
        recycling_method: RecyclingMethod::Fast,
    });
    let total: usize = std::env::var("DATABASE_MAX_CONN")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(256);
    cfg.pool = Some(deadpool_postgres::PoolConfig::new(total));
    cfg.create_pool(Some(Runtime::Tokio1), NoTls).ok()
}
