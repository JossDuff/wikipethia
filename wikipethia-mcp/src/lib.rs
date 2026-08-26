//! MCP server: the corpus exposed to LLM clients over stdio (default) or
//! streamable HTTP (`--http <addr>`). Mounted as the `wikipethia mcp`
//! subcommand — this crate is a library and builds no binary of its own.
//!
//! In stdio mode stdout carries the protocol — every diagnostic in this
//! crate must go to stderr, in both modes, for consistency. There is no
//! `println!` anywhere in this crate; keep it that way.

pub mod tools;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use rmcp::transport::streamable_http_server::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::StreamableHttpServerConfig;
use tokio_util::sync::CancellationToken;
use wikipethia_core::Store;
use wikipethia_embed::FastEmbedder;

use tools::CorpusServer;

/// Serve the corpus over MCP until the client disconnects (stdio) or a
/// shutdown signal arrives (HTTP). Synchronous on purpose: the CLI binary
/// is synchronous, and the tokio runtime lives entirely inside this call
/// so the crawl and index paths never run under one by accident.
pub fn run(db: PathBuf, http: Option<SocketAddr>, allow_hosts: Vec<String>) -> anyhow::Result<()> {
    if http.is_none() && !allow_hosts.is_empty() {
        anyhow::bail!("--allow-host only applies to --http mode");
    }
    for host in &allow_hosts {
        // rmcp matches port-less entries against any port; a host:port
        // value would build an entry that matches nothing and every
        // client would 403 with no output.
        if host.contains(':') || host.contains('/') {
            anyhow::bail!(
                "--allow-host takes a bare hostname (got {host:?}) — \
                 no port, no scheme; it matches any port"
            );
        }
    }
    // The standalone binary read CORPUS_DB as a pre-rename fallback; the
    // subcommand does not (no other subcommand ever did), but a set-and-
    // ignored variable should not fail silently.
    if std::env::var_os("CORPUS_DB").is_some() && std::env::var_os("WIKIPETHIA_DB").is_none() {
        eprintln!("wikipethia mcp: CORPUS_DB is no longer read — set WIKIPETHIA_DB or pass --db");
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?
        .block_on(serve(db, http, allow_hosts))
}

async fn serve(
    db: PathBuf,
    http: Option<SocketAddr>,
    allow_hosts: Vec<String>,
) -> anyhow::Result<()> {
    let store =
        Store::open_existing(&db).with_context(|| format!("opening {}", db.display()))?;
    if store.count()? == 0 {
        // Failing the connection beats serving an empty corpus silently —
        // this line shows up in the client's MCP logs.
        anyhow::bail!(
            "{} holds no documents — run `wikipethia build` first",
            db.display()
        );
    }
    // A db migrated from pre-M6 has documents but no manifest tiers until
    // an index run records them — surface that instead of silently serving
    // tierless citations (the retrieval invariant promises tier).
    for stats in store.source_stats()? {
        if stats.tier.is_none() {
            eprintln!(
                "wikipethia mcp: source {:?} has no tier recorded — run \
                 `wikipethia index` to record manifest tiers",
                stats.id
            );
        }
    }
    let embedder = if store.embedding_count()? > 0 {
        // Built once; model load is slow and FastEmbedder is shared safely.
        Some(FastEmbedder::new()?)
    } else {
        eprintln!(
            "wikipethia mcp: {} has no embeddings — serving lexical-only ranking \
             (run `wikipethia embed`)",
            db.display()
        );
        None
    };
    let server = CorpusServer::new(store, embedder)?;

    match http {
        None => {
            let service = server.serve(stdio()).await?;
            service.waiting().await?;
        }
        Some(bind) => serve_http(server, bind, allow_hosts).await?,
    }
    Ok(())
}

/// Streamable-HTTP mode: one long-running daemon, sessions at `/mcp`. The
/// handler factory clones the shared server per session (per REQUEST for
/// new-protocol clients), which is why CorpusServer::clone must stay cheap.
async fn serve_http(
    server: CorpusServer,
    bind: SocketAddr,
    extra_hosts: Vec<String>,
) -> anyhow::Result<()> {
    // rmcp's default host allowlist is loopback-only (DNS-rebind
    // protection). A non-loopback bind is unreachable without its own
    // name in the list, so allow the bind address and any --allow-host
    // names (e.g. a Tailscale hostname) on top of the loopback defaults.
    // Port-less entries match any port in rmcp's matcher, so bare names
    // are all that is needed (`run` rejects host:port values).
    let mut hosts = vec!["localhost".to_string(), "127.0.0.1".to_string(), "::1".to_string()];
    hosts.push(bind.ip().to_string());
    hosts.extend(extra_hosts);

    let ct = CancellationToken::new();
    let service: StreamableHttpService<CorpusServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(server.clone()),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default()
                .with_cancellation_token(ct.child_token())
                .with_allowed_hosts(hosts),
        );
    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    eprintln!(
        "wikipethia mcp: serving streamable HTTP on http://{bind}/mcp — no \
         authentication; bind only to loopback or a private (Tailscale/\
         WireGuard) interface, never a public one"
    );

    // One token drives both halves of shutdown: active MCP sessions
    // terminate and axum stops accepting, then drains.
    let shutdown = ct.clone();
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        eprintln!("wikipethia mcp: shutting down");
        shutdown.cancel();
    });
    axum::serve(listener, router)
        .with_graceful_shutdown({
            let ct = ct.clone();
            async move { ct.cancelled_owned().await }
        })
        .await?;
    Ok(())
}

/// SIGINT or (on unix) SIGTERM — systemd's default stop signal, so
/// `systemctl stop` gets the same graceful drain as ctrl-c. A failed
/// handler registration parks forever rather than resolving: resolving
/// would shut a freshly started daemon down cleanly (exit 0), invisible
/// to Restart=on-failure.
async fn wait_for_shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            eprintln!("wikipethia mcp: cannot listen for ctrl-c: {e}");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                term.recv().await;
            }
            Err(e) => {
                eprintln!("wikipethia mcp: cannot listen for SIGTERM: {e}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}
