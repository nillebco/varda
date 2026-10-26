//! GitHub Issues backend (milestone T4): the [`IssueTracker`] over the REST API.
//!
//! GitHub issues only have `open`/`closed` (+ `state_reason`), so the remote
//! state reported here is `open`, `closed:completed`, `closed:not_planned`
//! (or plain `closed` when no reason is set), and the `varda:*` helper labels
//! disambiguate the finer Varda statuses exactly as the tags do for Azure
//! DevOps. There is no revision field: `rev` is `updated_at` as epoch seconds.
//!
//! HTTP goes through the shared [`Transport`] so tests inject a local fake. The
//! token is resolved on the host by [`super::build_tracker`], sent only as
//! `Authorization: Bearer`, and scrubbed from every error.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, anyhow};
use async_trait::async_trait;
use serde_json::{Value, json};

use super::azdo::{
    HttpRequest, HttpResponse, Transport, cap_markdown, redact, send_with_retry, transport_for,
    url_encode,
};
use super::{IssueDraft, IssueTracker, RemoteIssue, Result, TrackerError, map_status};
use crate::config::{TrackerConfig, TrackerFilter};
use crate::task::TaskStatus;

const DEFAULT_API_BASE: &str = "https://api.github.com";
const API_VERSION: &str = "2022-11-28";
const PER_PAGE: usize = 100;
/// Cap on pages followed for one listing.
const MAX_PAGES: usize = 10;
/// Remote states a `status_map` value may name.
const VALID_STATES: &[&str] = &["open", "closed", "closed:completed", "closed:not_planned"];
/// Colour for labels created by setup.
const LABEL_COLOR: &str = "5319e7";

pub struct GithubTracker {
    name: String,
    /// `https://api.github.com` unless `api_base` (GitHub Enterprise, tests).
    api_base: String,
    /// `owner/name`.
    repo: String,
    token: String,
    transport: Arc<dyn Transport>,
    config: Option<TrackerConfig>,
}

/// `owner/name` with only characters GitHub allows in either part.
pub fn valid_repo_slug(slug: &str) -> bool {
    let part_ok = |p: &str| {
        !p.is_empty()
            && p != "."
            && p != ".."
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    matches!(slug.split_once('/'), Some((o, r)) if part_ok(o) && part_ok(r))
}

impl GithubTracker {
    /// Build from a `[[trackers]]` entry (`repo`, optional `api_base`) and an
    /// already-resolved token.
    pub fn from_config(config: &TrackerConfig, token: String) -> anyhow::Result<Self> {
        let repo = config
            .extra_str("repo")
            .map(str::trim)
            .filter(|s| valid_repo_slug(s))
            .ok_or_else(|| {
                anyhow!(
                    "github tracker '{}' needs a `repo = \"owner/name\"` setting",
                    config.name
                )
            })?
            .to_owned();
        let api_base = match config.extra_str("api_base").map(str::trim) {
            Some(url) if !url.is_empty() => url.trim_end_matches('/').to_owned(),
            _ => DEFAULT_API_BASE.to_owned(),
        };
        let transport = transport_for(&api_base);
        Ok(Self::new(config.name.clone(), api_base, repo, token, transport)
            .with_config(config.clone()))
    }

    /// Attach the tracker entry that drives `transition` (status map).
    pub fn with_config(mut self, config: TrackerConfig) -> Self {
        self.config = Some(config);
        self
    }

    pub fn new(
        name: String,
        api_base: String,
        repo: String,
        token: String,
        transport: Arc<dyn Transport>,
    ) -> Self {
        Self {
            name,
            api_base,
            repo,
            token,
            transport,
            config: None,
        }
    }

    fn repo_url(&self, suffix: &str) -> String {
        format!("{}/repos/{}{suffix}", self.api_base, self.repo)
    }

    fn request(&self, method: &'static str, url: String, body: Option<Value>) -> HttpRequest {
        let mut headers = vec![
            ("Authorization".to_owned(), format!("Bearer {}", self.token)),
            ("Accept".to_owned(), "application/vnd.github+json".to_owned()),
            ("X-GitHub-Api-Version".to_owned(), API_VERSION.to_owned()),
            ("User-Agent".to_owned(), "varda".to_owned()),
        ];
        if body.is_some() {
            headers.push(("Content-Type".to_owned(), "application/json".to_owned()));
        }
        HttpRequest {
            method,
            url,
            headers,
            body: body.map(|b| b.to_string().into_bytes()),
        }
    }

    /// Send with bounded retry on 429/503, redact the token from any error.
    async fn call(&self, request: HttpRequest) -> Result<HttpResponse> {
        let transport = self.transport.clone();
        let token = self.token.clone();
        let result = tokio::task::spawn_blocking(move || send_with_retry(&*transport, &request))
            .await
            .map_err(|e| anyhow!("http task failed: {e}"))?;
        result.map_err(|e| TrackerError::Other(anyhow!(redact(&format!("{e:#}"), &token))))
    }

    fn http_error(&self, what: &str, response: &HttpResponse) -> TrackerError {
        // Redact the WHOLE body before truncating: a secret straddling the cut
        // would otherwise survive as an unmatched prefix.
        let body = redact(&String::from_utf8_lossy(&response.body), &self.token);
        let snippet: String = body.chars().take(300).collect();
        TrackerError::Other(anyhow!(redact(
            &format!("github {what}: HTTP {}: {snippet}", response.status),
            &self.token
        )))
    }

    /// Success body as JSON; 404 ⇒ `NotFound(what)`, other non-2xx ⇒ error.
    fn json_of(&self, response: HttpResponse, what: &str) -> Result<Value> {
        if response.status == 404 {
            return Err(TrackerError::NotFound(what.to_owned()));
        }
        if !(200..300).contains(&response.status) {
            return Err(self.http_error(what, &response));
        }
        serde_json::from_slice(&response.body)
            .with_context(|| format!("github {what}: response is not JSON"))
            .map_err(TrackerError::from)
    }

    async fn call_json(
        &self,
        method: &'static str,
        url: String,
        body: Option<Value>,
        what: &str,
    ) -> Result<Value> {
        let response = self.call(self.request(method, url, body)).await?;
        self.json_of(response, what)
    }

    /// GET a paginated array, following `Link: <...>; rel="next"` for at most
    /// [`MAX_PAGES`] pages. A `next` link leaving `api_base` is never followed
    /// (the token would travel with it).
    async fn list_pages(&self, first_url: String, what: &str) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut url = first_url;
        for page in 0..MAX_PAGES {
            let response = self.call(self.request("GET", url.clone(), None)).await?;
            let next = response.header("link").and_then(next_link);
            let value = self.json_of(response, what)?;
            let items = value
                .as_array()
                .ok_or_else(|| anyhow!("github {what}: expected a JSON array"))?;
            out.extend(items.iter().cloned());
            match next {
                Some(next) if page + 1 < MAX_PAGES => {
                    if !next.starts_with(&format!("{}/", self.api_base)) {
                        return Err(anyhow!("github {what}: refusing to follow a next link outside the API base").into());
                    }
                    url = next;
                }
                _ => break,
            }
        }
        Ok(out)
    }

    fn map_issue(&self, item: &Value) -> anyhow::Result<RemoteIssue> {
        let number = item
            .get("number")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("issue without numeric `number`"))?;
        let text = |key: &str| item.get(key).and_then(Value::as_str).unwrap_or("");
        let updated_at = item.get("updated_at").and_then(Value::as_str);
        // `updated_at` is the only conflict signal; never manufacture one.
        let rev = updated_at
            .and_then(epoch_from_rfc3339)
            .ok_or_else(|| anyhow!("issue #{number}: missing or invalid `updated_at`"))?;
        let remote_state = match (text("state"), item.get("state_reason").and_then(Value::as_str)) {
            ("closed", Some(reason)) => format!("closed:{}", reason.to_ascii_lowercase()),
            ("closed", None) => "closed".to_owned(),
            _ => "open".to_owned(),
        };
        let tags = item
            .get("labels")
            .and_then(Value::as_array)
            .map(|labels| {
                labels
                    .iter()
                    .filter_map(|l| {
                        l.as_str()
                            .or_else(|| l.get("name").and_then(Value::as_str))
                            .map(str::to_owned)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let assignee = item
            .pointer("/assignee/login")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let mut fields = BTreeMap::new();
        if let Some(at) = updated_at {
            fields.insert("updated_at".to_owned(), json!(at));
        }
        if let Some(author) = item.pointer("/user/login") {
            fields.insert("author".to_owned(), author.clone());
        }
        Ok(RemoteIssue {
            id: number.to_string(),
            url: item
                .get("html_url")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    format!("https://github.com/{}/issues/{number}", self.repo)
                }),
            rev,
            title: text("title").to_owned(),
            body_markdown: text("body").to_owned(),
            closed: super::github_state_is_closed(&remote_state),
            remote_state,
            assignee,
            tags,
            fields,
        })
    }

    /// Names of the repository's labels.
    async fn label_names(&self) -> Result<Vec<String>> {
        let url = self.repo_url(&format!("/labels?per_page={PER_PAGE}"));
        let labels = self.list_pages(url, "labels").await?;
        Ok(labels
            .iter()
            .filter_map(|l| l.get("name").and_then(Value::as_str).map(str::to_owned))
            .collect())
    }
}

#[async_trait]
impl IssueTracker for GithubTracker {
    fn name(&self) -> &str {
        &self.name
    }

    async fn query(&self, filter: &TrackerFilter) -> Result<Vec<RemoteIssue>> {
        let all_open = !filter.states.is_empty()
            && filter.states.iter().all(|s| s.eq_ignore_ascii_case("open"));
        let all_closed = !filter.states.is_empty()
            && filter
                .states
                .iter()
                .all(|s| s.to_ascii_lowercase().starts_with("closed"));
        // Empty `states` ⇒ everything, so pull also sees items closed remotely.
        let state = if all_open {
            "open"
        } else if all_closed {
            "closed"
        } else {
            "all"
        };
        let mut url = self.repo_url(&format!(
            "/issues?state={state}&sort=updated&direction=desc&per_page={PER_PAGE}"
        ));
        if !filter.tags.is_empty() {
            let labels: Vec<String> = filter.tags.iter().map(|t| url_encode(t)).collect();
            url.push_str(&format!("&labels={}", labels.join(",")));
        }
        let mut issues = Vec::new();
        for item in self.list_pages(url, "issues").await? {
            // The issues endpoint also lists pull requests.
            if item.get("pull_request").is_some() {
                continue;
            }
            let issue = self.map_issue(&item)?;
            if filter.states.is_empty() || state_matches_any(&issue.remote_state, &filter.states) {
                issues.push(issue);
            }
        }
        Ok(issues)
    }

    async fn get(&self, id: &str) -> Result<RemoteIssue> {
        check_id(id)?;
        let value = self
            .call_json("GET", self.repo_url(&format!("/issues/{id}")), None, &format!("issue {id}"))
            .await?;
        if value.get("pull_request").is_some() {
            return Err(TrackerError::NotFound(id.to_owned()));
        }
        Ok(self.map_issue(&value)?)
    }

    async fn create(&self, draft: &IssueDraft) -> Result<RemoteIssue> {
        let mut body = json!({
            "title": draft.title,
            "body": draft.body_markdown,
        });
        if !draft.tags.is_empty() {
            body["labels"] = json!(draft.tags);
        }
        if let Some(who) = draft.assignee.as_deref().filter(|w| !w.is_empty()) {
            body["assignees"] = json!([who]);
        }
        let value = self
            .call_json("POST", self.repo_url("/issues"), Some(body), "create issue")
            .await?;
        Ok(self.map_issue(&value)?)
    }

    async fn transition(
        &self,
        id: &str,
        to: TaskStatus,
        expected_rev: Option<u64>,
    ) -> Result<RemoteIssue> {
        let config = self.config.as_ref().ok_or_else(|| {
            TrackerError::Other(anyhow!(
                "github tracker '{}' has no config for transition",
                self.name
            ))
        })?;
        let mapped = map_status(config, to);
        let current = self.get(id).await?;
        // `updated_at` is the only revision GitHub offers: a newer one than the
        // caller last saw means a human touched the issue, so do not close/reopen.
        if let Some(expected) = expected_rev
            && current.rev != expected
        {
            return Err(TrackerError::Conflict {
                id: id.to_owned(),
                expected: Some(expected),
                actual: Some(current.rev),
            });
        }
        let mut changed = false;
        if let Some(desired) = &mapped.state {
            let (state, reason) = parse_state(desired).map_err(TrackerError::Other)?;
            let is_open = current.remote_state == "open";
            // An already-closed issue whose close reason differs (e.g.
            // not_planned → completed) still needs the PATCH.
            let reason_differs = !is_open
                && state != "open"
                && reason.is_some_and(|r| current_state_reason(&current) != Some(r));
            if is_open != (state == "open") || reason_differs {
                let mut body = json!({ "state": state });
                if let Some(reason) = reason {
                    body["state_reason"] = json!(reason);
                }
                self.call_json("PATCH", self.repo_url(&format!("/issues/{id}")), Some(body), &format!("issue {id}"))
                    .await?;
                changed = true;
            }
        }
        // Helper labels move through the add/remove endpoints so a concurrent
        // human label edit is never overwritten by a stale full replacement.
        let to_add: Vec<&String> = mapped
            .add_tags
            .iter()
            .filter(|t| !current.tags.iter().any(|c| c.eq_ignore_ascii_case(t)))
            .collect();
        if !to_add.is_empty() {
            let url = self.repo_url(&format!("/issues/{id}/labels"));
            self.call_json("POST", url, Some(json!({ "labels": to_add })), &format!("labels of {id}"))
                .await?;
            changed = true;
        }
        for stale in current
            .tags
            .iter()
            .filter(|c| mapped.remove_tags.iter().any(|r| r.eq_ignore_ascii_case(c)))
        {
            let url = self.repo_url(&format!("/issues/{id}/labels/{}", url_encode(stale)));
            let response = self.call(self.request("DELETE", url, None)).await?;
            // Already gone is fine.
            if response.status != 404 && !(200..300).contains(&response.status) {
                return Err(self.http_error(&format!("remove label from {id}"), &response));
            }
            changed = true;
        }
        if changed { self.get(id).await } else { Ok(current) }
    }

    async fn comment(&self, id: &str, markdown: &str) -> Result<()> {
        check_id(id)?;
        let body = json!({ "body": cap_markdown(markdown) });
        let url = self.repo_url(&format!("/issues/{id}/comments"));
        self.call_json("POST", url, Some(body), id).await.map(|_| ())
    }

    async fn assign(&self, id: &str, who: Option<&str>) -> Result<()> {
        check_id(id)?;
        let assignees: Vec<&str> = who.into_iter().collect();
        let url = self.repo_url(&format!("/issues/{id}"));
        self.call_json("PATCH", url, Some(json!({ "assignees": assignees })), id)
            .await
            .map(|_| ())
    }

    async fn check_auth(&self) -> Result<()> {
        self.call_json("GET", self.repo_url(""), None, &format!("repository {}", self.repo))
            .await
            .map(|_| ())
    }

    async fn valid_states(&self) -> Result<Option<Vec<String>>> {
        Ok(Some(VALID_STATES.iter().map(|s| (*s).to_owned()).collect()))
    }

    async fn missing_labels(&self, wanted: &[String]) -> Result<Vec<String>> {
        let existing = self.label_names().await?;
        Ok(wanted
            .iter()
            .filter(|w| !existing.iter().any(|e| e.eq_ignore_ascii_case(w)))
            .cloned()
            .collect())
    }

    async fn create_labels(&self, names: &[String]) -> Result<()> {
        for name in names {
            let body = json!({
                "name": name,
                "color": LABEL_COLOR,
                "description": "Managed by Varda",
            });
            let response = self
                .call(self.request("POST", self.repo_url("/labels"), Some(body)))
                .await?;
            // 422 ⇒ it already exists (raced with a human); that is the goal.
            if response.status != 422 && !(200..300).contains(&response.status) {
                return Err(self.http_error(&format!("create label {name}"), &response));
            }
        }
        Ok(())
    }
}

fn check_id(id: &str) -> Result<()> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(TrackerError::NotFound(id.to_owned()));
    }
    Ok(())
}

/// The close reason encoded in a mapped `remote_state` (`closed:<reason>`).
fn current_state_reason(issue: &RemoteIssue) -> Option<&str> {
    issue.remote_state.strip_prefix("closed:")
}

/// `open` / `closed` / `closed:<reason>` → (API state, optional state_reason).
fn parse_state(value: &str) -> anyhow::Result<(&'static str, Option<&'static str>)> {
    match value.to_ascii_lowercase().as_str() {
        "open" => Ok(("open", None)),
        "closed" | "closed:completed" => Ok(("closed", Some("completed"))),
        "closed:not_planned" => Ok(("closed", Some("not_planned"))),
        other => Err(anyhow!(
            "github status_map value '{other}' must be one of {}",
            VALID_STATES.join(", ")
        )),
    }
}

/// Whether `remote` satisfies one of the filter `states` (`closed` matches any
/// `closed:<reason>`).
fn state_matches_any(remote: &str, states: &[String]) -> bool {
    states.iter().any(|s| {
        remote.eq_ignore_ascii_case(s)
            || (s.eq_ignore_ascii_case("closed") && remote.starts_with("closed"))
    })
}

/// The `rel="next"` URL of a `Link` header.
fn next_link(header: &str) -> Option<String> {
    header.split(',').find_map(|part| {
        let (url, params) = part.split_once(';')?;
        let is_next = params
            .split(';')
            .any(|p| p.trim().replace(' ', "").eq_ignore_ascii_case("rel=\"next\""));
        let url = url.trim().strip_prefix('<')?.strip_suffix('>')?;
        is_next.then(|| url.to_owned())
    })
}

/// `2026-10-01T09:00:00Z` → Unix seconds (UTC offsets and fractions accepted).
pub fn epoch_from_rfc3339(text: &str) -> Option<u64> {
    let (date, time) = text.split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let time_end = time.find(['Z', 'z', '+', '-']).unwrap_or(time.len());
    let (clock, zone) = time.split_at(time_end);
    let mut c = clock.split(':');
    let hour: i64 = c.next()?.parse().ok()?;
    let min: i64 = c.next()?.parse().ok()?;
    let sec: i64 = c.next().unwrap_or("0").split('.').next()?.parse().ok()?;
    let offset = match zone.as_bytes().first() {
        Some(sign @ (b'+' | b'-')) => {
            let (oh, om) = zone[1..].split_once(':')?;
            let secs = oh.parse::<i64>().ok()? * 3600 + om.parse::<i64>().ok()? * 60;
            if *sign == b'+' { secs } else { -secs }
        }
        _ => 0,
    };
    if !super::civil_time_is_valid(y, m, day, hour, min, sec, offset / 60) || offset % 60 != 0 {
        return None;
    }
    // Howard Hinnant's days-from-civil.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hour * 3600 + min * 60 + sec - offset).ok()
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Mutex;

    use super::super::azdo::StdHttpTransport;
    use super::super::{GITHUB_STATUS_MAP, status_from_remote, status_map_from};
    use super::*;
    use crate::config::{CredentialConfig, TrackerKind};

    const ISSUES_FIXTURE: &str = include_str!("../../testdata/github/issues.json");
    const ISSUE_FIXTURE: &str = include_str!("../../testdata/github/issue.json");
    const LABELS_FIXTURE: &str = include_str!("../../testdata/github/labels.json");

    type Reply = (u16, Vec<(&'static str, String)>, String);

    /// Serve `responses` one per connection and record each raw request.
    fn fake_server(responses: Vec<Reply>) -> (String, Arc<Mutex<Vec<String>>>) {
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
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
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
                let mut head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
                for (k, v) in headers {
                    head.push_str(&format!("{k}: {v}\r\n"));
                }
                let _ = stream.write_all(format!("{head}\r\n{body}").as_bytes());
            }
        });
        (base, seen)
    }

    fn ok(body: &str) -> Reply {
        (200, vec![], body.to_owned())
    }

    fn config(map: &[(&str, &str)]) -> TrackerConfig {
        TrackerConfig {
            name: "gh-test".to_owned(),
            kind: TrackerKind::Github,
            project_path: "/tmp/proj".to_owned(),
            credential: CredentialConfig {
                from_env: Some("TOK".to_owned()),
                ..CredentialConfig::default()
            },
            filter: TrackerFilter::default(),
            status_map: status_map_from(map),
            acknowledged: None,
            extra: BTreeMap::from([("repo".to_owned(), "acme/widgets".to_owned())]),
        }
    }

    fn tracker(base: &str) -> GithubTracker {
        GithubTracker::new(
            "gh".to_owned(),
            base.to_owned(),
            "acme/widgets".to_owned(),
            "ghp_s3cret".to_owned(),
            Arc::new(StdHttpTransport),
        )
        .with_config(config(GITHUB_STATUS_MAP))
    }

    fn with_state(fixture: &str, state: &str, reason: Option<&str>, labels: &[&str], updated: &str) -> String {
        let mut v: Value = serde_json::from_str(fixture).unwrap();
        v["state"] = json!(state);
        v["state_reason"] = json!(reason);
        v["labels"] = json!(labels.iter().map(|l| json!({"name": l})).collect::<Vec<_>>());
        v["updated_at"] = json!(updated);
        v.to_string()
    }

    #[test]
    fn rfc3339_to_epoch() {
        assert_eq!(epoch_from_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(epoch_from_rfc3339("2026-10-01T09:00:00Z"), Some(1_790_845_200));
        assert_eq!(epoch_from_rfc3339("2026-10-01T11:00:00+02:00"), Some(1_790_845_200));
        assert_eq!(epoch_from_rfc3339("2026-10-01T09:00:00.500Z"), Some(1_790_845_200));
        assert_eq!(epoch_from_rfc3339("2024-02-29T00:00:00Z"), Some(1_709_164_800));
        assert_eq!(epoch_from_rfc3339("garbage"), None);
    }

    #[test]
    fn repo_slug_and_link_parsing() {
        assert!(valid_repo_slug("acme/widgets"));
        assert!(valid_repo_slug("a-b/c_d.e"));
        for bad in ["", "acme", "acme/", "/w", "a/b/c", "a b/c", "../x", "a/.."] {
            assert!(!valid_repo_slug(bad), "{bad}");
        }
        let link = "<https://h/x?page=2>; rel=\"next\", <https://h/x?page=9>; rel=\"last\"";
        assert_eq!(next_link(link).as_deref(), Some("https://h/x?page=2"));
        assert_eq!(next_link("<https://h/x?page=1>; rel=\"prev\""), None);
    }

    #[test]
    fn from_config_validates_repo_and_api_base() {
        let mut c = config(GITHUB_STATUS_MAP);
        let t = GithubTracker::from_config(&c, "t".to_owned()).unwrap();
        assert_eq!(t.api_base, "https://api.github.com");
        c.extra.insert("api_base".to_owned(), "https://ghe.example/api/v3/".to_owned());
        assert_eq!(GithubTracker::from_config(&c, "t".to_owned()).unwrap().api_base, "https://ghe.example/api/v3");
        c.extra.insert("repo".to_owned(), "nope".to_owned());
        let err = GithubTracker::from_config(&c, "t".to_owned()).err().unwrap();
        assert!(err.to_string().contains("owner/name"), "{err}");
        c.extra.remove("repo");
        assert!(GithubTracker::from_config(&c, "t".to_owned()).is_err());
    }

    #[test]
    fn default_map_round_trips() {
        let c = config(GITHUB_STATUS_MAP);
        assert_eq!(status_from_remote(&c, "open", &[]), Some(TaskStatus::Backlog));
        assert_eq!(
            status_from_remote(&c, "open", &["varda:ready".to_owned()]),
            Some(TaskStatus::Ready)
        );
        assert_eq!(
            status_from_remote(&c, "open", &["varda:needs-user".to_owned()]),
            Some(TaskStatus::NeedsUser)
        );
        assert_eq!(status_from_remote(&c, "closed:completed", &[]), Some(TaskStatus::Done));
        assert_eq!(status_from_remote(&c, "closed:not_planned", &[]), None);
        // A bare `closed` value in the map accepts any close reason.
        let loose = config(&[("backlog", "open"), ("done", "closed")]);
        assert_eq!(status_from_remote(&loose, "closed:not_planned", &[]), Some(TaskStatus::Done));
        assert_eq!(status_from_remote(&loose, "closed", &[]), Some(TaskStatus::Done));
    }

    #[tokio::test]
    async fn query_sends_auth_headers_and_refuses_off_base_next_link() {
        let (base, seen) = fake_server(vec![(
            200,
            vec![("Link", "<https://evil.example/x?page=2>; rel=\"next\"".to_owned())],
            ISSUES_FIXTURE.to_owned(),
        )]);
        let t = tracker(&base);
        let filter = TrackerFilter {
            tags: vec!["varda".to_owned(), "needs:review".to_owned()],
            ..TrackerFilter::default()
        };
        let err = t.query(&filter).await.unwrap_err();
        assert!(err.to_string().contains("next link"), "{err}");
        let first = seen.lock().unwrap()[0].clone();
        assert!(
            first.starts_with("GET /repos/acme/widgets/issues?state=all&sort=updated&direction=desc&per_page=100&labels=varda,needs%3Areview HTTP/1.1"),
            "{first}"
        );
        let lower = first.to_ascii_lowercase();
        assert!(lower.contains("authorization: bearer ghp_s3cret"), "{first}");
        assert!(lower.contains("x-github-api-version: 2022-11-28"), "{first}");
        assert!(lower.contains("user-agent: varda"), "{first}");
    }

    #[tokio::test]
    async fn query_follows_next_link_and_stops_at_last_page() {
        // Two servers can't share a port, so serve both pages from one server
        // whose first reply links to a path on itself (address known after bind).
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let log = seen.clone();
        let page1 = ISSUES_FIXTURE.to_owned();
        let link = format!("<{base}/repos/acme/widgets/issues?page=2>; rel=\"next\"");
        std::thread::spawn(move || {
            for (i, body) in [page1, "[]".to_owned()].into_iter().enumerate() {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                log.lock().unwrap().push(String::from_utf8_lossy(&buf[..n]).into_owned());
                let extra = if i == 0 { format!("Link: {link}\r\n") } else { String::new() };
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        });
        let issues = tracker(&base).query(&TrackerFilter::default()).await.unwrap();
        assert_eq!(seen.lock().unwrap().len(), 2);
        assert!(seen.lock().unwrap()[1].starts_with("GET /repos/acme/widgets/issues?page=2 "));
        // Fixture: #11 open, #12 closed/completed, #13 is a pull request.
        let ids: Vec<&str> = issues.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["11", "12"]);

        let a = &issues[0];
        assert_eq!(a.url, "https://github.com/acme/widgets/issues/11");
        assert_eq!(a.title, "Retry on 429");
        assert_eq!(a.body_markdown, "Honour **Retry-After**.\n\n- give up after 2 retries");
        assert_eq!(a.remote_state, "open");
        assert_eq!(a.rev, 1_790_845_200);
        assert_eq!(a.assignee.as_deref(), Some("ada"));
        assert_eq!(a.tags, vec!["varda", "backend", "varda:ready"]);
        assert_eq!(a.fields["updated_at"], json!("2026-10-01T09:00:00Z"));

        assert!(!a.closed);
        let b = &issues[1];
        assert_eq!(b.remote_state, "closed:completed");
        assert!(b.closed);
        assert_eq!(b.body_markdown, "");
        assert_eq!(b.assignee, None);
    }

    #[tokio::test]
    async fn query_state_filter_selects_api_state_and_post_filters() {
        let (base, seen) = fake_server(vec![ok(ISSUES_FIXTURE)]);
        let filter = TrackerFilter {
            states: vec!["closed".to_owned()],
            ..TrackerFilter::default()
        };
        let issues = tracker(&base).query(&filter).await.unwrap();
        assert!(seen.lock().unwrap()[0].contains("issues?state=closed&"));
        assert_eq!(issues.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["12"]);
    }

    #[tokio::test]
    async fn query_caps_pages_at_ten() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(Mutex::new(0usize));
        let counter = hits.clone();
        let link = format!("<{base}/repos/acme/widgets/issues?page=n>; rel=\"next\"");
        std::thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                *counter.lock().unwrap() += 1;
                let _ = stream.write_all(
                    format!("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\nLink: {link}\r\n\r\n[]").as_bytes(),
                );
            }
        });
        tracker(&base).query(&TrackerFilter::default()).await.unwrap();
        assert_eq!(*hits.lock().unwrap(), MAX_PAGES);
    }

    #[tokio::test]
    async fn get_maps_not_found_and_refuses_pull_requests() {
        let t = tracker("http://127.0.0.1:1");
        assert!(matches!(t.get("abc").await, Err(TrackerError::NotFound(_))));
        let (base, _) = fake_server(vec![(404, vec![], "{\"message\":\"Not Found\"}".to_owned())]);
        assert!(matches!(tracker(&base).get("999").await, Err(TrackerError::NotFound(_))));
        let mut pr: Value = serde_json::from_str(ISSUE_FIXTURE).unwrap();
        pr["pull_request"] = json!({"url": "x"});
        let (base, _) = fake_server(vec![ok(&pr.to_string())]);
        assert!(matches!(tracker(&base).get("11").await, Err(TrackerError::NotFound(_))));
    }

    #[tokio::test]
    async fn transition_done_closes_as_completed_and_swaps_helper_labels() {
        let open = with_state(ISSUE_FIXTURE, "open", None, &["varda", "varda:ready", "bug"], "2026-10-01T09:00:00Z");
        let closed = with_state(ISSUE_FIXTURE, "closed", Some("completed"), &["varda", "bug"], "2026-10-01T09:05:00Z");
        let (base, seen) = fake_server(vec![
            ok(&open),   // get
            ok(&closed), // PATCH state
            ok("[]"),    // DELETE varda:ready
            ok(&closed), // get
        ]);
        let moved = tracker(&base)
            .transition("11", TaskStatus::Done, Some(1_790_845_200))
            .await
            .unwrap();
        assert_eq!(moved.remote_state, "closed:completed");
        assert_eq!(moved.rev, 1_790_845_500);
        let reqs = seen.lock().unwrap().clone();
        assert_eq!(reqs.len(), 4, "{reqs:#?}");
        assert!(reqs[1].starts_with("PATCH /repos/acme/widgets/issues/11 "), "{}", reqs[1]);
        assert!(reqs[1].contains("\"state\":\"closed\"") && reqs[1].contains("\"state_reason\":\"completed\""), "{}", reqs[1]);
        assert!(reqs[2].starts_with("DELETE /repos/acme/widgets/issues/11/labels/varda%3Aready "), "{}", reqs[2]);
    }

    #[tokio::test]
    async fn transition_reopens_and_adds_helper_label() {
        let closed = with_state(ISSUE_FIXTURE, "closed", Some("completed"), &["varda"], "2026-10-01T09:00:00Z");
        let after = with_state(ISSUE_FIXTURE, "open", None, &["varda", "varda:failed"], "2026-10-01T09:05:00Z");
        let (base, seen) = fake_server(vec![ok(&closed), ok(&after), ok("[]"), ok(&after)]);
        let moved = tracker(&base).transition("11", TaskStatus::Failed, None).await.unwrap();
        assert_eq!(moved.remote_state, "open");
        let reqs = seen.lock().unwrap().clone();
        assert!(reqs[1].contains("\"state\":\"open\"") && !reqs[1].contains("state_reason"), "{}", reqs[1]);
        assert!(reqs[2].starts_with("POST /repos/acme/widgets/issues/11/labels "), "{}", reqs[2]);
        assert!(reqs[2].contains("\"labels\":[\"varda:failed\"]"), "{}", reqs[2]);
    }

    #[tokio::test]
    async fn transition_with_newer_updated_at_is_a_conflict_and_changes_nothing() {
        let human = with_state(ISSUE_FIXTURE, "open", None, &["varda"], "2026-10-01T09:30:00Z");
        let (base, seen) = fake_server(vec![ok(&human)]);
        let err = tracker(&base)
            .transition("11", TaskStatus::Done, Some(1_790_845_200))
            .await
            .unwrap_err();
        match err {
            TrackerError::Conflict { expected, actual, .. } => {
                assert_eq!(expected, Some(1_790_845_200));
                assert_eq!(actual, Some(1_790_847_000));
            }
            other => panic!("{other}"),
        }
        assert_eq!(seen.lock().unwrap().len(), 1, "only the read happened");
    }

    #[tokio::test]
    async fn transition_without_changes_makes_no_writes() {
        // Running ⇒ open with no helper labels; the issue already is.
        let open = with_state(ISSUE_FIXTURE, "open", None, &["varda"], "2026-10-01T09:00:00Z");
        let (base, seen) = fake_server(vec![ok(&open)]);
        let same = tracker(&base).transition("11", TaskStatus::Running, None).await.unwrap();
        assert_eq!(same.remote_state, "open");
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn create_comment_assign_send_expected_bodies() {
        let (base, seen) = fake_server(vec![
            (201, vec![], ISSUE_FIXTURE.to_owned()),
            (201, vec![], "{}".to_owned()),
            ok("{}"),
            ok("{}"),
        ]);
        let t = tracker(&base);
        let created = t
            .create(&IssueDraft {
                title: "T".to_owned(),
                body_markdown: "B".to_owned(),
                tags: vec!["varda".to_owned()],
                assignee: Some("ada".to_owned()),
                work_item_type: None,
                area_path: None,
            })
            .await
            .unwrap();
        assert_eq!(created.id, "11");
        t.comment("11", "recap **md**").await.unwrap();
        t.assign("11", Some("bob")).await.unwrap();
        t.assign("11", None).await.unwrap();
        let reqs = seen.lock().unwrap().clone();
        assert!(reqs[0].starts_with("POST /repos/acme/widgets/issues "));
        assert!(reqs[0].contains("\"labels\":[\"varda\"]") && reqs[0].contains("\"assignees\":[\"ada\"]"), "{}", reqs[0]);
        assert!(reqs[1].starts_with("POST /repos/acme/widgets/issues/11/comments "));
        assert!(reqs[1].ends_with("{\"body\":\"recap **md**\"}"), "{}", reqs[1]);
        assert!(reqs[2].ends_with("{\"assignees\":[\"bob\"]}"), "{}", reqs[2]);
        assert!(reqs[3].ends_with("{\"assignees\":[]}"), "{}", reqs[3]);
    }

    #[tokio::test]
    async fn labels_are_checked_and_created() {
        let (base, seen) = fake_server(vec![
            ok(LABELS_FIXTURE),
            (201, vec![], "{}".to_owned()),
            (422, vec![], "{\"message\":\"already_exists\"}".to_owned()),
            (403, vec![], "{\"message\":\"nope\"}".to_owned()),
        ]);
        let t = tracker(&base);
        let wanted: Vec<String> = ["varda", "Varda:Ready", "varda:failed"].map(str::to_owned).to_vec();
        let missing = t.missing_labels(&wanted).await.unwrap();
        // `varda` exists; `Varda:Ready` matches case-insensitively.
        assert_eq!(missing, vec!["varda:failed"]);
        t.create_labels(&["varda:failed".to_owned()]).await.unwrap();
        t.create_labels(&["varda:failed".to_owned()]).await.unwrap();
        let err = t.create_labels(&["x".to_owned()]).await.unwrap_err();
        assert!(err.to_string().contains("403"), "{err}");
        assert!(seen.lock().unwrap()[1].contains("\"name\":\"varda:failed\""));
    }

    #[tokio::test]
    async fn errors_never_contain_the_token() {
        let (base, _) = fake_server(vec![(401, vec![], "bad credentials ghp_s3cret".to_owned())]);
        let err = tracker(&base).check_auth().await.unwrap_err();
        let text = err.to_string();
        assert!(text.contains("401") && !text.contains("ghp_s3cret"), "{text}");
        // Transport failure (closed port) is scrubbed too.
        let err = tracker("http://127.0.0.1:1").check_auth().await.unwrap_err();
        assert!(!err.to_string().contains("ghp_s3cret"));
    }

    #[tokio::test]
    async fn auth_check_hits_the_repo_and_valid_states_are_listed() {
        let (base, seen) = fake_server(vec![ok("{\"full_name\":\"acme/widgets\"}")]);
        let t = tracker(&base);
        t.check_auth().await.unwrap();
        assert!(seen.lock().unwrap()[0].starts_with("GET /repos/acme/widgets HTTP/1.1"));
        let states = t.valid_states().await.unwrap().unwrap();
        assert!(states.contains(&"closed:completed".to_owned()));
        assert!(super::super::doctor::invalid_status_map_entries(&config(GITHUB_STATUS_MAP), &states).is_empty());
    }

    /// Live read against a real repository. `VARDA_GITHUB_LIVE=1`,
    /// `VARDA_GITHUB_TOKEN`, `VARDA_GITHUB_REPO=owner/name`.
    #[tokio::test]
    #[ignore]
    async fn live_read() {
        if std::env::var("VARDA_GITHUB_LIVE").as_deref() != Ok("1") {
            return;
        }
        let token = std::env::var("VARDA_GITHUB_TOKEN").expect("VARDA_GITHUB_TOKEN");
        let repo = std::env::var("VARDA_GITHUB_REPO").expect("VARDA_GITHUB_REPO");
        let mut c = config(GITHUB_STATUS_MAP);
        c.extra.insert("repo".to_owned(), repo);
        let t = GithubTracker::from_config(&c, token).unwrap();
        t.check_auth().await.expect("auth");
        let issues = t.query(&TrackerFilter::default()).await.expect("query");
        println!("{} issue(s)", issues.len());
    }

    // Review #1235: a token straddling the 300-char snippet cut must not leak.
    #[test]
    fn http_error_redacts_before_truncating() {
        let t = tracker("http://127.0.0.1:9");
        let body = format!("{}ghp_s3cret tail", "x".repeat(295));
        let response = HttpResponse { status: 500, headers: vec![], body: body.into_bytes() };
        let err = format!("{:#}", anyhow::Error::from(t.http_error("probe", &response)));
        assert!(!err.contains("ghp_s"), "{err}");
    }

    // Review #1235: closed:not_planned → done (closed:completed) must PATCH the reason.
    #[tokio::test]
    async fn transition_changes_close_reason_on_already_closed_issue() {
        let not_planned = with_state(ISSUE_FIXTURE, "closed", Some("not_planned"), &["varda"], "2026-10-01T09:00:00Z");
        let completed = with_state(ISSUE_FIXTURE, "closed", Some("completed"), &["varda"], "2026-10-01T09:05:00Z");
        let (base, seen) = fake_server(vec![ok(&not_planned), ok(&completed), ok(&completed)]);
        let moved = tracker(&base).transition("11", TaskStatus::Done, None).await.unwrap();
        assert_eq!(moved.remote_state, "closed:completed");
        let reqs = seen.lock().unwrap().clone();
        assert!(reqs[1].starts_with("PATCH /repos/acme/widgets/issues/11 "), "{reqs:#?}");
        assert!(reqs[1].contains("\"state_reason\":\"completed\""), "{}", reqs[1]);
    }

    #[test]
    fn issue_without_updated_at_is_rejected() {
        let t = tracker("http://127.0.0.1:9");
        let mut v: Value = serde_json::from_str(ISSUE_FIXTURE).unwrap();
        v.as_object_mut().unwrap().remove("updated_at");
        let err = t.map_issue(&v).unwrap_err().to_string();
        assert!(err.contains("updated_at"), "{err}");
    }

    #[test]
    fn impossible_timestamps_are_rejected() {
        for bad in ["2026-99-99T09:00:00Z", "2026-02-30T09:00:00Z", "2026-10-01T25:00:00Z", "2026-10-01T09:61:00Z", "2026-10-01T09:00:00+15:00"] {
            assert_eq!(epoch_from_rfc3339(bad), None, "{bad}");
        }
        assert!(epoch_from_rfc3339("2024-02-29T23:59:60Z").is_some());
        let t = tracker("http://127.0.0.1:9");
        let mut v: Value = serde_json::from_str(ISSUE_FIXTURE).unwrap();
        v["updated_at"] = json!("2026-99-99T09:00:00Z");
        assert!(t.map_issue(&v).unwrap_err().to_string().contains("updated_at"));
    }
}
