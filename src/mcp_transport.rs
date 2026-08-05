//! Unix-socket MCP transport for the host-side spawn broker.
//!
//! The broker itself lives in [`crate::orchestration`]. This module is only the
//! newline-delimited JSON-RPC transport: each line read from a sandbox-visible
//! Unix socket is dispatched to `SpawnBroker::handle_rpc`, then answered with one
//! JSON line. The socket path is created by the run path inside the mounted
//! project tree, so a sandboxed master can reach it without receiving any host
//! process capability.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::orchestration::{SpawnBroker, SubtaskLauncher};

pub type SharedSpawnBroker<L> = Arc<SpawnBroker<L>>;

pub async fn serve_unix_socket<L>(
    socket_path: &Path,
    parent_id: String,
    broker: SharedSpawnBroker<L>,
) -> Result<()>
where
    L: SubtaskLauncher + Send + 'static,
{
    if socket_path.exists() {
        std::fs::remove_file(socket_path).with_context(|| {
            format!(
                "failed to remove stale MCP socket at {}",
                socket_path.display()
            )
        })?;
    }
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("failed to create MCP socket directory {}", parent.display())
        })?;
    }
    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("failed to bind MCP socket at {}", socket_path.display()))?;

    loop {
        let (stream, _) = listener
            .accept()
            .await
            .context("failed to accept MCP client")?;
        let parent_id = parent_id.clone();
        let broker = Arc::clone(&broker);
        tokio::spawn(async move {
            if let Err(error) = handle_stream(stream, parent_id, broker).await {
                eprintln!("warning: MCP broker stream failed: {error:#}");
            }
        });
    }
}

async fn handle_stream<L>(
    stream: UnixStream,
    parent_id: String,
    broker: SharedSpawnBroker<L>,
) -> Result<()>
where
    L: SubtaskLauncher + Send + 'static,
{
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines
        .next_line()
        .await
        .context("failed to read MCP request")?
    {
        if line.trim().is_empty() {
            continue;
        }
        let request: serde_json::Value =
            serde_json::from_str(&line).context("MCP request was not valid JSON")?;
        let response = broker.handle_rpc(&parent_id, &request);
        let response = serde_json::to_vec(&response).context("failed to encode MCP response")?;
        write
            .write_all(&response)
            .await
            .context("failed to write MCP response")?;
        write
            .write_all(b"\n")
            .await
            .context("failed to terminate MCP response")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    use super::*;
    use crate::orchestration::{
        OrchestrationPolicy, SPAWN_SUBTASK_TOOL, SpawnGrant, SpawnRequest, SubtaskId,
        SubtaskLauncher,
    };

    struct MockLauncher;

    impl SubtaskLauncher for MockLauncher {
        fn launch(
            &mut self,
            _req: &SpawnRequest,
            _grant: &SpawnGrant,
        ) -> anyhow::Result<SubtaskId> {
            Ok("child-1".to_owned())
        }
    }

    #[tokio::test]
    async fn unix_socket_round_trips_spawn_subtask_rpc() {
        let root = Path::new("/tmp").join(format!(
            "vmcp-{}-{}",
            std::process::id(),
            &uuid::Uuid::new_v4().to_string()[..8]
        ));
        std::fs::create_dir_all(&root).unwrap();
        let socket = root.join("broker.sock");
        let policy = OrchestrationPolicy {
            enabled: true,
            ..Default::default()
        };
        let broker = Arc::new(crate::orchestration::SpawnBroker::new(
            policy,
            "root",
            MockLauncher,
        ));
        let server_socket = socket.clone();
        let server = tokio::spawn(async move {
            let _ = serve_unix_socket(&server_socket, "root".to_owned(), broker).await;
        });

        for _ in 0..50 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
        let (read, mut write) = stream.into_split();
        write
            .write_all(
                format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{{\"name\":\"{}\",\"arguments\":{{\"brief\":\"do it\"}}}}}}\n",
                    SPAWN_SUBTASK_TOOL
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut line = String::new();
        BufReader::new(read).read_line(&mut line).await.unwrap();
        let response: serde_json::Value = serde_json::from_str(&line).unwrap();

        server.abort();
        let _ = std::fs::remove_file(&socket);
        let _ = std::fs::remove_dir(&root);

        assert_eq!(response["result"]["isError"], serde_json::json!(false));
        assert_eq!(
            response["result"]["content"][0]["text"],
            serde_json::json!("subtask_id: child-1")
        );
    }
}
