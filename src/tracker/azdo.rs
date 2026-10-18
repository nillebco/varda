//! Azure DevOps backend, read path (milestone T1): WIQL query + work-item
//! batch fetch, mapped to [`RemoteIssue`] with HTML descriptions converted to
//! markdown. The write methods are filled in by the next task.
//!
//! All HTTP goes through the [`Transport`] trait so tests inject a local fake.
//! The PAT is resolved on the host by [`super::build_tracker`], sent as
//! `Authorization: Basic base64(":" + pat)`, and scrubbed from every error.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use serde_json::{Value, json};

use super::{IssueDraft, IssueTracker, RemoteIssue, Result, TrackerError, map_status};
use crate::config::{TrackerConfig, TrackerFilter};
use crate::task::TaskStatus;

const API_VERSION: &str = "7.1";
const BATCH_SIZE: usize = 200;
const COMMENT_API_VERSION: &str = "7.1-preview.4";
const PATCH_CONTENT_TYPE: &str = "application/json-patch+json";
const DEFAULT_WORK_ITEM_TYPE: &str = "Task";
/// Comments longer than this are truncated (with a note) before posting.
const MAX_COMMENT_BYTES: usize = 100 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Cap on the whole raw response (headers + body) read from the socket.
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_RETRIES: usize = 2;
/// Total time spent waiting on `Retry-After` across all retries of one request.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(30);

const FIELDS: &[&str] = &[
    "System.Title",
    "System.Description",
    "System.State",
    "System.Tags",
    "System.AssignedTo",
    "System.Rev",
    "System.WorkItemType",
    "Microsoft.VSTS.Common.AcceptanceCriteria",
];

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    /// Header names lower-cased.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Blocking HTTP transport; run inside `spawn_blocking`.
pub trait Transport: Send + Sync {
    fn send(&self, request: &HttpRequest) -> anyhow::Result<HttpResponse>;
}

/// HTTPS-capable transport (`ureq` + rustls, webpki roots) — the production
/// transport for `https://` base URLs. Redirects are NOT followed: Azure DevOps
/// answers a bad/expired PAT with a redirect to the sign-in page, which must
/// surface as an error rather than as a 200 HTML body. Non-2xx statuses are
/// returned as responses (the caller maps them), only transport failures error.
pub struct UreqTransport {
    agent: ureq::Agent,
}

impl UreqTransport {
    pub fn new() -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .timeout_connect(CONNECT_TIMEOUT)
                .timeout(REQUEST_TIMEOUT)
                .redirects(0)
                .build(),
        }
    }
}

impl Default for UreqTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl Transport for UreqTransport {
    fn send(&self, request: &HttpRequest) -> anyhow::Result<HttpResponse> {
        let mut call = self.agent.request(request.method, &request.url);
        for (k, v) in &request.headers {
            call = call.set(k, v);
        }
        let result = match &request.body {
            Some(body) => call.send_bytes(body),
            None => call.call(),
        };
        let response = match result {
            Ok(response) => response,
            Err(ureq::Error::Status(_, response)) => response,
            // The PAT travels only in a header, never in the URL, so the
            // transport error text cannot carry it.
            Err(ureq::Error::Transport(error)) => bail!("HTTP transport error: {error}"),
        };
        let status = response.status();
        let headers = response
            .headers_names()
            .into_iter()
            .filter_map(|name| {
                let value = response.header(&name)?.to_owned();
                Some((name.to_ascii_lowercase(), value))
            })
            .collect();
        let mut body = Vec::new();
        response
            .into_reader()
            .take(MAX_RESPONSE_BYTES as u64 + 1)
            .read_to_end(&mut body)
            .context("failed to read HTTP response body")?;
        if body.len() > MAX_RESPONSE_BYTES {
            bail!("HTTP response exceeds {MAX_RESPONSE_BYTES} bytes");
        }
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

/// Pick the transport for a base URL: `https://` → [`UreqTransport`]; plain
/// `http://` (local fakes, tests) → the std-only [`StdHttpTransport`].
pub fn transport_for(base_url: &str) -> Arc<dyn Transport> {
    if base_url.starts_with("https://") {
        Arc::new(UreqTransport::new())
    } else {
        Arc::new(StdHttpTransport)
    }
}

/// Minimal std-only HTTP/1.1 client (`Connection: close`, plain `http://`),
/// used for local fakes and tests; production `https://` goes through
/// [`UreqTransport`] (see [`transport_for`]).
pub struct StdHttpTransport;

/// Timeouts and size caps for one [`StdHttpTransport`] request.
#[derive(Clone, Copy)]
struct Limits {
    connect: Duration,
    request: Duration,
    max_response: usize,
    max_headers: usize,
}

const DEFAULT_LIMITS: Limits = Limits {
    connect: CONNECT_TIMEOUT,
    request: REQUEST_TIMEOUT,
    max_response: MAX_RESPONSE_BYTES,
    max_headers: MAX_HEADER_BYTES,
};

/// Time left before `deadline`, or a timeout error.
fn time_left(deadline: Instant, limits: &Limits) -> anyhow::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| anyhow!("request timed out after {:?}", limits.request))
}

/// Resolve `addr_str`, waiting at most `budget`. Literal IPs skip DNS.
fn resolve(addr_str: &str, authority: &str, budget: Duration) -> anyhow::Result<SocketAddr> {
    if let Ok(addr) = addr_str.parse::<SocketAddr>() {
        return Ok(addr);
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let target = addr_str.to_owned();
    // The resolver thread is detached and may outlive the timeout; acceptable
    // for this stopgap transport, which `ureq` is expected to replace.
    std::thread::spawn(move || {
        let _ = tx.send(target.to_socket_addrs().map(|mut it| it.next()));
    });
    match rx.recv_timeout(budget) {
        Ok(Ok(Some(addr))) => Ok(addr),
        Ok(Ok(None)) => Err(anyhow!("cannot resolve {authority}")),
        Ok(Err(e)) => Err(anyhow!("cannot resolve {authority}: {e}")),
        Err(_) => Err(anyhow!("timed out resolving {authority}")),
    }
}

impl Transport for StdHttpTransport {
    fn send(&self, request: &HttpRequest) -> anyhow::Result<HttpResponse> {
        send_limited(request, DEFAULT_LIMITS)
    }
}

/// Write all of `data`, re-arming the write timeout from the overall
/// `deadline` before every partial write.
fn write_with_deadline(
    stream: &mut TcpStream,
    mut data: &[u8],
    deadline: Instant,
    limits: &Limits,
) -> anyhow::Result<()> {
    while !data.is_empty() {
        stream.set_write_timeout(Some(time_left(deadline, limits)?))?;
        match stream.write(data) {
            Ok(0) => bail!("write failed: connection accepted zero bytes"),
            Ok(n) => data = &data[n..],
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(anyhow!("write failed: {e}")),
        }
    }
    Ok(())
}

fn send_limited(request: &HttpRequest, limits: Limits) -> anyhow::Result<HttpResponse> {
    let deadline = Instant::now() + limits.request;
    let rest = request.url.strip_prefix("http://").ok_or_else(|| {
        anyhow!("only http:// is supported by the built-in transport (TLS transport pending)")
    })?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let addr_str = if authority.contains(':') {
        authority.to_owned()
    } else {
        format!("{authority}:80")
    };
    let addr = resolve(&addr_str, authority, time_left(deadline, &limits)?)?;
    let mut stream =
        TcpStream::connect_timeout(&addr, limits.connect.min(time_left(deadline, &limits)?))?;

    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
        request.method, path, authority
    );
    for (k, v) in &request.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    let body = request.body.as_deref().unwrap_or(&[]);
    if request.body.is_some() {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    write_with_deadline(&mut stream, head.as_bytes(), deadline, &limits)?;
    write_with_deadline(&mut stream, body, deadline, &limits)?;

    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    let mut headers_done = false;
    loop {
        stream.set_read_timeout(Some(time_left(deadline, &limits)?))?;
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if raw.len() + n > limits.max_response {
                    bail!("response exceeds {} bytes", limits.max_response);
                }
                raw.extend_from_slice(&buf[..n]);
                if !headers_done {
                    let header_end = raw
                        .windows(4)
                        .position(|w| w == b"\r\n\r\n")
                        .map(|i| i + 4);
                    if header_end.unwrap_or(raw.len()) > limits.max_headers {
                        bail!("response headers exceed {} bytes", limits.max_headers);
                    }
                    headers_done = header_end.is_some();
                }
            }
            Err(e) => return Err(anyhow!("read failed: {e}")),
        }
    }
    parse_response(&raw, limits.max_response)
}

fn parse_response(raw: &[u8], max_body: usize) -> anyhow::Result<HttpResponse> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow!("malformed HTTP response (no header terminator)"))?;
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| anyhow!("malformed HTTP status line"))?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let body = &raw[split + 4..];
    let chunked = headers
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked"));
    let body = if chunked {
        dechunk(body, max_body)?
    } else {
        body.to_vec()
    };
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

fn dechunk(mut data: &[u8], max_out: usize) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let eol = data
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| anyhow!("malformed chunked body"))?;
        let size_str = String::from_utf8_lossy(&data[..eol]);
        let size = usize::from_str_radix(size_str.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| anyhow!("malformed chunk size"))?;
        data = &data[eol + 2..];
        if size == 0 {
            return Ok(out);
        }
        let framed = size
            .checked_add(2)
            .ok_or_else(|| anyhow!("chunk size overflows"))?;
        if out.len().saturating_add(size) > max_out {
            bail!("response exceeds {max_out} bytes");
        }
        if data.len() < framed {
            bail!("truncated chunked body");
        }
        out.extend_from_slice(&data[..size]);
        data = &data[framed..];
    }
}

// ---------------------------------------------------------------------------
// Tracker
// ---------------------------------------------------------------------------

pub struct AzdoTracker {
    name: String,
    /// `https://dev.azure.com/{org}` unless overridden (tests).
    base_url: String,
    project: String,
    pat: String,
    transport: Arc<dyn Transport>,
    /// Central tracker entry (status map, default work-item type); `None` for
    /// trackers built with [`AzdoTracker::new`] alone, which cannot transition.
    config: Option<TrackerConfig>,
}

impl AzdoTracker {
    /// Build from a `[[trackers]]` entry (`organization`, `project`, optional
    /// `base_url` in its `extra` settings) and an already-resolved PAT.
    pub fn from_config(config: &TrackerConfig, pat: String) -> anyhow::Result<Self> {
        let project = config
            .extra_str("project")
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow!("azdo tracker '{}' needs a `project` setting", config.name))?
            .to_owned();
        let base_url = match config.extra_str("base_url") {
            Some(url) => url.trim_end_matches('/').to_owned(),
            None => {
                let org = config
                    .extra_str("organization")
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| {
                        anyhow!("azdo tracker '{}' needs an `organization` setting", config.name)
                    })?;
                format!("https://dev.azure.com/{}", url_encode(org))
            }
        };
        let transport = transport_for(&base_url);
        Ok(Self::new(config.name.clone(), base_url, project, pat, transport)
            .with_config(config.clone()))
    }

    /// Attach the tracker entry that drives `transition` (status map) and `create`.
    pub fn with_config(mut self, config: TrackerConfig) -> Self {
        self.config = Some(config);
        self
    }

    pub fn new(
        name: String,
        base_url: String,
        project: String,
        pat: String,
        transport: Arc<dyn Transport>,
    ) -> Self {
        Self {
            name,
            base_url,
            project,
            pat,
            transport,
            config: None,
        }
    }

    fn api(&self, path: &str, extra_query: &str) -> String {
        format!(
            "{}/{}/_apis/wit/{}?{}api-version={API_VERSION}",
            self.base_url,
            url_encode(&self.project),
            path,
            extra_query
        )
    }

    fn request(&self, method: &'static str, url: String, body: Option<Value>) -> HttpRequest {
        self.request_as(method, url, body, "application/json")
    }

    fn request_as(
        &self,
        method: &'static str,
        url: String,
        body: Option<Value>,
        content_type: &str,
    ) -> HttpRequest {
        let mut headers = vec![
            (
                "Authorization".to_owned(),
                format!("Basic {}", base64(format!(":{}", self.pat).as_bytes())),
            ),
            ("Accept".to_owned(), "application/json".to_owned()),
        ];
        if body.is_some() {
            headers.push(("Content-Type".to_owned(), content_type.to_owned()));
        }
        HttpRequest {
            method,
            url,
            headers,
            body: body.map(|b| b.to_string().into_bytes()),
        }
    }

    /// Send with bounded retry on 429/503, redact the PAT from any error.
    async fn call(&self, request: HttpRequest) -> Result<HttpResponse> {
        let transport = self.transport.clone();
        let pat = self.pat.clone();
        let result = tokio::task::spawn_blocking(move || send_with_retry(&*transport, &request))
            .await
            .map_err(|e| anyhow!("http task failed: {e}"))?;
        result.map_err(|e| TrackerError::Other(anyhow!(redact(&format!("{e:#}"), &pat))))
    }

    async fn call_json(&self, request: HttpRequest, what: &str) -> Result<Value> {
        let response = self.call(request).await?;
        if response.status == 404 {
            return Err(TrackerError::NotFound(what.to_owned()));
        }
        if !(200..300).contains(&response.status) {
            let snippet: String = String::from_utf8_lossy(&response.body)
                .chars()
                .take(300)
                .collect();
            return Err(TrackerError::Other(anyhow!(redact(
                &format!("azdo {what}: HTTP {}: {snippet}", response.status),
                &self.pat
            ))));
        }
        serde_json::from_slice(&response.body)
            .with_context(|| format!("azdo {what}: response is not JSON"))
            .map_err(TrackerError::from)
    }

    async fn fetch_batch(&self, ids: &[u64]) -> Result<Vec<RemoteIssue>> {
        let mut issues = Vec::new();
        for chunk in ids.chunks(BATCH_SIZE) {
            let body = json!({ "ids": chunk, "fields": FIELDS });
            let request = self.request("POST", self.api("workitemsbatch", ""), Some(body));
            let value = self.call_json(request, "workitemsbatch").await?;
            let items = value
                .get("value")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("azdo workitemsbatch: missing `value` array"))?;
            for item in items {
                issues.push(self.map_item(item)?);
            }
        }
        Ok(issues)
    }

    fn work_item_url(&self, id: &str, suffix: &str) -> String {
        format!(
            "{}/{}/_apis/wit/workitems/{id}{suffix}",
            self.base_url,
            url_encode(&self.project)
        )
    }

    /// JSON Patch against one work item. 409/412 (or a failed `test` op, which
    /// Azure DevOps reports as 400) become [`TrackerError::Conflict`].
    async fn patch_item(
        &self,
        id: &str,
        ops: Vec<Value>,
        expected_rev: Option<u64>,
    ) -> Result<RemoteIssue> {
        let url = format!("{}?api-version={API_VERSION}", self.work_item_url(id, ""));
        let request = self.request_as("PATCH", url, Some(Value::Array(ops)), PATCH_CONTENT_TYPE);
        let response = self.call(request).await?;
        let body = String::from_utf8_lossy(&response.body).into_owned();
        let lower = body.to_ascii_lowercase();
        let test_failed = response.status == 400
            && (lower.contains("testoperation") || lower.contains("jsonpatchtest"));
        if matches!(response.status, 409 | 412) || test_failed {
            return Err(TrackerError::Conflict {
                id: id.to_owned(),
                expected: expected_rev,
                actual: None,
            });
        }
        self.parse_item_response(response, &format!("work item {id}"))
    }

    fn parse_item_response(&self, response: HttpResponse, what: &str) -> Result<RemoteIssue> {
        if response.status == 404 {
            return Err(TrackerError::NotFound(what.to_owned()));
        }
        if !(200..300).contains(&response.status) {
            return Err(self.http_error(what, &response));
        }
        let value: Value = serde_json::from_slice(&response.body)
            .with_context(|| format!("azdo {what}: response is not JSON"))?;
        Ok(self.map_item(&value)?)
    }

    fn http_error(&self, what: &str, response: &HttpResponse) -> TrackerError {
        let snippet: String = String::from_utf8_lossy(&response.body)
            .chars()
            .take(300)
            .collect();
        TrackerError::Other(anyhow!(redact(
            &format!("azdo {what}: HTTP {}: {snippet}", response.status),
            &self.pat
        )))
    }

    /// State names of a work-item type, for `doctor`/`setup` validation of a
    /// `status_map`.
    pub async fn work_item_states(&self, work_item_type: &str) -> Result<Vec<String>> {
        let url = self.api(
            &format!("workitemtypes/{}/states", url_encode(work_item_type)),
            "",
        );
        let value = self
            .call_json(
                self.request("GET", url, None),
                &format!("work item type {work_item_type}"),
            )
            .await?;
        let states = value
            .get("value")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("azdo workitemtypes states: missing `value` array"))?;
        Ok(states
            .iter()
            .filter_map(|s| s.get("name").and_then(Value::as_str).map(str::to_owned))
            .collect())
    }

    fn map_item(&self, item: &Value) -> anyhow::Result<RemoteIssue> {
        let id = item
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("work item without numeric `id`"))?;
        let empty = serde_json::Map::new();
        let fields = item
            .get("fields")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let text = |key: &str| fields.get(key).and_then(Value::as_str).unwrap_or("");
        let rev = item
            .get("rev")
            .and_then(Value::as_u64)
            .or_else(|| fields.get("System.Rev").and_then(Value::as_u64))
            .unwrap_or(0);

        let mut body = html_to_markdown(text("System.Description"));
        let criteria = html_to_markdown(text("Microsoft.VSTS.Common.AcceptanceCriteria"));
        if !criteria.is_empty() {
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            body.push_str("## Acceptance criteria\n\n");
            body.push_str(&criteria);
        }

        let assignee = match fields.get("System.AssignedTo") {
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            Some(Value::Object(o)) => o
                .get("uniqueName")
                .or_else(|| o.get("displayName"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            _ => None,
        };
        let tags = text("System.Tags")
            .split(';')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
            .collect();

        Ok(RemoteIssue {
            id: id.to_string(),
            url: format!(
                "{}/{}/_workitems/edit/{id}",
                self.base_url,
                url_encode(&self.project)
            ),
            rev,
            title: text("System.Title").to_owned(),
            body_markdown: body,
            remote_state: text("System.State").to_owned(),
            assignee,
            tags,
            fields: fields
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<BTreeMap<_, _>>(),
        })
    }
}

#[async_trait]
impl IssueTracker for AzdoTracker {
    fn name(&self) -> &str {
        &self.name
    }

    async fn query(&self, filter: &TrackerFilter) -> Result<Vec<RemoteIssue>> {
        let wiql = build_wiql(filter);
        let request = self.request("POST", self.api("wiql", ""), Some(json!({ "query": wiql })));
        let value = self.call_json(request, "wiql").await?;
        let ids: Vec<u64> = value
            .get("workItems")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|i| i.get("id").and_then(Value::as_u64))
                    .collect()
            })
            .unwrap_or_default();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        self.fetch_batch(&ids).await
    }

    async fn get(&self, id: &str) -> Result<RemoteIssue> {
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(TrackerError::NotFound(id.to_owned()));
        }
        let fields = format!("fields={}&", FIELDS.join(","));
        let request = self.request("GET", self.api(&format!("workitems/{id}"), &fields), None);
        let value = self.call_json(request, &format!("work item {id}")).await?;
        Ok(self.map_item(&value)?)
    }

    async fn create(&self, draft: &IssueDraft) -> Result<RemoteIssue> {
        let work_item_type = draft
            .work_item_type
            .as_deref()
            .or_else(|| {
                self.config
                    .as_ref()
                    .and_then(|c| c.extra_str("work_item_type"))
            })
            .filter(|t| !t.trim().is_empty())
            .unwrap_or(DEFAULT_WORK_ITEM_TYPE);
        let mut ops = vec![
            json!({"op": "add", "path": "/fields/System.Title", "value": draft.title}),
            json!({"op": "add", "path": "/fields/System.Description",
                   "value": markdown_to_html(&draft.body_markdown)}),
        ];
        if !draft.tags.is_empty() {
            ops.push(json!({"op": "add", "path": "/fields/System.Tags",
                            "value": draft.tags.join("; ")}));
        }
        if let Some(who) = draft.assignee.as_deref().filter(|w| !w.is_empty()) {
            ops.push(json!({"op": "add", "path": "/fields/System.AssignedTo", "value": who}));
        }
        // `$` is the literal path marker before the type name.
        let url = format!(
            "{}/{}/_apis/wit/workitems/${}?api-version={API_VERSION}",
            self.base_url,
            url_encode(&self.project),
            url_encode(work_item_type)
        );
        let request = self.request_as("POST", url, Some(Value::Array(ops)), PATCH_CONTENT_TYPE);
        let response = self.call(request).await?;
        self.parse_item_response(response, "create work item")
    }

    async fn transition(
        &self,
        id: &str,
        to: TaskStatus,
        expected_rev: Option<u64>,
    ) -> Result<RemoteIssue> {
        let config = self.config.as_ref().ok_or_else(|| {
            TrackerError::Other(anyhow!("azdo tracker '{}' has no config for transition", self.name))
        })?;
        let mapped = map_status(config, to);
        // Without a caller rev we pin the PATCH to the rev we just read, so a
        // concurrent human tag edit fails the test op instead of being
        // overwritten by our stale-snapshot tags. Re-read and retry once.
        let attempts = if expected_rev.is_some() { 1 } else { 2 };
        let mut attempt = 0;
        loop {
            attempt += 1;
            let current = self.get(id).await?;
            if let Some(expected) = expected_rev
                && current.rev != expected
            {
                return Err(TrackerError::Conflict {
                    id: id.to_owned(),
                    expected: Some(expected),
                    actual: Some(current.rev),
                });
            }
            // Only `varda:*` helper tags are touched; user tags keep their order.
            let mut tags: Vec<String> = current
                .tags
                .iter()
                .filter(|t| !mapped.remove_tags.iter().any(|r| r.eq_ignore_ascii_case(t)))
                .cloned()
                .collect();
            for tag in &mapped.add_tags {
                if !tags.iter().any(|t| t.eq_ignore_ascii_case(tag)) {
                    tags.push(tag.clone());
                }
            }
            let mut ops = vec![
                json!({"op": "test", "path": "/rev", "value": expected_rev.unwrap_or(current.rev)}),
            ];
            if let Some(state) = &mapped.state {
                ops.push(json!({"op": "add", "path": "/fields/System.State", "value": state}));
            }
            if tags != current.tags {
                ops.push(json!({"op": "add", "path": "/fields/System.Tags", "value": tags.join("; ")}));
            }
            if ops.iter().all(|op| op["op"] == "test") {
                return Ok(current);
            }
            match self.patch_item(id, ops, expected_rev).await {
                Err(e) if e.is_conflict() && attempt < attempts => continue,
                result => return result,
            }
        }
    }

    async fn comment(&self, id: &str, markdown: &str) -> Result<()> {
        check_id(id)?;
        let markdown = cap_markdown(markdown);
        let base = format!(
            "{}/{}/_apis/wit/workItems/{id}/comments",
            self.base_url,
            url_encode(&self.project)
        );
        let md_url = format!("{base}?format=markdown&api-version={COMMENT_API_VERSION}");
        let body = json!({ "text": markdown });
        let response = self.call(self.request("POST", md_url, Some(body))).await?;
        let response = if response.status == 400 {
            // Org rejects `format=markdown`: post HTML (the default format).
            let url = format!("{base}?api-version={COMMENT_API_VERSION}");
            let body = json!({ "text": capped_html(&markdown) });
            self.call(self.request("POST", url, Some(body))).await?
        } else {
            response
        };
        if response.status == 404 {
            return Err(TrackerError::NotFound(id.to_owned()));
        }
        if !(200..300).contains(&response.status) {
            return Err(self.http_error(&format!("comment on {id}"), &response));
        }
        Ok(())
    }

    async fn assign(&self, id: &str, who: Option<&str>) -> Result<()> {
        check_id(id)?;
        let op = match who {
            Some(who) => json!({"op": "add", "path": "/fields/System.AssignedTo", "value": who}),
            None => json!({"op": "remove", "path": "/fields/System.AssignedTo"}),
        };
        self.patch_item(id, vec![op], None).await.map(|_| ())
    }

    async fn check_auth(&self) -> Result<()> {
        let url = format!("{}/_apis/connectionData?api-version={API_VERSION}-preview.1", self.base_url);
        let request = self.request("GET", url, None);
        self.call_json(request, "connectionData").await.map(|_| ())
    }
}

fn check_id(id: &str) -> Result<()> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(TrackerError::NotFound(id.to_owned()));
    }
    Ok(())
}

const TRUNCATION_NOTICE: &str = "\n\n_[truncated by Varda: comment exceeded 100 KB]_";

/// Truncate to [`MAX_COMMENT_BYTES`] on a char boundary; the result, notice
/// included, never exceeds the cap.
fn cap_markdown(markdown: &str) -> String {
    cap_markdown_to(markdown, MAX_COMMENT_BYTES)
}

fn cap_markdown_to(markdown: &str, limit: usize) -> String {
    if markdown.len() <= limit {
        return markdown.to_owned();
    }
    let mut end = limit.saturating_sub(TRUNCATION_NOTICE.len());
    while !markdown.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{TRUNCATION_NOTICE}", &markdown[..end])
}

/// Render the HTML fallback payload within [`MAX_COMMENT_BYTES`]. Escaping can
/// expand the text (`"` becomes `&quot;`), so shrink the markdown budget until
/// the rendered HTML fits: truncating markdown never splits an entity or tag.
fn capped_html(markdown: &str) -> String {
    let mut limit = MAX_COMMENT_BYTES;
    loop {
        let html = markdown_to_html(&cap_markdown_to(markdown, limit));
        if html.len() <= MAX_COMMENT_BYTES || limit <= TRUNCATION_NOTICE.len() * 2 {
            return html;
        }
        limit = limit * 3 / 4;
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Minimal markdown → HTML for the no-`format=markdown` fallback: everything
/// is escaped; fenced blocks become `<pre><code>`, blank-line separated runs
/// become `<p>` with `<br>` between lines. Inline markup is left as text.
fn markdown_to_html(markdown: &str) -> String {
    let mut out = String::new();
    let mut para: Vec<String> = Vec::new();
    let mut code: Option<Vec<&str>> = None;
    fn flush(out: &mut String, para: &mut Vec<String>) {
        if !para.is_empty() {
            out.push_str("<p>");
            out.push_str(&para.join("<br>"));
            out.push_str("</p>");
            para.clear();
        }
    }
    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            match code.take() {
                Some(lines) => {
                    out.push_str("<pre><code>");
                    out.push_str(&html_escape(&lines.join("\n")));
                    out.push_str("</code></pre>");
                }
                None => {
                    flush(&mut out, &mut para);
                    code = Some(Vec::new());
                }
            }
        } else if let Some(lines) = code.as_mut() {
            lines.push(line);
        } else if line.trim().is_empty() {
            flush(&mut out, &mut para);
        } else {
            para.push(html_escape(line));
        }
    }
    if let Some(lines) = code {
        // Unterminated fence: still render what we have.
        out.push_str("<pre><code>");
        out.push_str(&html_escape(&lines.join("\n")));
        out.push_str("</code></pre>");
    }
    flush(&mut out, &mut para);
    out
}

// ---------------------------------------------------------------------------
// Retry, WIQL, small helpers
// ---------------------------------------------------------------------------

/// At most [`MAX_RETRIES`] retries on 429/503, honouring `Retry-After`
/// (seconds), never waiting more than [`MAX_RETRY_WAIT`] in total.
fn send_with_retry(transport: &dyn Transport, request: &HttpRequest) -> anyhow::Result<HttpResponse> {
    let mut waited = Duration::ZERO;
    let mut attempt = 0;
    loop {
        let response = transport.send(request)?;
        if response.status != 429 && response.status != 503 {
            return Ok(response);
        }
        let wait = Duration::from_secs(
            response
                .header("retry-after")
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(1),
        );
        if attempt >= MAX_RETRIES || waited + wait > MAX_RETRY_WAIT {
            bail!(
                "rate limited (HTTP {}) after {attempt} retr{}; giving up",
                response.status,
                if attempt == 1 { "y" } else { "ies" }
            );
        }
        std::thread::sleep(wait);
        waited += wait;
        attempt += 1;
    }
}

fn wiql_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// WIQL for a filter; `filter.wiql` wins verbatim when set.
pub fn build_wiql(filter: &TrackerFilter) -> String {
    if let Some(wiql) = filter.wiql.as_deref().filter(|w| !w.trim().is_empty()) {
        return wiql.to_owned();
    }
    let mut clauses = vec!["[System.TeamProject] = @project".to_owned()];
    for tag in &filter.tags {
        clauses.push(format!("[System.Tags] CONTAINS {}", wiql_quote(tag)));
    }
    if !filter.states.is_empty() {
        let states: Vec<String> = filter.states.iter().map(|s| wiql_quote(s)).collect();
        clauses.push(format!("[System.State] IN ({})", states.join(", ")));
    }
    if let Some(area) = filter.area_path.as_deref().filter(|a| !a.is_empty()) {
        clauses.push(format!("[System.AreaPath] UNDER {}", wiql_quote(area)));
    }
    format!(
        "SELECT [System.Id] FROM WorkItems WHERE {} ORDER BY [System.ChangedDate] DESC",
        clauses.join(" AND ")
    )
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in input.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

fn redact(text: &str, secret: &str) -> String {
    let mut out = text.to_owned();
    if !secret.is_empty() {
        out = out.replace(secret, "[redacted]");
        out = out.replace(&base64(format!(":{secret}").as_bytes()), "[redacted]");
    }
    out
}

// ---------------------------------------------------------------------------
// HTML -> markdown
// ---------------------------------------------------------------------------

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let decoded = rest.find(';').filter(|&e| e <= 10).and_then(|e| {
            let name = &rest[1..e];
            let ch = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                _ => name
                    .strip_prefix('#')
                    .and_then(|n| match n.strip_prefix(['x', 'X']) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok(),
                        None => n.parse().ok(),
                    })
                    .and_then(char::from_u32),
            };
            ch.map(|c| (c, e))
        });
        match decoded {
            Some((c, e)) => {
                out.push(c);
                rest = &rest[e + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn tag_attr(tag: &str, attr: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    for quote in ['"', '\''] {
        if let Some(i) = lower.find(&format!("{attr}={quote}")) {
            let start = i + attr.len() + 2;
            let end = tag[start..].find(quote)? + start;
            return Some(decode_entities(&tag[start..end]));
        }
    }
    None
}

#[derive(Default)]
struct Md {
    out: String,
    /// Active table cell buffer, if inside a cell.
    cell: Option<String>,
    row: Vec<String>,
    rows: Vec<Vec<String>>,
    lists: Vec<(bool, usize)>,
    links: Vec<String>,
    pre: bool,
}

impl Md {
    fn buf(&mut self) -> &mut String {
        self.cell.as_mut().unwrap_or(&mut self.out)
    }

    fn push(&mut self, s: &str) {
        self.buf().push_str(s);
    }

    /// Start a new block: ensure a blank line before it.
    fn block(&mut self) {
        if self.cell.is_some() {
            self.push(" ");
            return;
        }
        let trimmed = self.out.trim_end_matches([' ', '\t']).len();
        self.out.truncate(trimmed);
        if self.out.is_empty() || self.out.ends_with("\n\n") {
            return;
        }
        self.out.push_str(if self.out.ends_with('\n') { "\n" } else { "\n\n" });
    }

    fn line(&mut self) {
        if self.cell.is_some() {
            self.push(" ");
        } else if !self.out.is_empty() && !self.out.ends_with('\n') {
            self.out.push('\n');
        }
    }

    fn text(&mut self, raw: &str) {
        let decoded = decode_entities(raw);
        if self.pre {
            self.push(&decoded);
            return;
        }
        for ch in decoded.chars() {
            if ch.is_whitespace() {
                let buf = self.buf();
                if !buf.is_empty() && !buf.ends_with([' ', '\n']) {
                    buf.push(' ');
                }
            } else {
                self.buf().push(ch);
            }
        }
    }

    fn finish_table(&mut self) {
        let rows = std::mem::take(&mut self.rows);
        if rows.is_empty() {
            return;
        }
        let width = rows.iter().map(Vec::len).max().unwrap_or(0).max(1);
        self.block();
        for (i, row) in rows.iter().enumerate() {
            let mut cells: Vec<String> = row.iter().map(|c| c.replace('|', "\\|")).collect();
            cells.resize(width, String::new());
            self.out.push_str(&format!("| {} |\n", cells.join(" | ")));
            if i == 0 {
                self.out
                    .push_str(&format!("|{}\n", " --- |".repeat(width)));
            }
        }
        self.out.push('\n');
    }

    fn tag(&mut self, raw: &str) {
        let inner = raw.trim_start_matches('<').trim_end_matches('>').trim();
        let closing = inner.starts_with('/');
        let name: String = inner
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        match (name.as_str(), closing) {
            ("br", _) => self.push(if self.pre { "\n" } else { "  \n" }),
            ("p" | "div" | "blockquote", _) => self.block(),
            ("h1" | "h2" | "h3" | "h4" | "h5" | "h6", false) => {
                self.block();
                let level = name[1..].parse::<usize>().unwrap_or(1);
                self.push(&format!("{} ", "#".repeat(level)));
            }
            ("h1" | "h2" | "h3" | "h4" | "h5" | "h6", true) => self.block(),
            ("ul", false) => self.lists.push((false, 0)),
            ("ol", false) => self.lists.push((true, 0)),
            ("ul" | "ol", true) => {
                self.lists.pop();
                if self.lists.is_empty() {
                    self.block();
                }
            }
            ("li", false) => {
                self.line();
                let depth = self.lists.len().saturating_sub(1);
                let marker = match self.lists.last_mut() {
                    Some((true, n)) => {
                        *n += 1;
                        format!("{n}.")
                    }
                    _ => "-".to_owned(),
                };
                self.push(&format!("{}{marker} ", "  ".repeat(depth)));
            }
            ("li", true) => self.line(),
            ("b" | "strong", _) => self.push("**"),
            ("i" | "em", _) => self.push("*"),
            ("code", _) if !self.pre => self.push("`"),
            ("pre", false) => {
                self.block();
                self.push("```\n");
                self.pre = true;
            }
            ("pre", true) => {
                self.pre = false;
                self.line();
                self.push("```");
                self.block();
            }
            ("a", false) => match tag_attr(inner, "href") {
                Some(href) => {
                    self.links.push(href);
                    self.push("[");
                }
                None => self.links.push(String::new()),
            },
            ("a", true) => {
                if let Some(href) = self.links.pop().filter(|h| !h.is_empty()) {
                    self.push(&format!("]({href})"));
                }
            }
            ("img", _) => self.push("[image omitted]"),
            ("table", false) => {
                self.block();
                self.rows.clear();
            }
            ("table", true) => self.finish_table(),
            ("tr", false) => self.row.clear(),
            ("tr", true) => {
                let row = std::mem::take(&mut self.row);
                if !row.is_empty() {
                    self.rows.push(row);
                }
            }
            ("td" | "th", false) => self.cell = Some(String::new()),
            ("td" | "th", true) => {
                if let Some(cell) = self.cell.take() {
                    self.row.push(cell.trim().to_owned());
                }
            }
            _ => {}
        }
    }
}

/// Convert an Azure DevOps HTML field to markdown. Handles paragraphs, line
/// breaks, headings, bold/italic/code, lists, links, tables and `<pre>`;
/// images become `[image omitted]`; unknown tags are dropped but their text kept.
pub fn html_to_markdown(html: &str) -> String {
    let mut md = Md::default();
    let mut rest = html;
    while !rest.is_empty() {
        match rest.find('<') {
            None => {
                md.text(rest);
                break;
            }
            Some(0) => {
                if rest.starts_with("<!--") {
                    rest = rest.find("-->").map_or("", |e| &rest[e + 3..]);
                    continue;
                }
                match rest.find('>') {
                    Some(e) => {
                        md.tag(&rest[..=e]);
                        rest = &rest[e + 1..];
                    }
                    None => {
                        md.text(rest);
                        break;
                    }
                }
            }
            Some(i) => {
                md.text(&rest[..i]);
                rest = &rest[i..];
            }
        }
    }
    md.finish_table();
    let mut cleaned = String::new();
    let mut blank = 0;
    for line in md.out.lines() {
        let line = line.trim_end_matches([' ', '\t']);
        // Keep markdown hard breaks (`  \n`) out: trailing spaces are noise here.
        if line.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        cleaned.push_str(line);
        cleaned.push('\n');
    }
    cleaned.trim().to_owned()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::sync::Mutex;

    use super::*;

    const WIQL_FIXTURE: &str = include_str!("../../testdata/azdo/wiql.json");
    const BATCH_FIXTURE: &str = include_str!("../../testdata/azdo/workitemsbatch.json");

    /// Serve `responses` (status, extra headers, body) one per connection and
    /// record each raw request.
    fn fake_server(
        responses: Vec<(u16, Vec<(&'static str, &'static str)>, String)>,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for (status, headers, body) in responses {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut raw = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let n = stream.read(&mut buf).unwrap_or(0);
                    raw.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&raw).into_owned();
                    if let Some(h) = text.find("\r\n\r\n") {
                        let len = text[..h]
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                            .unwrap_or(0);
                        if raw.len() >= h + 4 + len {
                            break;
                        }
                    }
                    if n == 0 {
                        break;
                    }
                }
                log.lock().unwrap().push(String::from_utf8_lossy(&raw).into_owned());
                let mut head = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
                for (k, v) in headers {
                    head.push_str(&format!("{k}: {v}\r\n"));
                }
                let _ = stream.write_all(format!("{head}\r\n{body}").as_bytes());
            }
        });
        (base, seen)
    }

    fn tracker(base: &str) -> AzdoTracker {
        AzdoTracker::new(
            "azdo".to_owned(),
            base.to_owned(),
            "My Project".to_owned(),
            "s3cret-pat".to_owned(),
            Arc::new(StdHttpTransport),
        )
    }

    /// Real TLS handshake against dev.azure.com, unauthenticated — any HTTP
    /// status proves rustls + webpki roots work. `VARDA_AZDO_LIVE_TLS=1`.
    #[test]
    #[ignore]
    fn live_tls_handshake() {
        if std::env::var("VARDA_AZDO_LIVE_TLS").as_deref() != Ok("1") {
            return;
        }
        let response = transport_for("https://dev.azure.com")
            .send(&HttpRequest {
                method: "GET",
                url: "https://dev.azure.com/_apis/connectionData".to_owned(),
                headers: vec![],
                body: None,
            })
            .expect("TLS round trip to dev.azure.com");
        println!("dev.azure.com answered HTTP {}", response.status);
        assert!((100..600).contains(&response.status));
    }

    #[test]
    fn ureq_transport_returns_non_2xx_as_response_and_lowercases_headers() {
        let (base, seen) = fake_server(vec![
            (200, vec![("X-Echo", "a")], "{\"ok\":true}".to_owned()),
            (429, vec![("Retry-After", "1")], "slow down".to_owned()),
            (302, vec![("Location", "https://login.example/")], String::new()),
        ]);
        let t = UreqTransport::new();
        let req = |method: &'static str, body: Option<Vec<u8>>| HttpRequest {
            method,
            url: format!("{base}/x"),
            headers: vec![("Authorization".to_owned(), "Basic abc".to_owned())],
            body,
        };

        let ok = t.send(&req("POST", Some(b"{\"q\":1}".to_vec()))).unwrap();
        assert_eq!(ok.status, 200);
        assert_eq!(ok.header("x-echo"), Some("a"));
        assert_eq!(ok.body, b"{\"ok\":true}");

        let limited = t.send(&req("GET", None)).unwrap();
        assert_eq!(limited.status, 429);
        assert_eq!(limited.header("retry-after"), Some("1"));

        // Redirects are surfaced, not followed (a bad PAT redirects to sign-in).
        let redirect = t.send(&req("GET", None)).unwrap();
        assert_eq!(redirect.status, 302);

        let first = seen.lock().unwrap()[0].to_ascii_lowercase();
        assert!(first.starts_with("post /x"), "{first}");
        assert!(first.contains("authorization: basic abc"), "{first}");
        assert!(first.ends_with("{\"q\":1}"), "{first}");
    }

    #[test]
    fn dechunk_huge_chunk_size_errors_without_panic() {
        let err = dechunk(b"FFFFFFFFFFFFFFFF\r\nabc\r\n0\r\n\r\n", 1024).unwrap_err();
        assert!(err.to_string().contains("overflow"), "{err}");
        let err = dechunk(b"7FFFFFFFFFFFFFFF\r\nabc", 1024).unwrap_err();
        assert!(err.to_string().contains("exceeds"), "{err}");
        assert_eq!(dechunk(b"3\r\nabc\r\n0\r\n\r\n", 1024).unwrap(), b"abc");
    }

    #[test]
    fn over_cap_response_is_rejected() {
        let (base, _) = fake_server(vec![(200, vec![], "x".repeat(5000))]);
        let req = HttpRequest { method: "GET", url: format!("{base}/"), headers: vec![], body: None };
        let limits = Limits { max_response: 1000, ..DEFAULT_LIMITS };
        let err = send_limited(&req, limits).unwrap_err();
        assert!(err.to_string().contains("exceeds"), "{err}");
    }

    #[test]
    fn over_cap_headers_in_single_read_are_rejected() {
        const LONG: &str = "yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy";
        let (base, _) = fake_server(vec![(200, vec![("X-Long", LONG)], "ok".to_owned())]);
        let req = HttpRequest { method: "GET", url: format!("{base}/"), headers: vec![], body: None };
        let limits = Limits { max_headers: 128, ..DEFAULT_LIMITS };
        let err = send_limited(&req, limits).unwrap_err();
        assert!(err.to_string().contains("headers exceed"), "{err}");
    }

    #[test]
    fn silent_server_hits_overall_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let req = HttpRequest {
            method: "GET",
            url: format!("http://{}/", listener.local_addr().unwrap()),
            headers: vec![],
            body: None,
        };
        let limits = Limits { request: Duration::from_millis(300), ..DEFAULT_LIMITS };
        let start = Instant::now();
        let err = send_limited(&req, limits).unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(err.to_string().contains("timed out") || err.to_string().contains("read failed"), "{err}");
        drop(listener);
    }

    #[tokio::test]
    async fn query_runs_wiql_then_batch_and_maps_items() {
        let (base, seen) = fake_server(vec![
            (200, vec![], WIQL_FIXTURE.to_owned()),
            (200, vec![], BATCH_FIXTURE.to_owned()),
        ]);
        let filter = TrackerFilter {
            tags: vec!["varda".to_owned()],
            states: vec!["New".to_owned()],
            ..TrackerFilter::default()
        };
        let issues = tracker(&base).query(&filter).await.unwrap();
        assert_eq!(issues.len(), 2);

        let a = &issues[0];
        assert_eq!(a.id, "101");
        assert_eq!(a.rev, 7);
        assert_eq!(a.url, format!("{base}/My%20Project/_workitems/edit/101"));
        assert_eq!(a.tags, vec!["varda", "backend", "varda:ready"]);
        assert_eq!(a.assignee.as_deref(), Some("ada@acme.example"));
        assert_eq!(a.remote_state, "New");
        assert!(a.body_markdown.contains("**must** retry on *429*"), "{}", a.body_markdown);
        assert!(a.body_markdown.contains("- Honour `Retry-After`"));
        assert!(a.body_markdown.contains("- See [the spec](https://example.com/spec)"));
        assert!(a.body_markdown.contains("1. first\n2. second"));
        assert!(a.body_markdown.contains("| Code | Action |\n| --- | --- |\n| 429 | retry |"));
        assert!(a.body_markdown.contains("[image omitted]"));
        assert!(!a.body_markdown.contains("<img"));
        assert!(a.body_markdown.ends_with("## Acceptance criteria\n\n- Gives up after 2 retries"));

        let b = &issues[1];
        assert_eq!(b.body_markdown, "");
        assert!(b.tags.is_empty());
        assert_eq!(b.assignee, None);

        let requests = seen.lock().unwrap();
        assert!(requests[0].starts_with("POST /My%20Project/_apis/wit/wiql?api-version=7.1 "));
        assert!(requests[0].contains("[System.Tags] CONTAINS 'varda'"));
        assert!(requests[0].contains("[System.State] IN ('New')"));
        // base64(":s3cret-pat")
        assert!(requests[0].contains("Authorization: Basic OnMzY3JldC1wYXQ="), "{}", requests[0]);
        assert!(requests[1].starts_with("POST /My%20Project/_apis/wit/workitemsbatch?api-version=7.1 "));
        assert!(requests[1].contains("\"ids\":[101,102]"));
        assert!(requests[1].contains("Microsoft.VSTS.Common.AcceptanceCriteria"));
    }

    #[tokio::test]
    async fn empty_wiql_result_skips_batch() {
        let (base, seen) = fake_server(vec![(200, vec![], r#"{"workItems":[]}"#.to_owned())]);
        let issues = tracker(&base).query(&TrackerFilter::default()).await.unwrap();
        assert!(issues.is_empty());
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn get_fetches_one_item_and_404_is_not_found() {
        let item = serde_json::from_str::<Value>(BATCH_FIXTURE).unwrap()["value"][1].to_string();
        let (base, seen) = fake_server(vec![(200, vec![], item), (404, vec![], "{}".to_owned())]);
        let t = tracker(&base);
        let issue = t.get("102").await.unwrap();
        assert_eq!(issue.title, "Bare item");
        assert!(seen.lock().unwrap()[0].starts_with("GET /My%20Project/_apis/wit/workitems/102?fields="));
        assert!(matches!(t.get("999").await, Err(TrackerError::NotFound(_))));
        assert!(matches!(t.get("../x").await, Err(TrackerError::NotFound(_))));
    }

    #[tokio::test]
    async fn rate_limit_honours_retry_after_then_succeeds() {
        let (base, seen) = fake_server(vec![
            (429, vec![("Retry-After", "0")], String::new()),
            (503, vec![("Retry-After", "0")], String::new()),
            (200, vec![], r#"{"workItems":[]}"#.to_owned()),
        ]);
        tracker(&base).query(&TrackerFilter::default()).await.unwrap();
        assert_eq!(seen.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn rate_limit_gives_up_after_two_retries_and_when_wait_too_long() {
        let (base, seen) = fake_server(vec![
            (429, vec![("Retry-After", "0")], String::new()),
            (429, vec![("Retry-After", "0")], String::new()),
            (429, vec![("Retry-After", "0")], String::new()),
        ]);
        let err = tracker(&base).query(&TrackerFilter::default()).await.unwrap_err();
        assert!(err.to_string().contains("rate limited"), "{err}");
        assert_eq!(seen.lock().unwrap().len(), 3);

        let (base, seen) = fake_server(vec![(429, vec![("Retry-After", "31")], String::new())]);
        let err = tracker(&base).query(&TrackerFilter::default()).await.unwrap_err();
        assert!(err.to_string().contains("rate limited"), "{err}");
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn errors_never_contain_the_pat() {
        let (base, _) = fake_server(vec![(401, vec![], "bad token s3cret-pat".to_owned())]);
        let err = tracker(&base).query(&TrackerFilter::default()).await.unwrap_err();
        let text = err.to_string();
        assert!(text.contains("HTTP 401"), "{text}");
        assert!(!text.contains("s3cret-pat"), "{text}");
    }

    fn agile_tracker(base: &str) -> AzdoTracker {
        let mut config = TrackerConfig {
            name: "azdo".to_owned(),
            kind: crate::config::TrackerKind::Azdo,
            project_path: "/tmp/p".to_owned(),
            credential: Default::default(),
            filter: TrackerFilter::default(),
            status_map: super::super::status_map_from(super::super::AZDO_AGILE_STATUS_MAP),
            acknowledged: None,
            extra: BTreeMap::new(),
        };
        config.extra.insert("project".to_owned(), "My Project".to_owned());
        tracker(base).with_config(config)
    }

    fn item_json(index: usize) -> String {
        serde_json::from_str::<Value>(BATCH_FIXTURE).unwrap()["value"][index].to_string()
    }

    fn body_of(raw: &str) -> Value {
        serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn transition_patches_state_and_recomputed_tags_with_rev_test() {
        // Item 101 (rev 7): tags "varda; backend; varda:ready".
        let (base, seen) = fake_server(vec![
            (200, vec![], item_json(0)),
            (200, vec![], item_json(0)),
        ]);
        let t = agile_tracker(&base);
        let updated = t.transition("101", TaskStatus::NeedsUser, Some(7)).await.unwrap();
        assert_eq!(updated.id, "101");
        let requests = seen.lock().unwrap();
        assert!(requests[0].starts_with("GET /My%20Project/_apis/wit/workitems/101?"));
        assert!(requests[1].starts_with("PATCH /My%20Project/_apis/wit/workitems/101?api-version=7.1 "));
        assert!(requests[1].contains("Content-Type: application/json-patch+json"), "{}", requests[1]);
        let ops = body_of(&requests[1]);
        assert_eq!(ops[0], json!({"op": "test", "path": "/rev", "value": 7}));
        assert_eq!(ops[1], json!({"op": "add", "path": "/fields/System.State", "value": "Active"}));
        // user tags kept, stale varda:ready dropped, new helper added
        assert_eq!(
            ops[2],
            json!({"op": "add", "path": "/fields/System.Tags", "value": "varda; backend; varda:needs-user"})
        );
    }

    #[tokio::test]
    async fn transition_without_expected_rev_tests_the_fetched_rev() {
        let (base, seen) = fake_server(vec![(200, vec![], item_json(0)), (200, vec![], item_json(0))]);
        agile_tracker(&base).transition("101", TaskStatus::Done, None).await.unwrap();
        let ops = body_of(&seen.lock().unwrap()[1]);
        assert_eq!(ops[0], json!({"op": "test", "path": "/rev", "value": 7}));
        assert_eq!(ops[1]["path"], "/fields/System.State");
        assert_eq!(ops[1]["value"], "Closed");
    }

    #[tokio::test]
    async fn transition_without_expected_rev_retries_once_after_concurrent_edit() {
        // First PATCH loses the race (412); the re-read sees the human's new tag.
        let edited = item_json(0).replace("varda; backend; varda:ready", "varda; backend; varda:ready; human-tag");
        assert_ne!(edited, item_json(0), "fixture tags changed");
        let (base, seen) = fake_server(vec![
            (200, vec![], item_json(0)),
            (412, vec![], "{}".to_owned()),
            (200, vec![], edited),
            (200, vec![], item_json(0)),
        ]);
        agile_tracker(&base).transition("101", TaskStatus::NeedsUser, None).await.unwrap();
        let requests = seen.lock().unwrap();
        assert_eq!(requests.len(), 4);
        let ops = body_of(&requests[3]);
        assert_eq!(ops[0]["op"], "test");
        assert_eq!(
            ops[2]["value"],
            "varda; backend; human-tag; varda:needs-user",
            "{ops}"
        );
    }

    #[tokio::test]
    async fn transition_without_expected_rev_persistent_conflict_is_returned_after_one_retry() {
        let (base, seen) = fake_server(vec![
            (200, vec![], item_json(0)),
            (412, vec![], "{}".to_owned()),
            (200, vec![], item_json(0)),
            (412, vec![], "{}".to_owned()),
        ]);
        let err = agile_tracker(&base).transition("101", TaskStatus::Done, None).await.unwrap_err();
        assert!(err.is_conflict(), "{err}");
        assert_eq!(seen.lock().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn transition_with_expected_rev_does_not_retry() {
        let (base, seen) = fake_server(vec![(200, vec![], item_json(0)), (412, vec![], "{}".to_owned())]);
        let err = agile_tracker(&base).transition("101", TaskStatus::Done, Some(7)).await.unwrap_err();
        assert!(err.is_conflict(), "{err}");
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn transition_conflict_paths() {
        // stale rev detected on the pre-read: no PATCH is sent
        let (base, seen) = fake_server(vec![(200, vec![], item_json(0))]);
        let err = agile_tracker(&base)
            .transition("101", TaskStatus::Done, Some(3))
            .await
            .unwrap_err();
        assert!(matches!(err, TrackerError::Conflict { expected: Some(3), actual: Some(7), .. }), "{err}");
        assert_eq!(seen.lock().unwrap().len(), 1);

        // 412 and 409 and 400-with-failed-test on the PATCH
        for (status, body) in [
            (412, "{}"),
            (409, "{}"),
            (400, r#"{"typeKey":"JsonPatchTestOperationFailedException"}"#),
        ] {
            let (base, _) = fake_server(vec![
                (200, vec![], item_json(0)),
                (status, vec![], body.to_owned()),
            ]);
            let err = agile_tracker(&base)
                .transition("101", TaskStatus::Done, Some(7))
                .await
                .unwrap_err();
            assert!(err.is_conflict(), "{status}: {err}");
        }

        // other failures are not conflicts
        let (base, _) = fake_server(vec![(200, vec![], item_json(0)), (500, vec![], "boom".to_owned())]);
        let err = agile_tracker(&base).transition("101", TaskStatus::Done, None).await.unwrap_err();
        assert!(!err.is_conflict() && err.to_string().contains("HTTP 500"), "{err}");
    }

    #[tokio::test]
    async fn transition_requires_config() {
        let err = tracker("http://127.0.0.1:1")
            .transition("1", TaskStatus::Done, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no config"), "{err}");
    }

    #[tokio::test]
    async fn comment_posts_markdown() {
        let (base, seen) = fake_server(vec![(200, vec![], "{}".to_owned())]);
        tracker(&base).comment("101", "hello **x**").await.unwrap();
        let requests = seen.lock().unwrap();
        assert!(requests[0].starts_with(
            "POST /My%20Project/_apis/wit/workItems/101/comments?format=markdown&api-version=7.1-preview.4 "
        ), "{}", requests[0]);
        assert_eq!(body_of(&requests[0]), json!({"text": "hello **x**"}));
    }

    #[tokio::test]
    async fn comment_falls_back_to_html_on_400() {
        let (base, seen) = fake_server(vec![
            (400, vec![], "no markdown".to_owned()),
            (200, vec![], "{}".to_owned()),
        ]);
        tracker(&base)
            .comment("101", "a <b> & c\nline2\n\n```\nlet x = 1 < 2;\n```")
            .await
            .unwrap();
        let requests = seen.lock().unwrap();
        assert!(requests[1].starts_with("POST /My%20Project/_apis/wit/workItems/101/comments?api-version=7.1-preview.4 "), "{}", requests[1]);
        assert_eq!(
            body_of(&requests[1])["text"],
            "<p>a &lt;b&gt; &amp; c<br>line2</p><pre><code>let x = 1 &lt; 2;</code></pre>"
        );

        // a second 400 is an error, and 404 is NotFound
        let (base, _) = fake_server(vec![(400, vec![], "x".to_owned()), (400, vec![], "y".to_owned())]);
        assert!(tracker(&base).comment("1", "m").await.is_err());
        let (base, _) = fake_server(vec![(404, vec![], "{}".to_owned())]);
        assert!(matches!(tracker(&base).comment("1", "m").await, Err(TrackerError::NotFound(_))));
    }

    #[test]
    fn long_comments_are_truncated_on_a_char_boundary() {
        let long = "é".repeat(MAX_COMMENT_BYTES); // 2 bytes each
        let capped = cap_markdown(&long);
        assert!(capped.len() <= MAX_COMMENT_BYTES, "{}", capped.len());
        assert!(capped.contains("truncated by Varda"));
        assert_eq!(cap_markdown("short"), "short");
        let exact = "a".repeat(MAX_COMMENT_BYTES);
        assert_eq!(cap_markdown(&exact), exact);
        assert!(cap_markdown(&"a".repeat(MAX_COMMENT_BYTES + 1)).len() <= MAX_COMMENT_BYTES);
    }

    #[test]
    fn html_fallback_payload_stays_within_the_cap() {
        for ch in ["\"", "&", "<", "a\n"] {
            let html = capped_html(&ch.repeat(MAX_COMMENT_BYTES));
            assert!(html.len() <= MAX_COMMENT_BYTES, "{ch:?}: {}", html.len());
            assert!(html.contains("truncated by Varda"), "{ch:?}");
            // no entity split at the end of the escaped text
            assert!(!html.contains("&quo</") && !html.contains("&am</"), "{ch:?}");
        }
        assert_eq!(capped_html("x < y"), "<p>x &lt; y</p>");
    }

    #[tokio::test]
    async fn create_does_not_truncate_long_descriptions() {
        let (base, seen) = fake_server(vec![(200, vec![], item_json(0))]);
        let body = "d".repeat(MAX_COMMENT_BYTES + 1000);
        let draft = IssueDraft {
            title: "T".to_owned(),
            body_markdown: body.clone(),
            tags: vec![],
            assignee: None,
            work_item_type: None,
        };
        agile_tracker(&base).create(&draft).await.unwrap();
        let ops = body_of(&seen.lock().unwrap()[0]);
        assert_eq!(ops[1]["value"], format!("<p>{body}</p>"));
    }

    #[tokio::test]
    async fn assign_sets_and_removes() {
        let (base, seen) = fake_server(vec![(200, vec![], item_json(0)), (200, vec![], item_json(0))]);
        let t = tracker(&base);
        t.assign("101", Some("ada@acme.example")).await.unwrap();
        t.assign("101", None).await.unwrap();
        let requests = seen.lock().unwrap();
        assert_eq!(
            body_of(&requests[0]),
            json!([{"op": "add", "path": "/fields/System.AssignedTo", "value": "ada@acme.example"}])
        );
        assert_eq!(
            body_of(&requests[1]),
            json!([{"op": "remove", "path": "/fields/System.AssignedTo"}])
        );
        assert!(requests[0].contains("application/json-patch+json"));
        assert!(matches!(t.assign("../x", None).await, Err(TrackerError::NotFound(_))));
    }

    #[tokio::test]
    async fn create_posts_typed_patch_with_html_description_and_tags() {
        let (base, seen) = fake_server(vec![(200, vec![], item_json(0)), (200, vec![], item_json(0))]);
        let t = agile_tracker(&base);
        let draft = IssueDraft {
            title: "T".to_owned(),
            body_markdown: "x < y".to_owned(),
            tags: vec!["varda".to_owned(), "a".to_owned()],
            assignee: Some("me".to_owned()),
            work_item_type: None,
        };
        let created = t.create(&draft).await.unwrap();
        assert_eq!(created.id, "101");
        t.create(&IssueDraft { work_item_type: Some("User Story".to_owned()), ..draft })
            .await
            .unwrap();
        let requests = seen.lock().unwrap();
        assert!(requests[0].starts_with("POST /My%20Project/_apis/wit/workitems/$Task?api-version=7.1 "), "{}", requests[0]);
        assert!(requests[1].starts_with("POST /My%20Project/_apis/wit/workitems/$User%20Story?"), "{}", requests[1]);
        assert!(requests[0].contains("application/json-patch+json"));
        let ops = body_of(&requests[0]);
        assert_eq!(ops[0]["value"], "T");
        assert_eq!(ops[1]["value"], "<p>x &lt; y</p>");
        assert_eq!(ops[2]["value"], "varda; a");
        assert_eq!(ops[3]["path"], "/fields/System.AssignedTo");
    }

    #[tokio::test]
    async fn work_item_states_lists_names() {
        let body = r#"{"count":2,"value":[{"name":"New","category":"Proposed"},{"name":"Active"}]}"#;
        let (base, seen) = fake_server(vec![(200, vec![], body.to_owned())]);
        let states = tracker(&base).work_item_states("User Story").await.unwrap();
        assert_eq!(states, vec!["New", "Active"]);
        assert!(seen.lock().unwrap()[0].starts_with(
            "GET /My%20Project/_apis/wit/workitemtypes/User%20Story/states?api-version=7.1 "
        ));
    }

    #[test]
    fn wiql_escapes_quotes_and_honours_override() {
        let filter = TrackerFilter {
            tags: vec!["it's".to_owned()],
            area_path: Some("Proj\\Team".to_owned()),
            ..TrackerFilter::default()
        };
        let w = build_wiql(&filter);
        assert!(w.contains("CONTAINS 'it''s'"), "{w}");
        assert!(w.contains("[System.AreaPath] UNDER 'Proj\\Team'"));
        let custom = TrackerFilter {
            wiql: Some("SELECT [System.Id] FROM WorkItems".to_owned()),
            tags: vec!["ignored".to_owned()],
            ..TrackerFilter::default()
        };
        assert_eq!(build_wiql(&custom), "SELECT [System.Id] FROM WorkItems");
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b":abc"), "OmFiYw==");
    }

    #[test]
    fn html_conversion_basics() {
        assert_eq!(html_to_markdown(""), "");
        assert_eq!(html_to_markdown("a &amp; b &lt;c&gt; &#65;"), "a & b <c> A");
        assert_eq!(html_to_markdown("<h2>T</h2><p>x<br>y</p>"), "## T\n\nx\ny");
        assert_eq!(
            html_to_markdown("<ul><li>a<ul><li>b</li></ul></li><li>c</li></ul>"),
            "- a\n  - b\n- c"
        );
        assert_eq!(html_to_markdown("<pre>a\n b</pre>"), "```\na\n b\n```");
        assert_eq!(html_to_markdown("<img src='x'>"), "[image omitted]");
    }

    #[test]
    fn from_config_requires_settings() {
        let mut config = TrackerConfig {
            name: "azdo".to_owned(),
            kind: crate::config::TrackerKind::Azdo,
            project_path: "/tmp/p".to_owned(),
            credential: Default::default(),
            filter: TrackerFilter::default(),
            status_map: BTreeMap::new(),
            acknowledged: None,
            extra: BTreeMap::new(),
        };
        config.extra.insert("project".to_owned(), "P".to_owned());
        assert!(AzdoTracker::from_config(&config, "x".to_owned()).is_err());
        config.extra.insert("organization".to_owned(), "acme".to_owned());
        let t = AzdoTracker::from_config(&config, "x".to_owned()).unwrap();
        assert_eq!(t.base_url, "https://dev.azure.com/acme");
    }

    /// Live smoke test; needs `VARDA_AZDO_LIVE=1` plus org/project/PAT. Uses the
    /// production HTTPS transport ([`UreqTransport`]).
    #[tokio::test]
    #[ignore]
    async fn live_query() {
        if std::env::var("VARDA_AZDO_LIVE").as_deref() != Ok("1") {
            return;
        }
        let org = std::env::var("VARDA_AZDO_ORG").unwrap();
        let project = std::env::var("VARDA_AZDO_PROJECT").unwrap();
        let pat = std::env::var("VARDA_AZDO_PAT").unwrap();
        let base = format!("https://dev.azure.com/{org}");
        let t = AzdoTracker::new("azdo".to_owned(), base.clone(), project, pat, transport_for(&base));
        let issues = t.query(&TrackerFilter::default()).await.unwrap();
        println!("{} items", issues.len());
    }
}
