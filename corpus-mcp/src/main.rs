//! MCP server: the corpus exposed to LLM clients over stdio (default) or
//! streamable HTTP (`--http <addr>`).
//!
//! In stdio mode stdout carries the protocol — every diagnostic in this
//! crate must go to stderr, in both modes, for consistency.

mod tools;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use corpus_core::Store;
use corpus_embed::FastEmbedder;
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use rmcp::transport::streamable_http_server::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::StreamableHttpServerConfig;
use tokio_util::sync::CancellationToken;

use tools::CorpusServer;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse()?;
    let store = Store::open(&args.db)?;
    if store.count()? == 0 {
        // Failing the connection beats serving an empty corpus silently —
        // this line shows up in the client's MCP logs.
        anyhow::bail!(
            "{} holds no documents — run `cargo run -p corpus-cli -- index` first",
            args.db.display()
        );
    }
    // A db migrated from pre-M6 has documents but no manifest tiers until
    // an index run records them — surface that instead of silently serving
    // tierless citations (the retrieval invariant promises tier).
    for stats in store.source_stats()? {
        if stats.tier.is_none() {
            eprintln!(
                "corpus-mcp: source {:?} has no tier recorded — run \
                 `cargo run -p corpus-cli -- index` to record manifest tiers",
                stats.id
            );
        }
    }
    let embedder = if store.embedding_count()? > 0 {
        // Built once; model load is slow and FastEmbedder is shared safely.
        Some(FastEmbedder::new()?)
    } else {
        eprintln!(
            "corpus-mcp: {} has no embeddings — serving lexical-only ranking \
             (run `cargo run -p corpus-cli -- embed`)",
            args.db.display()
        );
        None
    };
    let server = CorpusServer::new(store, embedder)?;

    match args.http {
        None => {
            let service = server.serve(stdio()).await?;
            service.waiting().await?;
        }
        Some(bind) => serve_http(server, bind, args.allow_hosts).await?,
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
    let mut hosts = vec!["localhost".to_string(), "127.0.0.1".to_string(), "::1".to_string()];
    hosts.push(bind.ip().to_string());
    // Host headers carry the port for non-default ports.
    hosts.push(bind.to_string());
    for host in extra_hosts {
        hosts.push(format!("{host}:{}", bind.port()));
        hosts.push(host);
    }

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
        "corpus-mcp: serving streamable HTTP on http://{bind}/mcp — no \
         authentication; bind only to loopback or a private (Tailscale/\
         WireGuard) interface, never a public one"
    );

    // One token drives both halves of shutdown: active MCP sessions
    // terminate and axum stops accepting, then drains.
    let shutdown = ct.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        eprintln!("corpus-mcp: shutting down");
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

/// Hand-rolled to stay dependency-free: `--db <path>`, `--http <addr>`,
/// `--allow-host <name>` (repeatable). db resolution: `--db` beats
/// `CORPUS_DB` beats `corpus.sqlite` (the .mcp.json entry launches from
/// the repo root, so the relative default works).
struct Args {
    db: PathBuf,
    http: Option<SocketAddr>,
    allow_hosts: Vec<String>,
}

impl Args {
    fn parse() -> anyhow::Result<Self> {
        let mut db = None;
        let mut http = None;
        let mut allow_hosts = Vec::new();
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--db" => db = Some(PathBuf::from(next_value(&mut args, "--db")?)),
                "--http" => {
                    let addr = next_value(&mut args, "--http")?;
                    http = Some(addr.parse().map_err(|e| {
                        anyhow::anyhow!("--http needs a bind address like 127.0.0.1:8642: {e}")
                    })?);
                }
                "--allow-host" => allow_hosts.push(next_value(&mut args, "--allow-host")?),
                other => anyhow::bail!(
                    "unknown argument {other:?} — corpus-mcp [--db <path>] \
                     [--http <addr> [--allow-host <name>]...]"
                ),
            }
        }
        if http.is_none() && !allow_hosts.is_empty() {
            anyhow::bail!("--allow-host only applies to --http mode");
        }
        let db = db
            .or_else(|| std::env::var("CORPUS_DB").ok().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("corpus.sqlite"));
        Ok(Self {
            db,
            http,
            allow_hosts,
        })
    }
}

fn next_value(args: &mut impl Iterator<Item = String>, flag: &str) -> anyhow::Result<String> {
    args.next()
        .ok_or_else(|| anyhow::anyhow!("{flag} needs a value"))
}
