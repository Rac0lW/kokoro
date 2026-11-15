//! stdio MCP transport for a running Kokoro.
//!
//! Kokoro owns the tools: this process only relays newline-delimited JSON-RPC
//! between an MCP client's stdio and Kokoro's loopback socket, so an agent sees
//! exactly the tool surface the application defines.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::UnixStream;

#[tokio::main]
async fn main() -> Result<()> {
    let path = socket_path()?;
    let stream = UnixStream::connect(&path).await.with_context(|| {
        format!(
            "could not reach Kokoro at {}; start Kokoro first",
            path.display()
        )
    })?;
    let (socket_read, mut socket_write) = stream.into_split();

    // Responses keep flowing after the client half-closes stdin, so a client
    // that pipes a whole conversation in one go still receives every result.
    let mut responses = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        let mut socket = BufReader::new(socket_read);
        let mut line = String::new();
        loop {
            line.clear();
            if socket.read_line(&mut line).await.unwrap_or(0) == 0 {
                return;
            }
            if stdout.write_all(line.as_bytes()).await.is_err() {
                return;
            }
            let _ = stdout.flush().await;
        }
    });

    let requests = async {
        let mut stdin = BufReader::new(tokio::io::stdin());
        let mut line = String::new();
        loop {
            line.clear();
            if stdin.read_line(&mut line).await? == 0 {
                return Ok::<_, std::io::Error>(());
            }
            if line.trim().is_empty() {
                continue;
            }
            socket_write.write_all(line.as_bytes()).await?;
            socket_write.flush().await?;
        }
    };

    tokio::select! {
        // Kokoro went away; nothing left to read.
        _ = &mut responses => {}
        result = requests => {
            result?;
            let _ = socket_write.shutdown().await;
            let _ = responses.await;
        }
    }
    Ok(())
}

fn socket_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("KOKORO_MCP_SOCKET") {
        return Ok(path.into());
    }
    let home = dirs::home_dir().context("the home directory is unavailable")?;
    Ok(home.join(".koharu").join("mcp.sock"))
}
