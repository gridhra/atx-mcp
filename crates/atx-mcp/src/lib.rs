//! atx-mcp: MCP サーバ実装(rmcp / stdio)。
//!
//! ツールの実処理は [`tools::AtxTools`] に切り出してある(stdio 非依存・同期)ので、
//! 統合テストからトランスポート抜きで直接叩ける。
//! rmcp のツール定義(`#[tool]` / `#[tool_router]` / `#[tool_handler]`)は [`server`]。

pub mod server;
pub mod tools;

pub use server::AtxServer;
pub use tools::AtxTools;
