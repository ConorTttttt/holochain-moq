//! `holochain-moq-app-web`: the local browser control surface (Stage 14H.1) over the
//! existing `holochain_moq_app::App` application layer.
//!
//! Startup mirrors `crates/app`'s own CLI shell exactly (connect to the conductor, load
//! identity, enter `Ready`) and then, additionally, starts a loopback-only HTTP server
//! serving both the API and the static frontend from the same origin. See
//! `crates/app-web/README.md` for the API, the security model, and what is deliberately
//! not implemented yet (browser video, WebSocket/SSE, authentication beyond loopback).

mod config;
mod diag;
mod guard;
mod ownership;
mod routes;
#[cfg(test)]
mod tests;
mod video_ws;
mod view;
mod wire;

use anyhow::{Context, Result};
use clap::Parser;
use holochain_moq_app::App;
use holochain_moq_hdk_bridge::{ConductorConfig, HolochainPeerDirectory};
use holochain_moq_node::RealtimeNode;
use holochain_moq_realtime::AdmissionSigner;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

/// Chosen not to collide with `screenshare`/`crates/app`'s own conventions
/// (`4443` is the default MoQ/QUIC bind, `4444` the default Holochain admin port).
const DEFAULT_PORT: u16 = 4480;

/// Longest a shutdown waits for a live share/watch to end before exiting anyway.
const SHUTDOWN_LIMIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Resolves on SIGINT (Ctrl-C) or, on Unix, SIGTERM.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
}

#[derive(Parser)]
struct Args {
    /// The conductor's admin websocket - same meaning and default as `holochain-moq-app`.
    #[arg(long, default_value = "127.0.0.1:4444")]
    admin: SocketAddr,
    /// The installed app id of the moq_directory hApp.
    #[arg(long, default_value = "moq_directory")]
    app_id: String,
    /// Address the local HTTP control surface listens on. Loopback by default; binding
    /// anywhere else exposes an *unauthenticated* control interface (it can start/stop
    /// sharing and choose who may watch a private session) - see the README before
    /// changing this.
    #[arg(long, default_value = "127.0.0.1")]
    bind: IpAddr,
    /// Port for the local HTTP control surface.
    #[arg(long, default_value_t = DEFAULT_PORT)]
    port: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,quinn=warn")),
        )
        .init();
    let args = Args::parse();

    // Startup: identical to `crates/app`'s own CLI - connect to the already-running
    // conductor, construct the existing Holochain-backed directory, load identity, reach
    // `Ready`. Identity provisioning and conductor/sandbox bootstrapping stay outside
    // this binary, exactly as they do for `holochain-moq-app`.
    let directory = HolochainPeerDirectory::connect(ConductorConfig::new(args.admin, args.app_id))
        .await
        .context("connecting to the Holochain conductor")?;
    // Stage 15.2: how a browser Watch proves *this process's own* Holochain identity to a
    // private session (Stage 14F). The proof is signed through this app's own conductor
    // connection (`HolochainPeerDirectory::sign_admission_proof` - a zome call, since the
    // real agent key never leaves lair), exactly as `screenshare`'s CLI `watch` builds it.
    // Nothing the browser sends can choose the identity: there is no agent parameter on any
    // route, and the signer can only ever sign as this conductor's agent.
    let sign: AdmissionSigner = {
        let directory = directory.clone();
        Arc::new(move |session_id, challenge| {
            let directory = directory.clone();
            Box::pin(async move {
                directory
                    .sign_admission_proof(session_id, challenge)
                    .await
                    .map_err(anyhow::Error::new)
            })
        })
    };
    let mut app = App::starting(RealtimeNode::new(directory)).with_admission_signer(sign);
    app.connect().await.context("loading the local identity")?;
    tracing::info!(agent = %app.agent().await?, "connected and ready");

    let handle = routes::AppHandle::new(app);
    // A Watch whose page went away is left, not kept alive forever (Stage 15.3).
    routes::spawn_abandoned_watch_watchdog(
        handle.clone(),
        std::time::Duration::from_secs(30),
        std::time::Duration::from_secs(5),
    );
    let handle_for_shutdown = handle.clone();
    let router = routes::router_with_policy(handle, guard::HostPolicy::for_bind(args.bind));

    let addr = SocketAddr::new(args.bind, args.port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding the local HTTP control surface on {addr}"))?;
    tracing::info!("listening on http://{addr}");
    if !addr.ip().is_loopback() {
        tracing::warn!(
            "bound beyond loopback ({addr}): this control interface is NOT authenticated - \
             anyone who can reach it can start/stop sharing and choose who may watch a \
             private session. See crates/app-web/README.md before deploying this way."
        );
    }
    tokio::select! {
        served = axum::serve(listener, router) => {
            served.context("running the HTTP server")?;
        }
        _ = shutdown_signal() => {
            tracing::info!("shutdown requested");
            let clean = routes::shutdown_app(&handle_for_shutdown, SHUTDOWN_LIMIT).await;
            if !clean {
                tracing::warn!("shutdown: gave up waiting after {SHUTDOWN_LIMIT:?}");
            }
            // Let an open video socket observe the end and close itself (it polls every
            // 20ms) before the runtime drops it.
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
    }
    Ok(())
}
