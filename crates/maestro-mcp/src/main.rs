//! maestro-mcp — stdio MCP proxy; mints advisor_session_id, forwards advisor
//! tool calls to the daemon socket (ADR-006).
//!
//! Transport: JSON-RPC 2.0, newline-delimited, on stdin/stdout. stdout is the
//! JSON-RPC channel ONLY — all logging goes to stderr.

use std::io::{self, BufRead, Write};

use maestro_mcp::{McpServer, SocketTransport};

fn main() {
    // Active profile from the environment (else None → daemon's default).
    let profile = std::env::var("MAESTRO_PROFILE")
        .ok()
        .filter(|s| !s.is_empty());

    let transport = Box::new(SocketTransport::new(profile.clone()));
    let mut server = McpServer::new(transport, profile);

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("maestro-mcp: stdin read error: {e}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = server.handle_line(&line) {
            if let Err(e) = writeln!(out, "{response}").and_then(|()| out.flush()) {
                eprintln!("maestro-mcp: stdout write error: {e}");
                break;
            }
        }
    }
    // Clean exit on EOF.
}
