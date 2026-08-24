//! MCP transports for the host-side spawn broker.
//!
//! The broker itself lives in [`crate::orchestration`]. This module is only the
//! newline-delimited JSON-RPC transport: each line read from a sandbox-visible
//! endpoint is dispatched to `SpawnBroker::handle_rpc`, then answered with one
//! JSON line.
//!
//! Two transports exist, selected by the sandbox primitive (see
//! `crate::config::primitive_needs_tcp_broker`):
//!
//! - **Unix socket** ([`serve_unix_socket`]) — for `local` (and shared-kernel
//!   `docker`) launches the socket path is created inside the mounted project
//!   tree, so a sandboxed master reaches it through the bind mount without
//!   receiving any host process capability.
//! - **TCP** ([`bind_tcp`] + [`serve_tcp`]) — for own-kernel microVM primitives
//!   (`microsandbox`, `clawk`) the project bind mount shares the socket *file*
//!   over virtio-fs but NOT the AF_UNIX endpoint, so an in-guest `connect()` is
//!   refused. The guest instead reaches the host over TCP (its default gateway),
//!   so the broker binds a host TCP listener on an ephemeral port and advertises
//!   `host:port` to the guest. The listener is short-lived (torn down with the
//!   session) and the broker is capability-gated regardless of reachability, so
//!   a reachable port grants no capability the unix socket did not.

use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixListener};

use crate::orchestration::{SpawnBroker, SubtaskLauncher};

pub type SharedSpawnBroker<L> = Arc<SpawnBroker<L>>;

/// Bind a host TCP listener for the broker on `bind_ip:0` (an ephemeral port).
///
/// Binding is eager and separate from serving so the caller can read the real
/// port ([`TcpListener::local_addr`]) and advertise it to the guest as
/// `VARDA_MCP_ADDR` BEFORE the agent starts, then hand the listener to
/// [`serve_tcp`]. `bind_ip` is a host-only interface (loopback by default, or the
/// per-sandbox gateway) — never a broad public exposure.
pub async fn bind_tcp(bind_ip: IpAddr) -> Result<(SocketAddr, TcpListener)> {
    let listener = TcpListener::bind((bind_ip, 0))
        .await
        .with_context(|| format!("failed to bind MCP TCP listener on {bind_ip}"))?;
    let addr = listener
        .local_addr()
        .context("failed to read bound MCP TCP address")?;
    Ok((addr, listener))
}

/// Serve the broker over a TCP listener obtained from [`bind_tcp`]. Each accepted
/// connection is dispatched exactly like the unix-socket transport.
pub async fn serve_tcp<L>(
    listener: TcpListener,
    parent_id: String,
    broker: SharedSpawnBroker<L>,
) -> Result<()>
where
    L: SubtaskLauncher + Send + 'static,
{
    loop {
        let (stream, _) = listener
            .accept()
            .await
            .context("failed to accept MCP TCP client")?;
        let parent_id = parent_id.clone();
        let broker = Arc::clone(&broker);
        tokio::spawn(async move {
            if let Err(error) = handle_stream(stream, parent_id, broker).await {
                eprintln!("warning: MCP broker stream failed: {error:#}");
            }
        });
    }
}

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

/// Dispatch newline-delimited JSON-RPC over any bidirectional stream (a
/// [`tokio::net::UnixStream`] or [`tokio::net::TcpStream`]). The wire framing is
/// identical for both transports; only the endpoint kind differs.
/// Serve one connection.
///
/// Requests are dispatched CONCURRENTLY rather than one-at-a-time (#667). Two
/// properties matter and both were violated by the previous read-handle-write
/// loop:
///
/// 1. `SpawnBroker::handle_rpc` is synchronous, and `await_subtask*` sleeps
///    inside it for up to `max_wait` (30 minutes by default). Calling it inline
///    meant the next request was not even READ until the current one returned, so
///    a parent blocked in `await_subtask` could not issue `get_task`,
///    `set_task_status`, `subtask_result` or `spawn_subtask` — it went deaf for
///    the whole await and could not supervise its own workers.
/// 2. `std::thread::sleep` inside an async task PARKS a runtime worker thread
///    instead of yielding it. Enough concurrent awaits and the runtime starves,
///    which also stalls the detached tasks that settle spawned subtasks.
///
/// So each request is handed to `spawn_blocking` (keeping the blocking sleep off
/// the async workers) and its response is funnelled through a single writer task.
/// Responses may therefore complete out of order — which JSON-RPC permits, since
/// every response carries its request id.
async fn handle_stream<S, L>(
    stream: S,
    parent_id: String,
    broker: SharedSpawnBroker<L>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    L: SubtaskLauncher + Send + 'static,
{
    let (read, mut write) = tokio::io::split(stream);
    let mut lines = BufReader::new(read).lines();

    // One writer task owns the write half; handlers hand it encoded responses.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let writer = tokio::spawn(async move {
        while let Some(payload) = rx.recv().await {
            if let Err(error) = write.write_all(&payload).await {
                eprintln!("warning: MCP broker response write failed: {error:#}");
                break;
            }
            if let Err(error) = write.write_all(b"\n").await {
                eprintln!("warning: MCP broker response write failed: {error:#}");
                break;
            }
        }
    });

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
        let broker = Arc::clone(&broker);
        let parent_id = parent_id.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let handled =
                tokio::task::spawn_blocking(move || broker.handle_rpc(&parent_id, &request)).await;
            let response = match handled {
                Ok(response) => response,
                Err(error) => {
                    eprintln!("warning: MCP broker handler failed to join: {error}");
                    return;
                }
            };
            match serde_json::to_vec(&response) {
                Ok(payload) => {
                    let _ = tx.send(payload);
                }
                Err(error) => {
                    eprintln!("warning: failed to encode MCP response: {error:#}")
                }
            }
        });
    }

    // Drop our sender so the writer drains and exits once in-flight handlers finish.
    drop(tx);
    let _ = writer.await;
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
        fn run_existing(
            &mut self,
            task_id: &str,
            _grant: &SpawnGrant,
        ) -> anyhow::Result<SubtaskId> {
            Ok(task_id.to_owned())
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

    #[tokio::test]
    async fn tcp_round_trips_spawn_subtask_rpc() {
        let policy = OrchestrationPolicy {
            enabled: true,
            ..Default::default()
        };
        let broker = Arc::new(crate::orchestration::SpawnBroker::new(
            policy,
            "root",
            MockLauncher,
        ));
        // Loopback stands in for the per-sandbox gateway the msb guest reaches.
        let (addr, listener) = bind_tcp(std::net::Ipv4Addr::LOCALHOST.into())
            .await
            .unwrap();
        assert_ne!(addr.port(), 0, "an ephemeral port must be assigned");
        let server = tokio::spawn(async move {
            let _ = serve_tcp(listener, "root".to_owned(), broker).await;
        });

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
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

        assert_eq!(response["result"]["isError"], serde_json::json!(false));
        assert_eq!(
            response["result"]["content"][0]["text"],
            serde_json::json!("subtask_id: child-1")
        );
    }

    /// #667 regression: a request blocked inside `handle_rpc` must not starve a
    /// short request behind it on the SAME connection. Before concurrent dispatch,
    /// `await_subtask` held the read loop for its whole `max_wait`, so a resident
    /// that was awaiting went deaf — it could not even list tools, let alone check
    /// on the worker it was waiting for.
    struct NeverSettles;

    impl crate::orchestration::SubtaskResults for NeverSettles {
        fn status(&self, _id: &str) -> crate::orchestration::SubtaskStatus {
            crate::orchestration::SubtaskStatus::Found(crate::task::TaskStatus::Running)
        }
        fn recap(&self, _id: &str) -> Option<String> {
            None
        }
        fn is_available(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn a_blocked_await_does_not_starve_later_requests_on_the_same_connection() {
        let policy = OrchestrationPolicy {
            enabled: true,
            ..Default::default()
        };
        // The await can never settle. Its ceiling is deliberately SHORT (2s): long
        // enough that a serialized transport could only answer the second call
        // after it, short enough that the test cannot hang — a `spawn_blocking`
        // task cannot be cancelled, and dropping the runtime waits for it.
        let broker = Arc::new(
            crate::orchestration::SpawnBroker::new(policy, "root", MockLauncher)
                .with_results(NeverSettles)
                .with_poll_timing(
                    std::time::Duration::from_millis(5),
                    std::time::Duration::from_secs(2),
                ),
        );
        let (addr, listener) = bind_tcp(std::net::Ipv4Addr::LOCALHOST.into())
            .await
            .unwrap();
        let server = tokio::spawn(async move {
            let _ = serve_tcp(listener, "root".to_owned(), broker).await;
        });

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (read, mut write) = stream.into_split();

        // 1: blocks until the 2s ceiling. 2: must be answered long before that.
        write
            .write_all(
                format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{{\"name\":\"{}\",\"arguments\":{{\"subtask_id\":\"never\"}}}}}}\n",
                    crate::orchestration::AWAIT_SUBTASK_TOOL
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        write
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n")
            .await
            .unwrap();

        let started = std::time::Instant::now();
        let mut line = String::new();
        let read_second = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            BufReader::new(read).read_line(&mut line),
        )
        .await;
        server.abort();

        read_second
            .expect("the second request must be answered while the first is still blocked")
            .unwrap();
        let response: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            response["id"],
            serde_json::json!(2),
            "the short request must come back first, out of order, while the await blocks"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "answered promptly rather than after the 2s await ceiling"
        );
    }
}
