//! maestro-mcp — the stdio MCP proxy a Claude Code advisor session connects to
//! (ADR-006). A thin JSON-RPC 2.0 (newline-delimited) proxy to the daemon socket
//! that mints the `advisor_session_id` at startup and appends the advisor inbox
//! to every tool result.
//!
//! - [`transport`]: [`DaemonTransport`] seam + the real [`SocketTransport`]
//!   (auto-spawn race, one exchange per call).
//! - [`server`]: the transport-agnostic MCP core; [`McpServer::handle_line`] is
//!   the unit-test seam.

pub mod server;
pub mod transport;

pub use server::McpServer;
pub use transport::{DaemonTransport, SocketTransport};
