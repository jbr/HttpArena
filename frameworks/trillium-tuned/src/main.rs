#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod handlers;
mod runtime;
mod state;
mod static_preload;

use crate::{
    handlers::{
        async_db, baseline_any, baseline_get, crud_create, crud_list, crud_read, crud_update,
        fortunes, json_handler, pipeline, upload, ws_echo,
    },
    runtime::bind_reuseport,
    state::{AppState, SharedState, build_pg_pool},
    static_preload::StaticPreload,
};
use std::sync::Arc;
use trillium::Handler;
use trillium_compression::Compression;
use trillium_quinn::QuicConfig;
use trillium_router::Router;
use trillium_rustls::RustlsAcceptor;
use trillium_tokio::tokio;
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

struct WorkerInputs {
    shared: SharedState,
    static_files: StaticPreload,
    cert: Option<Vec<u8>>,
    key: Option<Vec<u8>>,
    swansong: swansong::Swansong,
    tls_port: u16,
    workers: usize,
}

/// Per-worker current_thread runtime: TCP-only (h1, h2, ws). No QUIC.
/// The QUIC endpoint lives on the dedicated multi-thread runtime spawned in main.
fn run_worker(idx: usize, inputs: WorkerInputs) {
    let WorkerInputs {
        shared,
        static_files,
        cert,
        key,
        swansong,
        tls_port,
        workers,
    } = inputs;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build current_thread runtime");

    rt.block_on(async move {
        let state = Arc::new(AppState {
            dataset: shared.dataset.clone(),
            crud_cache: shared.crud_cache.clone(),
            pg: build_pg_pool(workers),
        });

        let l8080 = bind_reuseport(8080).expect("bind 8080");
        log::info!("worker {idx}: bound 8080");

        trillium_tokio::config()
            .with_prebound_server(l8080)
            .with_swansong(swansong.clone())
            .without_signals()
            .with_nodelay()
            .with_http_config(tuned_http_config())
            .with_shared_state(state.clone())
            .spawn(build_handler(static_files.clone()));

        if let (Some(cert), Some(key)) = (cert.as_deref(), key.as_deref()) {
            let l8081 = bind_reuseport(8081).expect("bind 8081");
            trillium_tokio::config()
                .with_prebound_server(l8081)
                .with_swansong(swansong.clone())
                .without_signals()
                .with_nodelay()
                .with_http_config(tuned_http_config())
                .with_shared_state(state.clone())
                .with_acceptor(RustlsAcceptor::from_single_cert_no_h2(cert, key))
                .spawn(build_handler(static_files.clone()));

            let l_tls = bind_reuseport(tls_port).expect("bind TLS port");
            trillium_tokio::config()
                .with_prebound_server(l_tls)
                .with_swansong(swansong.clone())
                .without_signals()
                .with_nodelay()
                .with_http_config(tuned_http_config())
                .with_shared_state(state.clone())
                .with_acceptor(RustlsAcceptor::from_single_cert(cert, key))
                .spawn(build_handler(static_files.clone()));
        } else if idx == 0 {
            log::warn!("TLS cert/key not found; only port 8080 is listening");
        }

        swansong.await;
    });
}

struct QuicRuntimeInputs {
    shared: SharedState,
    static_files: StaticPreload,
    cert: Vec<u8>,
    key: Vec<u8>,
    swansong: swansong::Swansong,
    tls_port: u16,
    n_threads: usize,
    workers: usize,
}

/// Dedicated multi-thread runtime that owns the QUIC endpoint and joins the TCP reuseport
/// pool on all three ports as one additional listener. h3 stream tasks spawned by quinn's
/// accept loop spread across all `n_threads` threads via tokio's work-stealing scheduler;
/// TCP traffic on this runtime is the kernel's reuseport share (1 of N+1 sockets per port),
/// so the per-worker current_thread runtimes still absorb the bulk of TCP work and keep
/// their per-core hot-cache benefit.
fn run_quic_runtime(inputs: QuicRuntimeInputs) {
    let QuicRuntimeInputs {
        shared,
        static_files,
        cert,
        key,
        swansong,
        tls_port,
        n_threads,
        workers,
    } = inputs;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(n_threads)
        .enable_all()
        .thread_name("quic-mt")
        .build()
        .expect("build quic multi_thread runtime");

    rt.block_on(async move {
        let state = Arc::new(AppState {
            dataset: shared.dataset.clone(),
            crud_cache: shared.crud_cache.clone(),
            pg: build_pg_pool(workers),
        });

        let l8080 = bind_reuseport(8080).expect("bind 8080 on quic runtime");
        trillium_tokio::config()
            .with_prebound_server(l8080)
            .with_swansong(swansong.clone())
            .without_signals()
            .with_nodelay()
            .with_http_config(tuned_http_config())
            .with_shared_state(state.clone())
            .spawn(build_handler(static_files.clone()));

        let l8081 = bind_reuseport(8081).expect("bind 8081 on quic runtime");
        trillium_tokio::config()
            .with_prebound_server(l8081)
            .with_swansong(swansong.clone())
            .without_signals()
            .with_nodelay()
            .with_http_config(tuned_http_config())
            .with_shared_state(state.clone())
            .with_acceptor(RustlsAcceptor::from_single_cert_no_h2(&cert, &key))
            .spawn(build_handler(static_files.clone()));

        let l_tls = bind_reuseport(tls_port).expect("bind TLS port on quic runtime");
        trillium_tokio::config()
            .with_prebound_server(l_tls)
            .with_swansong(swansong.clone())
            .without_signals()
            .with_nodelay()
            .with_http_config(tuned_http_config())
            .with_shared_state(state.clone())
            .with_acceptor(RustlsAcceptor::from_single_cert(&cert, &key))
            .with_quic(QuicConfig::from_single_cert(&cert, &key))
            .spawn(build_handler(static_files.clone()));

        log::info!("quic-mt runtime: TCP reuseport 8080/8081/{tls_port} + QUIC on {tls_port} ({n_threads} threads)");

        swansong.await;
    });
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

    // Default the QUIC runtime size proportionally to N, capped at 8 (Zen2/3/4 CCX size = 4
    // physical cores / 8 SMT threads — keeping the MT runtime ≤1 CCX worth keeps h3 work
    // L3-local and avoids paying the ~70-cycle inter-CCX hop on every steal). Override with
    // QUIC_THREADS for tuning.
    //
    // Empirical 8-core measurements: Q=2 preserves full per-worker TCP performance (≤3% delta)
    // while doubling h3 capacity over the previous worker-0-only design. Q=8 maximizes h3 (~4
    // cores' worth) at a 12-17% TCP cost. The proportional default lands users near the Q=2
    // point on small boxes and the Q=8 point on the bench machine (32 phys / 64 SMT cores).
    let quic_threads: usize = std::env::var("QUIC_THREADS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| (n_workers / 4).max(2).min(8))
        .max(1);

    let swansong = swansong::Swansong::new();

    {
        let swansong = swansong.clone();
        std::thread::Builder::new()
            .name("signals".into())
            .spawn(move || {
                let mut signals = signal_hook::iterator::Signals::new([
                    signal_hook::consts::SIGINT,
                    signal_hook::consts::SIGTERM,
                ])
                .expect("install signal handler");
                if signals.forever().next().is_some() {
                    log::info!("shutdown signal received");
                    swansong.shut_down();
                }
            })
            .expect("spawn signal thread");
    }

    log::info!(
        "starting {n_workers} per-worker current_thread workers (TCP) + 1 quic-mt runtime ({quic_threads} threads)"
    );

    let mut handles = Vec::with_capacity(n_workers + 1);

    for idx in 0..n_workers {
        let inputs = WorkerInputs {
            shared: shared.clone(),
            static_files: static_files.clone(),
            cert: cert.clone(),
            key: key.clone(),
            swansong: swansong.clone(),
            tls_port,
            workers: n_workers,
        };
        handles.push(
            std::thread::Builder::new()
                .name(format!("worker-{idx}"))
                .spawn(move || run_worker(idx, inputs))
                .expect("spawn worker thread"),
        );
    }

    if let (Some(cert), Some(key)) = (cert.clone(), key.clone()) {
        let inputs = QuicRuntimeInputs {
            shared: shared.clone(),
            static_files: static_files.clone(),
            cert,
            key,
            swansong: swansong.clone(),
            tls_port,
            n_threads: quic_threads,
            workers: n_workers,
        };
        handles.push(
            std::thread::Builder::new()
                .name("quic-mt-driver".into())
                .spawn(move || run_quic_runtime(inputs))
                .expect("spawn quic-mt driver thread"),
        );
    }

    for h in handles {
        h.join().expect("worker join");
    }
}
