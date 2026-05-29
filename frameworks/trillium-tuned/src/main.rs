#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod handlers;
mod state;
mod static_preload;

use crate::{
    handlers::{
        async_db, baseline_any, baseline_get, crud_create, crud_list, crud_read, crud_update,
        fortunes, json_handler, pipeline, upload, ws_echo,
    },
    state::{AppState, SharedState, build_pg_pool},
    static_preload::StaticPreload,
};
use std::sync::Arc;
use trillium::Handler;
use trillium_compression::Compression;
use trillium_quinn::QuicConfig;
use trillium_router::Router;
use trillium_rustls::RustlsAcceptor;
use trillium_tokio::server;
use trillium_websockets::websocket;

fn tuned_http_config() -> trillium::HttpConfig {
    trillium::HttpConfig::default()
        .with_response_buffer_len(8192)
        .with_received_body_max_len(32 * 1024 * 1024)
        .with_received_body_initial_len(64 * 1024)
        .with_received_body_max_preallocate(32 * 1024 * 1024)
        .with_copy_loops_per_yield(64)
        .with_h2_max_frame_size(65536)
        .with_request_buffer_initial_len(256)
}

fn build_handler(static_files: StaticPreload) -> impl Handler {
    (
        Compression::new(),
        Router::new()
            .get("/pipeline", pipeline)
            .any(&["get", "post"], "/baseline11", baseline_any)
            .get("/baseline2", baseline_get)
            .get("/json/:count", json_handler)
            .post("/upload", upload)
            .get("/static/*", static_files)
            .get("/async-db", async_db)
            .get("/fortunes", fortunes)
            .get("/crud/items", crud_list)
            .post("/crud/items", crud_create)
            .get("/crud/items/:id", crud_read)
            .put("/crud/items/:id", crud_update)
            .get("/ws", websocket(ws_echo)),
    )
}

fn main() {
    env_logger::init_from_env(env_logger::Env::default().default_filter_or("info"));

    let shared = SharedState::init();

    let static_dir = std::env::var("STATIC_DIR").unwrap_or_else(|_| "/data/static".into());
    let static_files = StaticPreload::load(&static_dir);

    let cert =
        std::fs::read(std::env::var("TLS_CERT").unwrap_or_else(|_| "/certs/server.crt".into()))
            .ok();
    let key =
        std::fs::read(std::env::var("TLS_KEY").unwrap_or_else(|_| "/certs/server.key".into())).ok();

    let tls_port: u16 = std::env::var("TLS_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8443);

    let n_workers: usize = std::env::var("WORKERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(num_cpus::get)
        .max(1);

    // ONE shared pool, dropped into shared state (init-once). The reuseport workers all draw from
    // it; the per-worker pool the hand-rolled topology used is unnecessary — a connection's I/O
    // driver task is woken by its socket's reactor on whichever worker runtime created it,
    // regardless of which worker checks the connection out (verified 2026-05-29). The shared pool
    // is also slightly faster (a busy worker can borrow idle capacity).
    let state = Arc::new(AppState {
        dataset: shared.dataset.clone(),
        crud_cache: shared.crud_cache.clone(),
        pg: build_pg_pool(),
    });

    // The whole topology in one builder: 8080 plaintext + 8081 TLS(no-h2) + 8443 TLS(h2) all fanned
    // across per-core worker threads via SO_REUSEPORT, plus a single QUIC/h3 endpoint on 8443 bound
    // once on the shared multi-threaded runtime (which also owns init, signals, and app spawns).
    // alt-svc h3=":8443" auto-pairs from the same-port 8443 TCP+QUIC binds.
    let mut builder = server()
        .with_nodelay()
        .with_http_config(tuned_http_config())
        .with_shared_state(state)
        .with_reuseport_workers(n_workers)
        .bind_reuseport_tcp(8080)
        .expect("bind 8080");

    if let (Some(cert), Some(key)) = (cert.as_deref(), key.as_deref()) {
        builder = builder
            .bind_reuseport_tls(8081, RustlsAcceptor::from_single_cert_no_h2(cert, key))
            .expect("bind 8081")
            .bind_reuseport_tls(tls_port, RustlsAcceptor::from_single_cert(cert, key))
            .expect("bind TLS port")
            .bind_quic(tls_port, QuicConfig::from_single_cert(cert, key))
            .expect("bind QUIC");
    } else {
        log::warn!("TLS cert/key not found; only port 8080 is listening");
    }

    log::info!(
        "starting trillium-tuned via server(): {n_workers} reuseport worker(s) + shared runtime for h3"
    );

    builder.run(build_handler(static_files));
}
