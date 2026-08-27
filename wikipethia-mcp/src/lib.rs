//! MCP server: the corpus exposed to LLM clients over stdio (default) or
//! streamable HTTP (`--http <addr>`). Mounted as the `wikipethia mcp`
//! subcommand — this crate is a library and builds no binary of its own.
//!
//! In stdio mode stdout carries the protocol — every diagnostic in this
//! crate must go to stderr, in both modes, for consistency. Enforced, not
//! just stated: clippy's `print_stdout` lint is denied crate-wide
//! (`[lints]` in Cargo.toml), so a stray `println!` fails CI.

mod tools;

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
///
/// Flag validation (--allow-host needs --http, bare hostnames only) is
/// clap's job in the CLI — this function trusts its arguments, except the
/// bind check below: it depends on the parsed address, and getting it
/// wrong exposes an authless, un-rate-limited server to the internet,
/// so it is enforced here rather than stated in prose.
pub fn run(
    db: PathBuf,
    http: Option<SocketAddr>,
    allow_hosts: Vec<String>,
    public_bind: bool,
) -> anyhow::Result<()> {
    if let Some(bind) = http
        && !is_private_bind(&bind.ip())
        && !public_bind
    {
        anyhow::bail!(
            "refusing to bind {}: not a loopback or private address. The bare \
             port has no auth and no rate limits — the sanctioned public \
             deployment is a TLS proxy in front of a loopback bind (see \
             deploy/). Bind a specific private address, or pass --public-bind \
             if you really are your own proxy.",
            bind.ip()
        );
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?
        .block_on(serve(db, http, allow_hosts))
}

/// Loopback, RFC1918, link-local, CGNAT (Tailscale hands these out), or
/// IPv6 unique-local — the binds that don't face the internet. Notably
/// excludes the unspecified addresses (0.0.0.0 / ::), which bind every
/// interface including public ones.
fn is_private_bind(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                // CGNAT 100.64.0.0/10
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 0x40)
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // unique-local fc00::/7
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
        }
    }
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
    // names (a Tailscale hostname, or the public domain a reverse proxy
    // forwards) on top of the loopback defaults.
    // Port-less entries match any port in rmcp's matcher, so bare names
    // are all that is needed (the CLI's --allow-host parser rejects
    // host:port values).
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
         authentication; bind only loopback or a private (Tailscale/WireGuard) \
         interface, and put a rate-limiting TLS proxy in front for public \
         exposure (see deploy/) — never expose this port directly"
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

#[cfg(test)]
mod bind_tests {
    use super::*;

    fn ip(s: &str) -> std::net::IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn private_binds_are_recognized() {
        for private in [
            "127.0.0.1", "10.0.0.5", "172.16.9.1", "192.168.1.2",
            "100.101.4.7", "169.254.0.1", "::1", "fd7a:115c::1", "fe80::1",
        ] {
            assert!(is_private_bind(&ip(private)), "{private} should be private");
        }
    }

    #[test]
    fn public_and_unspecified_binds_are_not() {
        for public in ["203.0.113.5", "167.99.148.37", "0.0.0.0", "2001:db8::1", "::"] {
            assert!(!is_private_bind(&ip(public)), "{public} should not be private");
        }
    }

    #[test]
    fn run_refuses_a_public_bind_without_the_flag() {
        let err = run(
            std::path::PathBuf::from("does-not-matter.sqlite"),
            Some("203.0.113.5:8642".parse().unwrap()),
            Vec::new(),
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("--public-bind"), "got: {err}");
    }
}
