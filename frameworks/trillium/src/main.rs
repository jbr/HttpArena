#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod handlers;
mod state;

use crate::{
    handlers::{
        async_db, baseline_any, baseline_get, crud_create, crud_list, crud_read, crud_update,
        fortunes, json_handler, pipeline, upload, ws_echo,
    },
    state::AppState,
};
use trillium::Handler;
use trillium_compression::Compression;
use trillium_quinn::QuicConfig;
use trillium_router::Router;
use trillium_rustls::RustlsAcceptor;
use trillium_static::StaticFileHandler;
use trillium_tokio::server;
use trillium_websockets::websocket;

fn build_handler() -> impl Handler {
    let static_dir = std::env::var("STATIC_DIR").unwrap_or_else(|_| "/data/static".into());
    (
        Compression::new(),
        Router::new()
            .get("/pipeline", pipeline)
            .any(&["get", "post"], "/baseline11", baseline_any)
            .get("/baseline2", baseline_get)
            .get("/json/:count", json_handler)
            .post("/upload", upload)
            .get(
                "/static/*",
                StaticFileHandler::new(static_dir).with_precompressed(),
            )
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

    let state = AppState::init();

    let cert =
        std::fs::read(std::env::var("TLS_CERT").unwrap_or_else(|_| "/certs/server.crt".into()))
            .ok();
    let key =
        std::fs::read(std::env::var("TLS_KEY").unwrap_or_else(|_| "/certs/server.key".into())).ok();

    let tls_port: u16 = std::env::var("TLS_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8443);

    let http_config = trillium::HttpConfig::default().with_received_body_max_len(32 * 1024 * 1024);

    // Multi-listener (no reuseport): one shared work-stealing runtime serves every listener with a
    // single initialized handler and shared state.
    //   8080: h1 cleartext (also /ws and h2c-prior-knowledge)
    //   8081: h1-only over TLS (ALPN http/1.1) — for json-tls
    //   8443: h1 + h2 over TLS, plus h3 over QUIC (alt-svc h3=":8443" auto-pairs)
    let mut builder = server()
        .with_nodelay()
        .with_http_config(http_config)
        .with_shared_state(state)
        .bind_tcp(8080)
        .expect("bind 8080");

    if let (Some(cert), Some(key)) = (cert.as_deref(), key.as_deref()) {
        builder = builder
            .bind_tls(8081, RustlsAcceptor::from_single_cert_no_h2(cert, key))
            .expect("bind 8081")
            .bind_tls(tls_port, RustlsAcceptor::from_single_cert(cert, key))
            .expect("bind TLS port")
            .bind_quic(tls_port, QuicConfig::from_single_cert(cert, key))
            .expect("bind QUIC");
    } else {
        log::warn!("TLS cert/key not found; only port 8080 is listening");
    }

    builder.run(build_handler());
}
