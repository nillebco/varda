//! In-process allow-listing HTTP(S) forward proxy.
//!
//! The `processcontainer` primitive (Windows MXC) cannot filter egress by
//! hostname: MXC's `egress.allow` takes CIDR + port only. Instead the container
//! is confined to ONE loopback endpoint (`runtimeConfig.networkProxy`, MXC
//! "Model 2"), and this proxy, running inside the `varda sandbox-exec` relay on
//! the host, decides which hosts that endpoint forwards to.
//!
//! Allow-list semantics mirror the docker tinyproxy sidecar
//! (`tinyproxy_filter` / `tinyproxy_conf` in the parent module):
//! - `example.com` matches that exact host; `*.example.com` matches exactly one
//!   extra label.
//! - CONNECT is allowed to ports 443/563, plus any port an entry names
//!   explicitly (`127.0.0.1:8765`).
//! - Absolute-form plain-HTTP requests are allowed to port 80, plus explicitly
//!   named ports.
//!
//! Everything else gets `403`. Each decision is appended to a log file (never
//! stderr: the agent's TUI owns the terminal).

use std::io::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// One parsed allow-list entry.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    /// Lowercased host, or the suffix after `*.` when `wildcard`.
    host: String,
    wildcard: bool,
    /// Explicit port named by the entry (`host:port`), if any.
    port: Option<u16>,
}

/// Parsed allow-list (see the module docs for the matching rules).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AllowList {
    rules: Vec<Rule>,
}

const DEFAULT_CONNECT_PORTS: &[u16] = &[443, 563];
const DEFAULT_HTTP_PORTS: &[u16] = &[80];

impl AllowList {
    pub fn parse(entries: &[String]) -> Self {
        let rules = entries
            .iter()
            .filter_map(|raw| {
                let entry = raw.trim();
                if entry.is_empty() {
                    return None;
                }
                let (host, port) = match entry.rsplit_once(':') {
                    Some((host, port))
                        if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) =>
                    {
                        (host, port.parse::<u16>().ok())
                    }
                    _ => (entry, None),
                };
                let host = host.trim_start_matches('[').trim_end_matches(']');
                let host = host.to_ascii_lowercase();
                Some(match host.strip_prefix("*.") {
                    Some(suffix) => Rule {
                        host: suffix.to_owned(),
                        wildcard: true,
                        port,
                    },
                    None => Rule {
                        host,
                        wildcard: false,
                        port,
                    },
                })
            })
            .collect();
        Self { rules }
    }

    fn host_matches(rule: &Rule, host: &str) -> bool {
        if rule.wildcard {
            host.strip_suffix(&rule.host)
                .and_then(|prefix| prefix.strip_suffix('.'))
                .is_some_and(|label| !label.is_empty() && !label.contains('.'))
        } else {
            host == rule.host
        }
    }

    fn allows(&self, host: &str, port: u16, default_ports: &[u16]) -> bool {
        let host = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .trim_end_matches('.')
            .to_ascii_lowercase();
        let host_rules: Vec<&Rule> = self
            .rules
            .iter()
            .filter(|rule| Self::host_matches(rule, &host))
            .collect();
        if host_rules.is_empty() {
            return false;
        }
        default_ports.contains(&port) || host_rules.iter().any(|rule| rule.port == Some(port))
    }

    /// Whether a `CONNECT host:port` tunnel is allowed.
    pub fn allows_connect(&self, host: &str, port: u16) -> bool {
        self.allows(host, port, DEFAULT_CONNECT_PORTS)
    }

    /// Whether an absolute-form plain-HTTP request to `host:port` is allowed.
    pub fn allows_http(&self, host: &str, port: u16) -> bool {
        self.allows(host, port, DEFAULT_HTTP_PORTS)
    }
}

/// Split `host:port` (IPv6 in brackets allowed). `None` when the port is
/// missing or not a number.
fn split_authority(authority: &str) -> Option<(String, u16)> {
    let (host, port) = authority.rsplit_once(':')?;
    let port = port.parse::<u16>().ok()?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() {
        return None;
    }
    Some((host.to_owned(), port))
}

/// Parse an absolute-form `http://host[:port]/path` target into
/// `(host, port, origin-form path)`. Only `http` is accepted: an HTTPS client
/// must CONNECT.
fn split_absolute_http(target: &str) -> Option<(String, u16, String)> {
    let rest = target.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);
    let (host, port) = if authority.starts_with('[') {
        match authority.find("]:") {
            Some(end) => (&authority[1..end], authority[end + 2..].parse().ok()?),
            None => (authority.trim_start_matches('[').trim_end_matches(']'), 80),
        }
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, port.parse().ok()?),
            None => (authority, 80),
        }
    };
    if host.is_empty() {
        return None;
    }
    Some((host.to_owned(), port, path.to_owned()))
}

/// A running proxy. Dropping it does NOT stop the accept loop; it lives as long
/// as the tokio runtime (the relay process).
pub struct EgressProxy {
    pub addr: SocketAddr,
}

#[derive(Clone)]
struct ProxyLog(Arc<Mutex<Option<std::fs::File>>>);

impl ProxyLog {
    fn open(path: Option<PathBuf>) -> Self {
        let file = path.and_then(|path| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()
        });
        Self(Arc::new(Mutex::new(file)))
    }

    fn line(&self, text: &str) {
        if let Ok(mut guard) = self.0.lock()
            && let Some(file) = guard.as_mut()
        {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or_default();
            let _ = writeln!(file, "{secs} {text}");
        }
    }
}

/// Bind `127.0.0.1:0` and start serving. The bound address is fixed for the
/// proxy's lifetime, so the port cannot be re-taken by another host process
/// while the container runs (the identity-less proxy's squatting risk is only
/// the window before this bind).
pub async fn start(allow: AllowList, log_path: Option<PathBuf>) -> Result<EgressProxy> {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .context("failed to bind the egress proxy on 127.0.0.1")?;
    let addr = listener.local_addr()?;
    let allow = Arc::new(allow);
    let log = ProxyLog::open(log_path);
    log.line(&format!("listening {addr}"));
    tokio::spawn(async move {
        loop {
            let Ok((stream, _peer)) = listener.accept().await else {
                continue;
            };
            let allow = allow.clone();
            let log = log.clone();
            tokio::spawn(async move {
                if let Err(error) = handle(stream, &allow, &log).await {
                    log.line(&format!("error {error:#}"));
                }
            });
        }
    });
    Ok(EgressProxy { addr })
}

async fn respond(stream: &mut TcpStream, status: &str) -> Result<()> {
    let body = format!("varda egress proxy: {status}\n");
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    Ok(())
}

async fn handle(stream: TcpStream, allow: &AllowList, log: &ProxyLog) -> Result<()> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).await? == 0 {
        return Ok(());
    }
    // Headers up to the blank line. Kept verbatim for plain-HTTP forwarding.
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            break;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        headers.push(line);
        if headers.len() > 200 {
            anyhow::bail!("too many request headers");
        }
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default().to_owned();
    let version = parts.next().unwrap_or("HTTP/1.1").to_owned();

    if method.eq_ignore_ascii_case("CONNECT") {
        let Some((host, port)) = split_authority(&target) else {
            log.line(&format!("DENY malformed CONNECT {target}"));
            return respond(reader.get_mut(), "400 Bad Request").await;
        };
        if !allow.allows_connect(&host, port) {
            log.line(&format!("DENY CONNECT {host}:{port}"));
            return respond(reader.get_mut(), "403 Forbidden").await;
        }
        let mut upstream = match TcpStream::connect((host.as_str(), port)).await {
            Ok(upstream) => upstream,
            Err(error) => {
                log.line(&format!("FAIL CONNECT {host}:{port} {error}"));
                return respond(reader.get_mut(), "502 Bad Gateway").await;
            }
        };
        log.line(&format!("ALLOW CONNECT {host}:{port}"));
        // Bytes the client may have pipelined after the CONNECT headers.
        let buffered = reader.buffer().to_vec();
        let mut client = reader.into_inner();
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
        if !buffered.is_empty() {
            upstream.write_all(&buffered).await?;
        }
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
        return Ok(());
    }

    let Some((host, port, path)) = split_absolute_http(&target) else {
        log.line(&format!("DENY non-proxy request {method} {target}"));
        return respond(reader.get_mut(), "400 Bad Request").await;
    };
    if !allow.allows_http(&host, port) {
        log.line(&format!("DENY {method} http://{host}:{port}"));
        return respond(reader.get_mut(), "403 Forbidden").await;
    }
    let mut upstream = match TcpStream::connect((host.as_str(), port)).await {
        Ok(upstream) => upstream,
        Err(error) => {
            log.line(&format!("FAIL {method} http://{host}:{port} {error}"));
            return respond(reader.get_mut(), "502 Bad Gateway").await;
        }
    };
    log.line(&format!("ALLOW {method} http://{host}:{port}"));
    // Forward in origin-form. One request per connection: hop-by-hop
    // `Proxy-*`/`Connection` headers are replaced with `Connection: close`, so
    // the connection ends after this exchange and keep-alive cannot smuggle a
    // second request to a different host.
    let mut head = format!("{method} {path} {version}\r\n");
    for header in &headers {
        let name = header.split(':').next().unwrap_or_default().trim();
        if name.eq_ignore_ascii_case("proxy-connection")
            || name.eq_ignore_ascii_case("proxy-authorization")
            || name.eq_ignore_ascii_case("connection")
            || name.eq_ignore_ascii_case("keep-alive")
        {
            continue;
        }
        head.push_str(header);
    }
    head.push_str("Connection: close\r\n\r\n");
    upstream.write_all(head.as_bytes()).await?;
    let buffered = reader.buffer().to_vec();
    if !buffered.is_empty() {
        upstream.write_all(&buffered).await?;
    }
    let mut client = reader.into_inner();
    let (mut client_read, mut client_write) = client.split();
    let (mut upstream_read, mut upstream_write) = upstream.split();
    let to_upstream = async {
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = client_read.read(&mut buf).await?;
            if n == 0 {
                let _ = upstream_write.shutdown().await;
                return Ok::<(), std::io::Error>(());
            }
            upstream_write.write_all(&buf[..n]).await?;
        }
    };
    let to_client = async {
        let result = tokio::io::copy(&mut upstream_read, &mut client_write).await;
        let _ = client_write.shutdown().await;
        result
    };
    // The exchange ends when the upstream response ends (`Connection: close`);
    // a request body still streaming the other way is cut off with it.
    tokio::select! {
        _ = to_client => {}
        _ = to_upstream => {
            // Client finished sending; still relay the rest of the response.
            let _ = tokio::io::copy(&mut upstream_read, &mut client_write).await;
            let _ = client_write.shutdown().await;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(entries: &[&str]) -> AllowList {
        AllowList::parse(&entries.iter().map(|e| e.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn exact_host_allows_default_connect_ports_only() {
        let allow = list(&["api.github.com"]);
        assert!(allow.allows_connect("api.github.com", 443));
        assert!(allow.allows_connect("API.GitHub.com", 443));
        assert!(!allow.allows_connect("api.github.com", 22));
        assert!(!allow.allows_connect("github.com", 443));
        assert!(!allow.allows_connect("evil.api.github.com", 443));
    }

    #[test]
    fn explicit_port_opens_that_port_for_connect_and_http() {
        let allow = list(&["127.0.0.1:8765"]);
        assert!(allow.allows_connect("127.0.0.1", 8765));
        assert!(allow.allows_http("127.0.0.1", 8765));
        assert!(allow.allows_connect("127.0.0.1", 443));
        assert!(!allow.allows_http("127.0.0.1", 8766));
        assert!(!allow.allows_connect("127.0.0.2", 8765));
    }

    #[test]
    fn wildcard_matches_exactly_one_label() {
        let allow = list(&["*.blob.core.windows.net"]);
        assert!(allow.allows_connect("acct.blob.core.windows.net", 443));
        assert!(!allow.allows_connect("blob.core.windows.net", 443));
        assert!(!allow.allows_connect("a.b.blob.core.windows.net", 443));
    }

    #[test]
    fn empty_list_denies_everything() {
        let allow = list(&[]);
        assert!(!allow.allows_connect("api.github.com", 443));
        assert!(!allow.allows_http("example.com", 80));
    }

    #[test]
    fn absolute_http_targets_parse() {
        assert_eq!(
            split_absolute_http("http://127.0.0.1:8765/mcp?x=1"),
            Some(("127.0.0.1".into(), 8765, "/mcp?x=1".into()))
        );
        assert_eq!(
            split_absolute_http("http://example.com"),
            Some(("example.com".into(), 80, "/".into()))
        );
        assert_eq!(
            split_absolute_http("http://[::1]:9000/a"),
            Some(("::1".into(), 9000, "/a".into()))
        );
        assert_eq!(split_absolute_http("https://example.com/"), None);
        assert_eq!(split_absolute_http("/relative"), None);
    }

    async fn roundtrip(proxy: SocketAddr, request: &str) -> String {
        let mut stream = TcpStream::connect(proxy).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut out = String::new();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            stream.read_to_string(&mut out),
        )
        .await;
        out
    }

    #[tokio::test]
    async fn proxy_forwards_allowed_http_and_refuses_the_rest() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = upstream.accept().await {
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]).to_string();
                    let first = request.lines().next().unwrap_or_default().to_owned();
                    let body = format!("UPSTREAM {first}");
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        let port = upstream_addr.port();
        let proxy = start(list(&[&format!("127.0.0.1:{port}")]), None)
            .await
            .unwrap();

        let ok = roundtrip(
            proxy.addr,
            &format!("GET http://127.0.0.1:{port}/mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
        )
        .await;
        assert!(ok.contains("UPSTREAM GET /mcp HTTP/1.1"), "{ok}");

        let denied = roundtrip(
            proxy.addr,
            "GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n",
        )
        .await;
        assert!(denied.starts_with("HTTP/1.1 403"), "{denied}");

        let denied_connect = roundtrip(
            proxy.addr,
            "CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n",
        )
        .await;
        assert!(denied_connect.starts_with("HTTP/1.1 403"), "{denied_connect}");
    }

    #[tokio::test]
    async fn proxy_tunnels_an_allowed_connect() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = upstream.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = upstream.accept().await {
                let _ = socket.write_all(b"TUNNEL-OK").await;
            }
        });
        let proxy = start(list(&[&format!("127.0.0.1:{port}")]), None)
            .await
            .unwrap();
        let out = roundtrip(
            proxy.addr,
            &format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
        )
        .await;
        assert!(out.starts_with("HTTP/1.1 200"), "{out}");
        assert!(out.ends_with("TUNNEL-OK"), "{out}");
    }
}
