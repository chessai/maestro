//! Wire client for the daemon Unix-socket API (ADR-006).
//!
//! One request per connection: write ONE line (compact JSON `Request` + `'\n'`),
//! read ONE line (`BufReader::read_line`), deserialize `Response`, close. The
//! daemon (`maestro-daemon`) implements the identical protocol.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::{bail, Context, Result};

use maestro_journal::proto::{Request, Response};

/// Send a single request over a connected [`UnixStream`] and read the response.
///
/// Factored out so it can be exercised against a throwaway `UnixListener` in
/// tests without any daemon binary (see the wire round-trip test).
pub fn exchange(stream: UnixStream, req: &Request) -> Result<Response> {
    let mut reader = BufReader::new(stream.try_clone().context("cloning socket stream")?);
    let mut write_half = stream;

    let mut out = serde_json::to_string(req).context("serializing request")?;
    out.push('\n');
    write_half
        .write_all(out.as_bytes())
        .context("writing request")?;
    write_half.flush().context("flushing request")?;

    let mut line = String::new();
    let n = reader
        .read_line(&mut line)
        .context("reading response line")?;
    if n == 0 {
        bail!("daemon closed the connection without responding");
    }
    let resp: Response =
        serde_json::from_str(line.trim_end()).context("deserializing response")?;
    Ok(resp)
}

/// Connect to the socket at `path` and perform a single request/response
/// exchange. Returns `Err` if the socket cannot be connected (ENOENT /
/// ECONNREFUSED) or the exchange fails.
pub fn request_at(path: &Path, req: &Request) -> Result<Response> {
    let stream = UnixStream::connect(path)
        .with_context(|| format!("connecting to daemon socket {}", path.display()))?;
    exchange(stream, req)
}
