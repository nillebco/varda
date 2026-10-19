//! Jira Cloud backend (REST v3, API token): JQL search via
//! `POST /rest/api/3/search/jql` (nextPageToken pagination, capped), ADF
//! descriptions converted to markdown ([`super::jira_adf`]), status moves by
//! workflow TRANSITION (the `status_map` value names the target status).
//!
//! All HTTP goes through the azdo [`Transport`] trait so tests inject a local
//! fake. The token is resolved on the host, sent as `Authorization: Basic
//! base64(email:token)`, and scrubbed from every error. `rev` is the epoch
//! milliseconds of `fields.updated`. Jira has no atomic compare-and-set, so the
//! conflict check is read-then-write (a narrow race remains).

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, anyhow};
use async_trait::async_trait;
use serde_json::{Value, json};

use super::azdo::{HttpRequest, HttpResponse, Transport, base64, send_with_retry, transport_for, url_encode};
use super::jira_adf::{adf_to_markdown, markdown_to_adf};
use super::{IssueDraft, IssueTracker, RemoteIssue, Result, TrackerError, map_status};
use crate::config::{TrackerConfig, TrackerFilter};
use crate::task::TaskStatus;

const PAGE_SIZE: usize = 100;
/// Hard cap on issues returned by one `query`.
const MAX_ISSUES: usize = 1000;
/// Hard page ceiling, independent of issue count (empty pages with a token
/// must not loop forever).
const MAX_PAGES: usize = 20;
const DEFAULT_ISSUE_TYPE: &str = "Task";
const MAX_COMMENT_BYTES: usize = 100 * 1024;
const FIELDS: &[&str] = &[
    "summary",
    "description",
    "status",
    "labels",
    "assignee",
    "updated",
    "issuetype",
    "project",
];

pub struct JiraTracker {
    name: String,
    /// `https://x.atlassian.net` (no trailing slash).
    site: String,
    project_key: String,
    email: String,
    token: String,
    transport: Arc<dyn Transport>,
    config: Option<TrackerConfig>,
}

impl JiraTracker {
    /// Build from a `[[trackers]]` entry (`site`, `project_key`, `email`) and an
    /// already-resolved API token.
    pub fn from_config(config: &TrackerConfig, token: String) -> anyhow::Result<Self> {
        let need = |key: &str| -> anyhow::Result<String> {
            config
                .extra_str(key)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("jira tracker '{}' needs a `{key}` setting", config.name))
        };
        let site = need("site")?.trim_end_matches('/').to_owned();
        if !site.starts_with("https://") && !site.starts_with("http://") {
            anyhow::bail!("jira tracker '{}': `site` must be an http(s) URL", config.name);
        }
        let project_key = need("project_key")?;
        if !valid_project_key(&project_key) {
            anyhow::bail!(
                "jira tracker '{}': `project_key` '{project_key}' is not a Jira project key",
                config.name
            );
        }
        let transport = transport_for(&site);
        Ok(Self::new(config.name.clone(), site, project_key, need("email")?, token, transport)
            .with_config(config.clone()))
    }

    pub fn new(
        name: String,
        site: String,
        project_key: String,
        email: String,
        token: String,
        transport: Arc<dyn Transport>,
    ) -> Self {
        Self { name, site, project_key, email, token, transport, config: None }
    }

    pub fn with_config(mut self, config: TrackerConfig) -> Self {
        self.config = Some(config);
        self
    }

    fn url(&self, path: &str) -> String {
        format!("{}/rest/api/3/{path}", self.site)
    }

    fn request(&self, method: &'static str, url: String, body: Option<Value>) -> HttpRequest {
        let mut headers = vec![
            (
                "Authorization".to_owned(),
                format!("Basic {}", base64(format!("{}:{}", self.email, self.token).as_bytes())),
            ),
            ("Accept".to_owned(), "application/json".to_owned()),
        ];
        if body.is_some() {
            headers.push(("Content-Type".to_owned(), "application/json".to_owned()));
        }
        HttpRequest { method, url, headers, body: body.map(|b| b.to_string().into_bytes()) }
    }

    fn redact(&self, text: &str) -> String {
        let mut out = text.to_owned();
        if !self.token.is_empty() {
            out = out.replace(&self.token, "[redacted]");
            out = out.replace(
                &base64(format!("{}:{}", self.email, self.token).as_bytes()),
                "[redacted]",
            );
        }
        out
    }

    /// Send with bounded retry on 429/503, redacting the token from any error.
    async fn call(&self, request: HttpRequest) -> Result<HttpResponse> {
        let transport = self.transport.clone();
        let result = tokio::task::spawn_blocking(move || send_with_retry(&*transport, &request))
            .await
            .map_err(|e| anyhow!("http task failed: {e}"))?;
        result.map_err(|e| TrackerError::Other(anyhow!(self.redact(&format!("{e:#}")))))
    }

    fn http_error(&self, what: &str, response: &HttpResponse) -> TrackerError {
        // Redact the WHOLE body before truncating (see azdo::http_error).
        let body = self.redact(&String::from_utf8_lossy(&response.body));
        let snippet: String = body.chars().take(300).collect();
        TrackerError::Other(anyhow!(self.redact(&format!(
            "jira {what}: HTTP {}: {snippet}",
            response.status
        ))))
    }

    /// Check the status; `404` ⇒ NotFound(`what`).
    fn check(&self, response: HttpResponse, what: &str) -> Result<HttpResponse> {
        if response.status == 404 {
            return Err(TrackerError::NotFound(what.to_owned()));
        }
        if !(200..300).contains(&response.status) {
            return Err(self.http_error(what, &response));
        }
        Ok(response)
    }

    async fn call_json(&self, request: HttpRequest, what: &str) -> Result<Value> {
        let response = self.call(request).await?;
        let response = self.check(response, what)?;
        serde_json::from_slice(&response.body)
            .with_context(|| format!("jira {what}: response is not JSON"))
            .map_err(TrackerError::from)
    }

    /// Statuses of the project's workflows (union over issue types, in order).
    pub async fn project_statuses(&self) -> Result<Vec<String>> {
        let url = self.url(&format!("project/{}/statuses", url_encode(&self.project_key)));
        let value = self
            .call_json(self.request("GET", url, None), &format!("project {}", self.project_key))
            .await?;
        let mut names: Vec<String> = Vec::new();
        for issue_type in value.as_array().map(Vec::as_slice).unwrap_or(&[]) {
            for status in issue_type.get("statuses").and_then(Value::as_array).into_iter().flatten() {
                if let Some(name) = status.get("name").and_then(Value::as_str)
                    && !names.iter().any(|n| n == name)
                {
                    names.push(name.to_owned());
                }
            }
        }
        Ok(names)
    }

    /// Display name of the authenticated user (`/myself`), for setup output.
    pub async fn myself(&self) -> Result<String> {
        let value = self.call_json(self.request("GET", self.url("myself"), None), "myself").await?;
        Ok(value
            .get("displayName")
            .or_else(|| value.get("emailAddress"))
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned())
    }

    fn map_issue(&self, item: &Value) -> anyhow::Result<RemoteIssue> {
        let key = item
            .get("key")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("issue without `key`"))?;
        let empty = serde_json::Map::new();
        let fields = item.get("fields").and_then(Value::as_object).unwrap_or(&empty);
        let text = |k: &str| fields.get(k).and_then(Value::as_str).unwrap_or("");
        // `updated` is the only conflict signal; never manufacture one.
        let rev = parse_jira_time_millis(text("updated"))
            .ok_or_else(|| anyhow!("issue {key}: missing or invalid `fields.updated`"))?;
        let assignee = fields.get("assignee").filter(|a| a.is_object()).and_then(|a| {
            ["emailAddress", "displayName", "accountId"]
                .iter()
                .find_map(|k| a.get(*k).and_then(Value::as_str).filter(|s| !s.is_empty()))
                .map(str::to_owned)
        });
        let tags = fields
            .get("labels")
            .and_then(Value::as_array)
            .map(|l| l.iter().filter_map(Value::as_str).map(str::to_owned).collect())
            .unwrap_or_default();
        Ok(RemoteIssue {
            id: key.to_owned(),
            url: format!("{}/browse/{key}", self.site),
            rev,
            title: text("summary").to_owned(),
            body_markdown: fields.get("description").map(adf_to_markdown).unwrap_or_default(),
            remote_state: fields
                .get("status")
                .and_then(|s| s.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            assignee,
            tags,
            fields: fields.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<BTreeMap<_, _>>(),
        })
    }

    async fn search(&self, jql: &str) -> Result<Vec<RemoteIssue>> {
        let mut issues = Vec::new();
        let mut token: Option<String> = None;
        let mut seen_tokens = std::collections::HashSet::new();
        for _page in 0..MAX_PAGES {
            let mut body = json!({"jql": jql, "fields": FIELDS, "maxResults": PAGE_SIZE});
            if let Some(t) = &token {
                body["nextPageToken"] = json!(t);
            }
            let request = self.request("POST", self.url("search/jql"), Some(body));
            let value = self.call_json(request, "search/jql").await?;
            for item in value.get("issues").and_then(Value::as_array).into_iter().flatten() {
                issues.push(self.map_issue(item)?);
                if issues.len() >= MAX_ISSUES {
                    return Ok(issues);
                }
            }
            token = value
                .get("nextPageToken")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .map(str::to_owned);
            let last = value.get("isLast").and_then(Value::as_bool).unwrap_or(false);
            if token.is_none() || last {
                return Ok(issues);
            }
            if let Some(t) = &token
                && !seen_tokens.insert(t.clone())
            {
                return Err(TrackerError::Other(anyhow!(
                    "jira search: repeated nextPageToken; aborting pagination"
                )));
            }
        }
        Err(TrackerError::Other(anyhow!(
            "jira search: more than {MAX_PAGES} pages; narrow the filter"
        )))
    }

    async fn account_id(&self, who: &str) -> Result<String> {
        if !who.contains('@') {
            return Ok(who.to_owned());
        }
        let url = self.url(&format!("user/search?query={}", url_encode(who)));
        let value = self.call_json(self.request("GET", url, None), "user search").await?;
        let users = value.as_array().map(Vec::as_slice).unwrap_or(&[]);
        let exact = users.iter().find(|u| {
            u.get("emailAddress").and_then(Value::as_str).is_some_and(|e| e.eq_ignore_ascii_case(who))
        });
        exact
            .or_else(|| (users.len() == 1).then(|| &users[0]))
            .and_then(|u| u.get("accountId").and_then(Value::as_str))
            .map(str::to_owned)
            .ok_or_else(|| TrackerError::Other(anyhow!("no Jira user matches '{who}'")))
    }
}

#[async_trait]
impl IssueTracker for JiraTracker {
    fn name(&self) -> &str {
        &self.name
    }

    async fn query(&self, filter: &TrackerFilter) -> Result<Vec<RemoteIssue>> {
        self.search(&build_jql(&self.project_key, filter)).await
    }

    async fn get(&self, id: &str) -> Result<RemoteIssue> {
        check_key(id)?;
        let url = self.url(&format!("issue/{id}?fields={}", FIELDS.join(",")));
        let value = self.call_json(self.request("GET", url, None), &format!("issue {id}")).await?;
        Ok(self.map_issue(&value)?)
    }

    async fn valid_states(&self) -> Result<Option<Vec<String>>> {
        self.project_statuses().await.map(Some)
    }

    async fn check_auth(&self) -> Result<()> {
        self.call_json(self.request("GET", self.url("myself"), None), "myself").await.map(|_| ())
    }

    async fn create(&self, draft: &IssueDraft) -> Result<RemoteIssue> {
        let issue_type = draft
            .work_item_type
            .as_deref()
            .or_else(|| self.config.as_ref().and_then(|c| c.extra_str("issue_type")))
            .filter(|t| !t.trim().is_empty())
            .unwrap_or(DEFAULT_ISSUE_TYPE);
        let mut fields = json!({
            "project": {"key": self.project_key},
            "summary": draft.title,
            "issuetype": {"name": issue_type},
            "description": markdown_to_adf(&draft.body_markdown),
        });
        if !draft.tags.is_empty() {
            fields["labels"] = json!(draft.tags);
        }
        if let Some(who) = draft.assignee.as_deref().filter(|w| !w.is_empty()) {
            fields["assignee"] = json!({"accountId": self.account_id(who).await?});
        }
        let request = self.request("POST", self.url("issue"), Some(json!({"fields": fields})));
        let created = self.call_json(request, "create issue").await?;
        let key = created
            .get("key")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("jira create issue: response has no `key`"))?;
        self.get(key).await
    }

    async fn transition(
        &self,
        id: &str,
        to: TaskStatus,
        expected_rev: Option<u64>,
    ) -> Result<RemoteIssue> {
        let config = self.config.as_ref().ok_or_else(|| {
            TrackerError::Other(anyhow!("jira tracker '{}' has no config for transition", self.name))
        })?;
        let mapped = map_status(config, to);
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
        let mut changed = false;
        if let Some(target) = &mapped.state
            && !current.remote_state.eq_ignore_ascii_case(target)
        {
            let url = self.url(&format!("issue/{id}/transitions"));
            let listed = self
                .call_json(self.request("GET", url.clone(), None), &format!("transitions of {id}"))
                .await?;
            let available = listed.get("transitions").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
            let pick = available.iter().find(|t| {
                t.pointer("/to/name").and_then(Value::as_str).is_some_and(|n| n.eq_ignore_ascii_case(target))
            });
            let Some(transition_id) = pick.and_then(|t| t.get("id")).and_then(Value::as_str) else {
                let names: Vec<String> = available
                    .iter()
                    .map(|t| {
                        format!(
                            "{} (-> {})",
                            t.get("name").and_then(Value::as_str).unwrap_or("?"),
                            t.pointer("/to/name").and_then(Value::as_str).unwrap_or("?")
                        )
                    })
                    .collect();
                // Server-provided names are remote text: scrub them like any body.
                return Err(TrackerError::Other(anyhow!(self.redact(&format!(
                    "jira {id} (currently '{}') has no transition to '{target}'; available: {}",
                    current.remote_state,
                    if names.is_empty() { "none".to_owned() } else { names.join(", ") }
                )))));
            };
            let body = json!({"transition": {"id": transition_id}});
            let response = self.call(self.request("POST", url, Some(body))).await?;
            self.check(response, &format!("transition {id}"))?;
            changed = true;
        }
        // Only `varda:*` helper labels are touched.
        let mut ops = Vec::new();
        for tag in &mapped.remove_tags {
            if let Some(existing) = current.tags.iter().find(|t| t.eq_ignore_ascii_case(tag)) {
                ops.push(json!({"remove": existing}));
            }
        }
        for tag in &mapped.add_tags {
            if !current.tags.iter().any(|t| t.eq_ignore_ascii_case(tag)) {
                ops.push(json!({"add": tag}));
            }
        }
        if !ops.is_empty() {
            let body = json!({"update": {"labels": ops}});
            let response = self.call(self.request("PUT", self.url(&format!("issue/{id}")), Some(body))).await?;
            self.check(response, &format!("labels of {id}"))?;
            changed = true;
        }
        if changed { self.get(id).await } else { Ok(current) }
    }

    async fn comment(&self, id: &str, markdown: &str) -> Result<()> {
        check_key(id)?;
        let body = json!({"body": markdown_to_adf(&cap_comment(markdown))});
        let url = self.url(&format!("issue/{id}/comment"));
        let response = self.call(self.request("POST", url, Some(body))).await?;
        self.check(response, id).map(|_| ())
    }

    async fn assign(&self, id: &str, who: Option<&str>) -> Result<()> {
        check_key(id)?;
        let account = match who {
            Some(who) => json!(self.account_id(who).await?),
            None => Value::Null,
        };
        let url = self.url(&format!("issue/{id}/assignee"));
        let response = self.call(self.request("PUT", url, Some(json!({"accountId": account})))).await?;
        self.check(response, id).map(|_| ())
    }
}

fn valid_project_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Issue keys look like `ABC-12`; anything else never reaches a URL.
fn check_key(id: &str) -> Result<()> {
    let ok = id.split_once('-').is_some_and(|(project, number)| {
        valid_project_key(project) && !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
    });
    if ok { Ok(()) } else { Err(TrackerError::NotFound(id.to_owned())) }
}

fn cap_comment(markdown: &str) -> String {
    if markdown.len() <= MAX_COMMENT_BYTES {
        return markdown.to_owned();
    }
    let mut end = MAX_COMMENT_BYTES - 64;
    while !markdown.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n\n_[truncated by Varda: comment exceeded 100 KB]_", &markdown[..end])
}

fn jql_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// JQL for a filter; `filter.jql` (or `wiql`) wins verbatim when set.
pub fn build_jql(project_key: &str, filter: &TrackerFilter) -> String {
    for custom in [&filter.jql, &filter.wiql] {
        if let Some(jql) = custom.as_deref().filter(|j| !j.trim().is_empty()) {
            return jql.to_owned();
        }
    }
    let mut clauses = vec![format!("project = {}", jql_quote(project_key))];
    for tag in &filter.tags {
        clauses.push(format!("labels = {}", jql_quote(tag)));
    }
    if !filter.states.is_empty() {
        let states: Vec<String> = filter.states.iter().map(|s| jql_quote(s)).collect();
        clauses.push(format!("status in ({})", states.join(", ")));
    }
    format!("{} ORDER BY updated DESC", clauses.join(" AND "))
}

/// `2026-10-17T10:00:00.123+0000` (also `Z`, `+00:00`, no fraction) → epoch ms.
pub fn parse_jira_time_millis(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b' ') {
        return None;
    }
    let num = |a: usize, z: usize| s.get(a..z)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    let mut rest = &s[19..];
    let mut millis = 0;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits: String = frac.chars().take_while(char::is_ascii_digit).collect();
        millis = format!("{digits:0<3}")[..3].parse::<i64>().ok()?;
        rest = &frac[digits.len()..];
    }
    let offset_min = match rest.as_bytes().first() {
        None | Some(b'Z') => 0,
        Some(sign @ (b'+' | b'-')) => {
            let tz: String = rest[1..].chars().filter(char::is_ascii_digit).collect();
            let (oh, om) = (tz.get(0..2)?.parse::<i64>().ok()?, tz.get(2..4).unwrap_or("00").parse::<i64>().ok()?);
            (oh * 60 + om) * if *sign == b'-' { -1 } else { 1 }
        }
        _ => return None,
    };
    if !super::civil_time_is_valid(y, mo, d, h, mi, sec, offset_min) {
        return None;
    }
    // Howard Hinnant's days-from-civil.
    let y = y - i64::from(mo <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h * 3600 + mi * 60 + sec - offset_min * 60;
    u64::try_from(secs * 1000 + millis).ok()
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Mutex;

    use super::*;
    use crate::config::{CredentialConfig, TrackerKind};
    use crate::tracker::azdo::StdHttpTransport;

    const SEARCH: &str = include_str!("../../testdata/jira/search.json");
    const SEARCH_PAGE2: &str = include_str!("../../testdata/jira/search_page2.json");
    const ISSUE: &str = include_str!("../../testdata/jira/issue.json");
    const TRANSITIONS: &str = include_str!("../../testdata/jira/transitions.json");
    const STATUSES: &str = include_str!("../../testdata/jira/project_statuses.json");
    const MYSELF: &str = include_str!("../../testdata/jira/myself.json");

    type Reply = (u16, String);

    /// Serve `replies` one per connection, recording each raw request.
    fn fake_server(replies: Vec<Reply>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for (status, body) in replies {
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
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(format!("{head}{body}").as_bytes());
            }
        });
        (base, seen)
    }

    fn config(base: &str) -> TrackerConfig {
        let mut extra = BTreeMap::new();
        extra.insert("site".to_owned(), base.to_owned());
        extra.insert("project_key".to_owned(), "VAR".to_owned());
        extra.insert("email".to_owned(), "me@example.com".to_owned());
        TrackerConfig {
            name: "jira-test".to_owned(),
            kind: TrackerKind::Jira,
            project_path: "/tmp/proj".to_owned(),
            credential: CredentialConfig { from_env: Some("T".to_owned()), ..Default::default() },
            filter: TrackerFilter::default(),
            status_map: super::super::status_map_from(&[
                ("backlog", "To Do"),
                ("ready", "To Do"),
                ("running", "In Progress"),
                ("review", "In Review"),
                ("needs_user", "In Progress"),
                ("failed", "In Progress"),
                ("done", "Done"),
            ]),
            acknowledged: None,
            extra,
        }
    }

    fn tracker(base: &str) -> JiraTracker {
        let c = config(base);
        JiraTracker::new(
            "jira-test".to_owned(),
            base.to_owned(),
            "VAR".to_owned(),
            "me@example.com".to_owned(),
            "s3cret-token".to_owned(),
            Arc::new(StdHttpTransport),
        )
        .with_config(c)
    }

    fn body_of(raw: &str) -> Value {
        serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap()).unwrap()
    }

    #[test]
    fn time_parsing() {
        assert_eq!(parse_jira_time_millis("1970-01-01T00:00:00.000+0000"), Some(0));
        assert_eq!(parse_jira_time_millis("2026-10-17T10:00:00.123+0000"), Some(1_792_231_200_123));
        assert_eq!(
            parse_jira_time_millis("2026-10-17T12:00:00.123+0200"),
            parse_jira_time_millis("2026-10-17T10:00:00.123Z")
        );
        assert_eq!(parse_jira_time_millis("2026-10-17T10:00:00-05:30"), Some(1_792_231_200_000 + 19_800_000));
        assert_eq!(parse_jira_time_millis("garbage"), None);
    }

    #[test]
    fn jql_building_and_passthrough() {
        let f = TrackerFilter {
            tags: vec!["varda".to_owned()],
            states: vec!["To Do".to_owned(), "In \"Progress\"".to_owned()],
            ..Default::default()
        };
        assert_eq!(
            build_jql("VAR", &f),
            "project = \"VAR\" AND labels = \"varda\" AND status in (\"To Do\", \"In \\\"Progress\\\"\") ORDER BY updated DESC"
        );
        let custom = TrackerFilter { jql: Some("assignee = currentUser()".to_owned()), ..Default::default() };
        assert_eq!(build_jql("VAR", &custom), "assignee = currentUser()");
        let wiql = TrackerFilter { wiql: Some("x = 1".to_owned()), ..Default::default() };
        assert_eq!(build_jql("VAR", &wiql), "x = 1");
    }

    #[test]
    fn from_config_requires_settings() {
        let mut c = config("https://x.atlassian.net");
        assert!(JiraTracker::from_config(&c, "t".to_owned()).is_ok());
        for key in ["site", "project_key", "email"] {
            let mut bad = c.clone();
            bad.extra.remove(key);
            let err = JiraTracker::from_config(&bad, "t".to_owned()).err().unwrap();
            assert!(err.to_string().contains(key), "{err}");
        }
        c.extra.insert("project_key".to_owned(), "a b".to_owned());
        assert!(JiraTracker::from_config(&c, "t".to_owned()).is_err());
    }

    #[tokio::test]
    async fn query_paginates_with_next_page_token_and_maps_issues() {
        let (base, seen) = fake_server(vec![(200, SEARCH.to_owned()), (200, SEARCH_PAGE2.to_owned())]);
        let t = tracker(&base);
        let filter = TrackerFilter { tags: vec!["varda".to_owned()], ..Default::default() };
        let issues = t.query(&filter).await.unwrap();
        assert_eq!(issues.len(), 3);
        let first = &issues[0];
        assert_eq!(first.id, "VAR-12");
        assert_eq!(first.url, format!("{base}/browse/VAR-12"));
        assert_eq!(first.title, "Add retry to the sync job");
        assert_eq!(first.remote_state, "To Do");
        assert_eq!(first.tags, vec!["varda", "backend"]);
        assert_eq!(first.assignee.as_deref(), Some("dev@example.com"));
        assert_eq!(first.rev, parse_jira_time_millis("2026-10-17T10:00:00.123+0000").unwrap());
        assert!(first.body_markdown.contains("## Acceptance criteria"), "{}", first.body_markdown);
        assert!(first.body_markdown.contains("- Retries use **exponential** backoff"), "{}", first.body_markdown);
        assert_eq!(issues[1].assignee, None);
        assert_eq!(issues[2].body_markdown, "");

        let seen = seen.lock().unwrap();
        assert!(seen[0].starts_with("POST /rest/api/3/search/jql "), "{}", seen[0]);
        assert!(seen[0].contains("Authorization: Basic bWVAZXhhbXBsZS5jb206czNjcmV0LXRva2Vu"), "{}", seen[0]);
        let first_body = body_of(&seen[0]);
        assert_eq!(first_body["jql"], "project = \"VAR\" AND labels = \"varda\" ORDER BY updated DESC");
        assert!(first_body.get("nextPageToken").is_none());
        assert_eq!(body_of(&seen[1])["nextPageToken"], "tok-page-2");
    }

    #[tokio::test]
    async fn get_maps_issue_and_bad_keys_are_not_found() {
        let (base, seen) = fake_server(vec![(200, ISSUE.to_owned()), (404, "{}".to_owned())]);
        let t = tracker(&base);
        let issue = t.get("VAR-7").await.unwrap();
        assert_eq!(issue.id, "VAR-7");
        assert!(issue.body_markdown.contains("```rust"), "{}", issue.body_markdown);
        assert!(issue.body_markdown.contains("| Name | Value |"), "{}", issue.body_markdown);
        assert!(matches!(t.get("../etc").await, Err(TrackerError::NotFound(_))));
        assert!(matches!(t.get("VAR-99").await, Err(TrackerError::NotFound(_))));
        assert!(seen.lock().unwrap()[0].starts_with("GET /rest/api/3/issue/VAR-7?fields="));
    }

    #[tokio::test]
    async fn transition_picks_by_target_status_then_adds_helper_label() {
        let (base, seen) = fake_server(vec![
            (200, ISSUE.to_owned()),
            (200, TRANSITIONS.to_owned()),
            (204, String::new()),
            (200, ISSUE.to_owned()),
        ]);
        let t = tracker(&base);
        let rev = parse_jira_time_millis("2026-10-17T10:00:00.123+0000").unwrap();
        t.transition("VAR-7", TaskStatus::Review, Some(rev)).await.unwrap();
        let seen = seen.lock().unwrap();
        assert!(seen[1].starts_with("GET /rest/api/3/issue/VAR-7/transitions"));
        assert!(seen[2].starts_with("POST /rest/api/3/issue/VAR-7/transitions"));
        assert_eq!(body_of(&seen[2]), json!({"transition": {"id": "31"}}));
        // Review has no helper label to add and the fixture has none to remove:
        // no label PUT, just the final re-read.
        assert_eq!(seen.len(), 4, "{seen:?}");
        assert!(seen[3].starts_with("GET /rest/api/3/issue/VAR-7?"), "{}", seen[3]);
    }

    #[tokio::test]
    async fn transition_without_matching_transition_lists_available() {
        let (base, _) = fake_server(vec![(200, ISSUE.to_owned()), (200, TRANSITIONS.to_owned())]);
        let t = tracker(&base);
        let err = t.transition("VAR-7", TaskStatus::Done, None).await.unwrap_err().to_string();
        assert!(err.contains("no transition to 'Done'"), "{err}");
        assert!(err.contains("Start progress (-> In Progress)") && err.contains("Request review (-> In Review)"), "{err}");
    }

    #[tokio::test]
    async fn transition_conflict_on_stale_rev_and_noop_when_already_there() {
        let (base, _) = fake_server(vec![(200, ISSUE.to_owned())]);
        let err = tracker(&base).transition("VAR-7", TaskStatus::Review, Some(1)).await.unwrap_err();
        assert!(err.is_conflict(), "{err}");

        // The fixture issue is 'In Progress' with no varda labels: moving to
        // running changes nothing and sends nothing further.
        let (base, seen) = fake_server(vec![(200, ISSUE.to_owned())]);
        let moved = tracker(&base).transition("VAR-7", TaskStatus::Running, None).await.unwrap();
        assert_eq!(moved.remote_state, "In Progress");
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn helper_label_added_and_stale_one_removed() {
        let (base, seen) = fake_server(vec![
            (200, {
                let mut v: Value = serde_json::from_str(ISSUE).unwrap();
                v["fields"]["labels"] = json!(["varda:failed", "varda"]);
                v.to_string()
            }),
            (204, String::new()),
            (200, ISSUE.to_owned()),
        ]);
        tracker(&base).transition("VAR-7", TaskStatus::NeedsUser, None).await.unwrap();
        let seen = seen.lock().unwrap();
        let body = body_of(&seen[1]);
        let ops = body["update"]["labels"].as_array().unwrap();
        assert!(ops.contains(&json!({"remove": "varda:failed"})), "{body}");
        assert!(ops.contains(&json!({"add": "varda:needs-user"})), "{body}");
    }

    #[tokio::test]
    async fn comment_posts_adf_and_create_assign_work() {
        let (base, seen) = fake_server(vec![(201, "{}".to_owned())]);
        tracker(&base).comment("VAR-7", "Done.\n\n```\ncargo test\n```\n\n- a\n- b").await.unwrap();
        let body = body_of(&seen.lock().unwrap()[0]);
        assert_eq!(body["body"]["type"], "doc");
        assert_eq!(body["body"]["content"][1]["type"], "codeBlock");
        assert_eq!(body["body"]["content"][2]["type"], "bulletList");

        let (base, seen) = fake_server(vec![(201, r#"{"key":"VAR-7"}"#.to_owned()), (200, ISSUE.to_owned())]);
        let draft = IssueDraft {
            title: "t".to_owned(),
            body_markdown: "hello".to_owned(),
            tags: vec!["varda".to_owned()],
            ..Default::default()
        };
        let created = tracker(&base).create(&draft).await.unwrap();
        assert_eq!(created.id, "VAR-7");
        let sent = body_of(&seen.lock().unwrap()[0]);
        assert_eq!(sent["fields"]["project"]["key"], "VAR");
        assert_eq!(sent["fields"]["issuetype"]["name"], "Task");
        assert_eq!(sent["fields"]["labels"], json!(["varda"]));

        let (base, seen) = fake_server(vec![(204, String::new()), (204, String::new())]);
        let t = tracker(&base);
        t.assign("VAR-7", Some("712020:abc")).await.unwrap();
        t.assign("VAR-7", None).await.unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(body_of(&seen[0]), json!({"accountId": "712020:abc"}));
        assert_eq!(body_of(&seen[1]), json!({"accountId": null}));
    }

    #[tokio::test]
    async fn statuses_myself_and_valid_states() {
        let (base, _) = fake_server(vec![
            (200, STATUSES.to_owned()),
            (200, MYSELF.to_owned()),
            (200, MYSELF.to_owned()),
        ]);
        let t = tracker(&base);
        assert_eq!(
            t.valid_states().await.unwrap().unwrap(),
            vec!["To Do", "In Progress", "In Review", "Done", "Blocked"]
        );
        assert_eq!(t.myself().await.unwrap(), "Dev Eloper");
        t.check_auth().await.unwrap();
    }

    #[tokio::test]
    async fn errors_never_contain_the_token() {
        let (base, _) = fake_server(vec![(401, "bad credentials s3cret-token".to_owned())]);
        let err = tracker(&base).check_auth().await.unwrap_err().to_string();
        assert!(err.contains("HTTP 401") && !err.contains("s3cret-token"), "{err}");
        // Unreachable server: transport error text is scrubbed too.
        let dead = tracker("http://127.0.0.1:1");
        let err = dead.check_auth().await.unwrap_err().to_string();
        assert!(!err.contains("s3cret-token"), "{err}");
    }

    /// Live smoke test: `VARDA_JIRA_LIVE=1 VARDA_JIRA_SITE=... VARDA_JIRA_EMAIL=...
    /// VARDA_JIRA_TOKEN=... VARDA_JIRA_PROJECT=...`.
    #[tokio::test]
    #[ignore]
    async fn live_query() {
        if std::env::var("VARDA_JIRA_LIVE").is_err() {
            return;
        }
        let var = |k: &str| std::env::var(k).unwrap();
        let site = var("VARDA_JIRA_SITE");
        let t = JiraTracker::new(
            "live".to_owned(),
            site.clone(),
            var("VARDA_JIRA_PROJECT"),
            var("VARDA_JIRA_EMAIL"),
            var("VARDA_JIRA_TOKEN"),
            transport_for(&site),
        );
        t.check_auth().await.unwrap();
        t.query(&TrackerFilter::default()).await.unwrap();
    }

    // Review #1236: empty pages that keep handing out tokens must terminate.
    #[tokio::test]
    async fn query_stops_on_repeated_page_token() {
        let page = r#"{"issues":[],"nextPageToken":"same","isLast":false}"#.to_owned();
        let (base, seen) = fake_server(vec![(200, page.clone()), (200, page.clone()), (200, page)]);
        let err = tracker(&base).query(&TrackerFilter::default()).await.unwrap_err();
        assert!(format!("{err:#}").contains("repeated nextPageToken"), "{err:#}");
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn query_is_bounded_by_page_count() {
        let replies = (0..MAX_PAGES)
            .map(|i| (200, format!(r#"{{"issues":[],"nextPageToken":"t{i}","isLast":false}}"#)))
            .collect();
        let (base, seen) = fake_server(replies);
        let err = tracker(&base).query(&TrackerFilter::default()).await.unwrap_err();
        assert!(format!("{err:#}").contains("more than"), "{err:#}");
        assert_eq!(seen.lock().unwrap().len(), MAX_PAGES);
    }

    #[test]
    fn issue_without_updated_is_rejected() {
        let t = tracker("http://127.0.0.1:9");
        let item = json!({"key": "VAR-1", "fields": {"summary": "x", "status": {"name": "To Do"}}});
        let err = t.map_issue(&item).unwrap_err().to_string();
        assert!(err.contains("updated"), "{err}");
    }

    #[test]
    fn http_error_redacts_before_truncating() {
        let t = tracker("http://127.0.0.1:9");
        let body = format!("{}s3cret-token tail", "x".repeat(295));
        let response = HttpResponse { status: 500, headers: vec![], body: body.into_bytes() };
        let err = format!("{:#}", anyhow::Error::from(t.http_error("probe", &response)));
        assert!(!err.contains("s3cr"), "{err}");
    }

    #[test]
    fn impossible_timestamps_are_rejected() {
        for bad in ["2026-99-99T09:00:00.000+0000", "2026-02-30T09:00:00.000+0000", "2026-10-01T24:00:00.000+0000", "2026-10-01T09:00:00.000+1500"] {
            assert_eq!(parse_jira_time_millis(bad), None, "{bad}");
        }
        let t = tracker("http://127.0.0.1:9");
        let item = json!({"key": "VAR-1", "fields": {"updated": "2026-13-01T09:00:00.000+0000", "status": {"name": "To Do"}}});
        assert!(t.map_issue(&item).unwrap_err().to_string().contains("updated"));
    }
}
