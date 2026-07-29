//! MCP server (stdio + http transports).
//!
//! M1 spike: one hardcoded tool over stdio, to prove the rmcp round trip
//! before M5 builds the real `search_posts`/`get_topic` tools on it.

use rmcp::{ServiceExt, tool, tool_router, transport::stdio};

#[derive(Clone)]
struct WikipethiaSpike;

#[tool_router(server_handler)]
impl WikipethiaSpike {
    #[tool(
        name = "ping",
        description = "Health check for the wikipethia corpus server. Returns a fixed string."
    )]
    fn ping(&self) -> String {
        "pong — wikipethia rmcp spike, round trip works".to_string()
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let service = WikipethiaSpike.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
