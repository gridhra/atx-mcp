//! atx-mcp: MCP サーバ本体(stdio トランスポート)。
//!
//! 起動: `atx-mcp --workspace <dir>`(env: ATX_WORKSPACE)
//! ツール仕様・annotations・返却パターンは docs/DESIGN.md §4 を正とする。
//!
//! stdout は stdio トランスポートが専有するため、ログは**必ず stderr** に出す。

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use atx_mcp::{AtxServer, AtxTools};
use clap::Parser;
use rmcp::transport::stdio;
use rmcp::ServiceExt as _;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "atx-mcp",
    about = "Deterministic image transformation MCP server (stdio transport)",
    version
)]
struct Cli {
    /// アセットワークスペースのディレクトリ(存在しなければ作成される)。
    #[arg(long, env = "ATX_WORKSPACE")]
    workspace: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // ログは stderr 固定。stdout に1バイトでも書くと JSON-RPC フレームが壊れる。
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let tools = AtxTools::open(&cli.workspace)
        .with_context(|| format!("failed to open workspace {}", cli.workspace.display()))?;
    tracing::info!(workspace = %tools.store().root().display(), "atx-mcp starting");

    let service = AtxServer::new(Arc::new(tools))
        .serve(stdio())
        .await
        .context("failed to start the MCP stdio service")?;
    let reason = service.waiting().await?;
    tracing::info!(?reason, "atx-mcp stopped");
    Ok(())
}
