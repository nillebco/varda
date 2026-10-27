//! Atlassian Document Format (ADF) <-> markdown, for the Jira backend.
//!
//! `adf_to_markdown` handles the common nodes (paragraph, heading, lists,
//! codeBlock, blockquote, rule, table, text marks strong/em/code/strike/link,
//! hardBreak, mention/emoji/inlineCard/status); an unknown node degrades to the
//! text of its children. `markdown_to_adf` covers what comments and created
//! issues need: paragraphs, headings, fenced code, (nested) lists, blockquotes,
//! tables, rules and the inline marks above. No crate: both directions are
//! small hand-rolled passes bounded by [`MAX_DEPTH`].

use serde_json::{Value, json};

/// Nesting limit for lists, quotes and inline marks (guards the recursion).
const MAX_DEPTH: usize = 12;

// ---------------------------------------------------------------------------
// ADF -> markdown
// ---------------------------------------------------------------------------

/// Render an ADF document (or any node) as markdown. Non-ADF input (a plain
/// string, `null`) yields its text or the empty string.
pub fn adf_to_markdown(doc: &Value) -> String {
    match doc {
        Value::String(s) => s.clone(),
        Value::Object(_) => blocks(doc, 0).trim_matches('\n').to_owned(),
        _ => String::new(),
    }
}

fn children(node: &Value) -> &[Value] {
    node.get("content")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn node_type(node: &Value) -> &str {
    node.get("type").and_then(Value::as_str).unwrap_or("")
}

/// Render the children of `node` as blocks separated by blank lines.
fn blocks(node: &Value, depth: usize) -> String {
    let mut parts = Vec::new();
    if node_type(node) == "doc" || node.get("content").is_some() && !is_inline(node) {
        for child in children(node) {
            let rendered = block(child, depth);
            if !rendered.trim().is_empty() {
                parts.push(rendered);
            }
        }
    } else {
        parts.push(block(node, depth));
    }
    parts.join("\n\n")
}

fn is_inline(node: &Value) -> bool {
    matches!(
        node_type(node),
        "text" | "hardBreak" | "mention" | "emoji" | "inlineCard" | "status" | "date"
    )
}

fn block(node: &Value, depth: usize) -> String {
    if depth > MAX_DEPTH {
        return inline_text(node);
    }
    match node_type(node) {
        "paragraph" => escape_line_start(&inlines(children(node))),
        "heading" => {
            let level = node
                .pointer("/attrs/level")
                .and_then(Value::as_u64)
                .unwrap_or(1)
                .clamp(1, 6) as usize;
            format!("{} {}", "#".repeat(level), inlines(children(node)))
        }
        "bulletList" | "orderedList" => list(node, depth),
        "codeBlock" => {
            let lang = node
                .pointer("/attrs/language")
                .and_then(Value::as_str)
                .unwrap_or("");
            let text: String = children(node).iter().map(inline_text).collect();
            let fence = fence_for(&text);
            format!("{fence}{lang}\n{}\n{fence}", text.trim_end_matches('\n'))
        }
        "blockquote" | "panel" | "expand" | "nestedExpand" => {
            let inner = blocks(node, depth + 1);
            inner
                .lines()
                .map(|l| if l.is_empty() { ">".to_owned() } else { format!("> {l}") })
                .collect::<Vec<_>>()
                .join("\n")
        }
        "rule" => "---".to_owned(),
        "table" => table(node),
        "mediaSingle" | "mediaGroup" | "media" => {
            let alt = node
                .pointer("/attrs/alt")
                .and_then(Value::as_str)
                .or_else(|| node.pointer("/attrs/collection").and_then(Value::as_str));
            let inner = blocks(node, depth + 1);
            if !inner.is_empty() {
                inner
            } else {
                alt.map(|a| format!("[attachment: {a}]")).unwrap_or_default()
            }
        }
        // listItem/tableRow/... outside their parent, doc, and unknown nodes.
        _ => {
            if children(node).is_empty() {
                inline_text(node)
            } else if children(node).iter().any(|c| !is_inline(c)) {
                blocks(node, depth + 1)
            } else {
                inlines(children(node))
            }
        }
    }
}

/// A code fence longer than any backtick run inside `text`.
fn fence_for(text: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat(longest.max(2) + 1)
}

fn list(node: &Value, depth: usize) -> String {
    let ordered = node_type(node) == "orderedList";
    let start = node
        .pointer("/attrs/order")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    let mut lines = Vec::new();
    for (i, item) in children(node).iter().enumerate() {
        let marker = if ordered {
            format!("{}. ", start + i as u64)
        } else {
            "- ".to_owned()
        };
        let pad = " ".repeat(marker.len());
        // Item children: paragraphs/nested lists. Tight rendering: blocks are
        // joined by a single newline when they are a paragraph + nested list.
        let mut rendered: Vec<String> = Vec::new();
        for child in children(item) {
            let r = block(child, depth + 1);
            if !r.trim().is_empty() {
                rendered.push(r);
            }
        }
        let body = rendered.join("\n");
        let mut first = true;
        for line in body.lines() {
            if first {
                lines.push(format!("{marker}{line}"));
                first = false;
            } else if line.is_empty() {
                lines.push(String::new());
            } else {
                lines.push(format!("{pad}{line}"));
            }
        }
        if first {
            lines.push(marker.trim_end().to_owned());
        }
    }
    lines.join("\n")
}

fn table(node: &Value) -> String {
    let rows: Vec<Vec<String>> = children(node)
        .iter()
        .filter(|r| node_type(r) == "tableRow")
        .map(|row| {
            children(row)
                .iter()
                .map(|cell| {
                    let text = blocks(cell, 1);
                    text.split('\n')
                        .map(str::trim)
                        .filter(|l| !l.is_empty())
                        .collect::<Vec<_>>()
                        .join("<br>")
                        .replace('|', "\\|")
                })
                .collect()
        })
        .collect();
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    if width == 0 {
        return String::new();
    }
    let render = |row: &Vec<String>| {
        let mut cells = row.clone();
        cells.resize(width, String::new());
        format!("| {} |", cells.join(" | "))
    };
    let mut out = vec![render(&rows[0])];
    out.push(format!("|{}|", vec![" --- "; width].join("|")));
    out.extend(rows[1..].iter().map(render));
    out.join("\n")
}

fn inlines(nodes: &[Value]) -> String {
    nodes.iter().map(inline).collect()
}

/// Plain text of a node and its descendants (marks dropped).
fn inline_text(node: &Value) -> String {
    match node_type(node) {
        "text" => node.get("text").and_then(Value::as_str).unwrap_or("").to_owned(),
        "hardBreak" => "\n".to_owned(),
        _ => {
            if let Some(t) = node
                .pointer("/attrs/text")
                .or_else(|| node.pointer("/attrs/url"))
                .and_then(Value::as_str)
            {
                return t.to_owned();
            }
            children(node).iter().map(inline_text).collect()
        }
    }
}

fn inline(node: &Value) -> String {
    match node_type(node) {
        "text" => {
            let text = node.get("text").and_then(Value::as_str).unwrap_or("");
            apply_marks(text, node.get("marks").and_then(Value::as_array))
        }
        "hardBreak" => "  \n".to_owned(),
        "mention" => {
            let t = node.pointer("/attrs/text").and_then(Value::as_str).unwrap_or("");
            if t.starts_with('@') { t.to_owned() } else { format!("@{t}") }
        }
        "emoji" => node
            .pointer("/attrs/text")
            .or_else(|| node.pointer("/attrs/shortName"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        "inlineCard" => {
            let url = node.pointer("/attrs/url").and_then(Value::as_str).unwrap_or("");
            format!("<{url}>")
        }
        "status" => node
            .pointer("/attrs/text")
            .and_then(Value::as_str)
            .map(|t| format!("[{t}]"))
            .unwrap_or_default(),
        "date" => node
            .pointer("/attrs/timestamp")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        _ => {
            // Unknown inline/block hiding in a paragraph: keep its text.
            if children(node).is_empty() {
                inline_text(node)
            } else {
                inlines(children(node))
            }
        }
    }
}

fn apply_marks(text: &str, marks: Option<&Vec<Value>>) -> String {
    let marks: &[Value] = marks.map_or(&[], Vec::as_slice);
    let has = |name: &str| marks.iter().any(|m| node_type(m) == name);
    let href = marks
        .iter()
        .find(|m| node_type(m) == "link")
        .and_then(|m| m.pointer("/attrs/href"))
        .and_then(Value::as_str);
    if text.is_empty() {
        return String::new();
    }
    // Markers must hug the non-space text: `**x **` is not emphasis.
    let trimmed = text.trim();
    let lead = &text[..text.len() - text.trim_start().len()];
    let trail = &text[text.trim_end().len()..];
    if trimmed.is_empty() {
        return text.to_owned();
    }
    let mut out = if has("code") {
        wrap_code(trimmed)
    } else {
        let mut out = escape_text(trimmed);
        if has("em") {
            out = format!("*{out}*");
        }
        if has("strong") {
            out = format!("**{out}**");
        }
        if has("strike") {
            out = format!("~~{out}~~");
        }
        out
    };
    if let Some(href) = href {
        out = format!("[{out}]({})", href.replace(')', "%29").replace(' ', "%20"));
    }
    format!("{lead}{out}{trail}")
}

fn wrap_code(text: &str) -> String {
    let fence = {
        let mut longest = 0;
        let mut run = 0;
        for c in text.chars() {
            if c == '`' {
                run += 1;
                longest = longest.max(run);
            } else {
                run = 0;
            }
        }
        "`".repeat(longest + 1)
    };
    if text.starts_with('`') || text.ends_with('`') {
        format!("{fence} {text} {fence}")
    } else {
        format!("{fence}{text}{fence}")
    }
}

/// Backslash-escape the characters that would otherwise be read as inline
/// markdown. `_` is only escaped at a word boundary so `snake_case` survives.
fn escape_text(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    for (i, &c) in chars.iter().enumerate() {
        match c {
            '\\' | '*' | '`' | '[' | ']' | '~' | '<' => {
                out.push('\\');
                out.push(c);
            }
            '_' => {
                let prev = i.checked_sub(1).map(|j| chars[j]);
                let next = chars.get(i + 1).copied();
                let inner = prev.is_some_and(char::is_alphanumeric)
                    && next.is_some_and(char::is_alphanumeric);
                if !inner {
                    out.push('\\');
                }
                out.push('_');
            }
            _ => out.push(c),
        }
    }
    out
}

/// Keep a paragraph that starts like a block marker (`# `, `- `, `1. `, `> `)
/// from being re-read as a heading/list/quote.
fn escape_line_start(text: &str) -> String {
    let t = text.trim_start();
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && t[digits..].starts_with(['.', ')']) {
        let at = text.len() - t.len() + digits;
        return format!("{}\\{}", &text[..at], &text[at..]);
    }
    if t.starts_with(['#', '>', '-', '+', '|']) || t.starts_with("```") || t.starts_with("~~~") {
        format!("\\{text}")
    } else {
        text.to_owned()
    }
}

// ---------------------------------------------------------------------------
// markdown -> ADF
// ---------------------------------------------------------------------------

/// Convert markdown to an ADF document (`version` 1). Never fails: anything
/// not recognised stays paragraph text. Empty input yields one empty paragraph
/// (ADF forbids an empty `doc`).
pub fn markdown_to_adf(markdown: &str) -> Value {
    let lines: Vec<&str> = markdown.lines().collect();
    let mut content = parse_blocks(&lines, 0);
    if content.is_empty() {
        content.push(json!({"type": "paragraph", "content": []}));
    }
    json!({"type": "doc", "version": 1, "content": content})
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// `- x`, `* x`, `+ x` -> (marker width, rest); `1. x` / `1) x` likewise.
fn list_marker(line: &str) -> Option<(bool, usize, u64, &str)> {
    let t = line.trim_start_matches(' ');
    let lead = line.len() - t.len();
    let mut chars = t.chars();
    let first = chars.next()?;
    if matches!(first, '-' | '*' | '+') {
        let rest = &t[1..];
        if rest.is_empty() {
            return Some((false, lead + 2, 1, ""));
        }
        if rest.starts_with(' ') && !is_rule(t) {
            return Some((false, lead + 2, 1, rest.trim_start()));
        }
        return None;
    }
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || digits > 9 {
        return None;
    }
    let after = &t[digits..];
    if (after.starts_with(". ") || after.starts_with(") ") || after == "." || after == ")")
        && !after.is_empty()
    {
        let n: u64 = t[..digits].parse().ok()?;
        let rest = after[1..].trim_start();
        return Some((true, lead + digits + 2, n, rest));
    }
    None
}

fn is_rule(line: &str) -> bool {
    let t = line.trim();
    t.len() >= 3
        && ["-", "*", "_"].iter().any(|c| {
            t.chars().all(|x| x.to_string() == *c || x == ' ')
                && t.chars().filter(|x| x.to_string() == *c).count() >= 3
        })
}

fn fence_open(line: &str) -> Option<(char, usize, String)> {
    let t = line.trim_start();
    let c = t.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let n = t.chars().take_while(|x| *x == c).count();
    (n >= 3).then(|| (c, n, t[n..].trim().to_owned()))
}

fn heading_of(line: &str) -> Option<(usize, &str)> {
    let t = line.trim_start();
    let n = t.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&n) && (t.len() == n || t[n..].starts_with(' ')) {
        Some((n, t[n..].trim().trim_end_matches('#').trim_end()))
    } else {
        None
    }
}

fn table_cells(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').filter(|_| !t.ends_with("\\|")).unwrap_or(t);
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek() == Some(&'|') {
            cur.push('|');
            chars.next();
        } else if c == '|' {
            cells.push(cur.trim().to_owned());
            cur.clear();
        } else {
            cur.push(c);
        }
    }
    cells.push(cur.trim().to_owned());
    cells
}

fn is_table_separator(line: &str) -> bool {
    line.contains('-')
        && line.contains('|')
        && line.trim().chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

fn parse_blocks(lines: &[&str], depth: usize) -> Vec<Value> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() {
            i += 1;
        } else if let Some((ch, n, lang)) = fence_open(line) {
            let mut code = Vec::new();
            i += 1;
            while i < lines.len() {
                let t = lines[i].trim();
                if t.len() >= n && t.chars().all(|c| c == ch) {
                    break;
                }
                code.push(lines[i]);
                i += 1;
            }
            i += 1; // closing fence (or end of input: unterminated block)
            let mut node = json!({"type": "codeBlock"});
            if !lang.is_empty() {
                node["attrs"] = json!({"language": lang});
            }
            let text = code.join("\n");
            if !text.is_empty() {
                node["content"] = json!([{"type": "text", "text": text}]);
            }
            out.push(node);
        } else if let Some((level, text)) = heading_of(line) {
            let mut node = json!({"type": "heading", "attrs": {"level": level}});
            let inl = parse_inline(text, &[], 0);
            if !inl.is_empty() {
                node["content"] = Value::Array(inl);
            }
            out.push(node);
            i += 1;
        } else if is_rule(line) && list_marker(line).is_none() {
            out.push(json!({"type": "rule"}));
            i += 1;
        } else if line.trim_start().starts_with('>') && depth < MAX_DEPTH {
            let mut inner = Vec::new();
            while i < lines.len() && lines[i].trim_start().starts_with('>') {
                let t = lines[i].trim_start()[1..].strip_prefix(' ').unwrap_or(&lines[i].trim_start()[1..]);
                inner.push(t);
                i += 1;
            }
            let mut content = parse_blocks(&inner, depth + 1);
            if content.is_empty() {
                content.push(json!({"type": "paragraph", "content": []}));
            }
            out.push(json!({"type": "blockquote", "content": content}));
        } else if line.contains('|')
            && i + 1 < lines.len()
            && is_table_separator(lines[i + 1])
        {
            let header = table_cells(line);
            let mut rows = vec![table_row(&header, "tableHeader", header.len())];
            i += 2;
            while i < lines.len() && lines[i].contains('|') && !lines[i].trim().is_empty() {
                rows.push(table_row(&table_cells(lines[i]), "tableCell", header.len()));
                i += 1;
            }
            out.push(json!({"type": "table", "attrs": {"isNumberColumnEnabled": false, "layout": "default"}, "content": rows}));
        } else if list_marker(line).is_some() && depth < MAX_DEPTH {
            let (node, next) = parse_list(lines, i, depth);
            out.push(node);
            i = next;
        } else {
            // Paragraph: consecutive lines until a blank line or a block start.
            let start = i;
            i += 1;
            while i < lines.len() && !starts_block(lines, i) {
                i += 1;
            }
            out.push(paragraph(&lines[start..i]));
        }
    }
    out
}

fn starts_block(lines: &[&str], i: usize) -> bool {
    let line = lines[i];
    line.trim().is_empty()
        || fence_open(line).is_some()
        || heading_of(line).is_some()
        || is_rule(line)
        || line.trim_start().starts_with('>')
        || list_marker(line).is_some()
}

fn paragraph(lines: &[&str]) -> Value {
    let mut content = Vec::new();
    for (n, line) in lines.iter().enumerate() {
        if n > 0 {
            content.push(json!({"type": "hardBreak"}));
        }
        content.extend(parse_inline(line.trim(), &[], 0));
    }
    json!({"type": "paragraph", "content": content})
}

fn table_row(cells: &[String], kind: &str, width: usize) -> Value {
    let cells: Vec<Value> = (0..width.max(1))
        .map(|n| {
            let text = cells.get(n).map_or("", String::as_str);
            let inline: Vec<Value> = text
                .split("<br>")
                .enumerate()
                .flat_map(|(k, part)| {
                    let mut v = Vec::new();
                    if k > 0 {
                        v.push(json!({"type": "hardBreak"}));
                    }
                    v.extend(parse_inline(part.trim(), &[], 0));
                    v
                })
                .collect();
            json!({"type": kind, "attrs": {}, "content": [{"type": "paragraph", "content": inline}]})
        })
        .collect();
    json!({"type": "tableRow", "content": cells})
}

/// Parse the list starting at `lines[start]`; returns the node and the index
/// of the first line after it.
fn parse_list(lines: &[&str], start: usize, depth: usize) -> (Value, usize) {
    let (ordered, _, first_number, _) = list_marker(lines[start]).expect("list start");
    let base = indent_of(lines[start]);
    let mut items = Vec::new();
    let mut i = start;
    while i < lines.len() {
        let line = lines[i];
        let Some((is_ordered, width, _, rest)) = list_marker(line) else { break };
        if indent_of(line) != base || is_ordered != ordered {
            break;
        }
        // Item body: this line's text plus following lines indented deeper,
        // blank lines included when more indented content follows.
        let mut item_lines: Vec<&str> = vec![rest];
        i += 1;
        while i < lines.len() {
            let next = lines[i];
            if next.trim().is_empty() {
                let more = lines[i + 1..].iter().find(|l| !l.trim().is_empty());
                if more.is_some_and(|l| indent_of(l) >= width) {
                    item_lines.push("");
                    i += 1;
                    continue;
                }
                break;
            }
            if indent_of(next) >= width.max(base + 1) {
                item_lines.push(&next[width.min(indent_of(next))..]);
                i += 1;
            } else if list_marker(next).is_none() && !starts_block(lines, i) {
                item_lines.push(next.trim_start()); // lazy continuation
                i += 1;
            } else {
                break;
            }
        }
        let mut content = parse_blocks(&item_lines, depth + 1);
        if content.is_empty() {
            content.push(json!({"type": "paragraph", "content": []}));
        }
        items.push(json!({"type": "listItem", "content": content}));
    }
    let node = if ordered {
        json!({"type": "orderedList", "attrs": {"order": first_number}, "content": items})
    } else {
        json!({"type": "bulletList", "content": items})
    };
    (node, i)
}

fn text_node(text: &str, marks: &[Value]) -> Value {
    let mut node = json!({"type": "text", "text": text});
    if !marks.is_empty() {
        node["marks"] = Value::Array(marks.to_vec());
    }
    node
}

fn mark(kind: &str) -> Value {
    json!({"type": kind})
}

fn push_text(out: &mut Vec<Value>, buf: &mut String, marks: &[Value]) {
    if !buf.is_empty() {
        out.push(text_node(buf, marks));
        buf.clear();
    }
}

/// Position of the first `delim` in `chars[from..]` that is not escaped.
fn find_closing(chars: &[char], from: usize, delim: &[char]) -> Option<usize> {
    let mut i = from;
    while i + delim.len() <= chars.len() {
        if chars[i] == '\\' {
            i += 2;
            continue;
        }
        if chars[i] == '`' && delim != ['`'] {
            // Skip over a code span so its contents never close a mark.
            if let Some(end) = chars[i + 1..].iter().position(|c| *c == '`') {
                i += end + 2;
                continue;
            }
        }
        if &chars[i..i + delim.len()] == delim && i > from {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Closing emphasis delimiter (`len` x `c`) at or after `from`: a run of `c`
/// at least `len` long, preceded by non-space; the LAST `len` chars of the run
/// close, so `**b *c***` closes strong after the inner em's own marker.
fn find_close_emph(chars: &[char], from: usize, c: char, len: usize) -> Option<usize> {
    let mut i = from;
    while i < chars.len() {
        if chars[i] == '\\' {
            i += 2;
            continue;
        }
        if chars[i] == '`' {
            let n = chars[i..].iter().take_while(|x| **x == '`').count();
            if let Some(end) = find_closing(chars, i + n, &vec!['`'; n]) {
                i = end + n;
                continue;
            }
            i += n;
            continue;
        }
        if chars[i] == c {
            let run = chars[i..].iter().take_while(|x| **x == c).count();
            let after = chars.get(i + run);
            let ok = run >= len
                && i > from
                && !chars[i - 1].is_whitespace()
                && !(c == '_' && after.is_some_and(|a| a.is_alphanumeric()));
            if ok {
                return Some(i + run - len);
            }
            i += run;
            continue;
        }
        i += 1;
    }
    None
}

fn parse_inline(text: &str, marks: &[Value], depth: usize) -> Vec<Value> {
    let chars: Vec<char> = text.chars().collect();
    inline_from(&chars, marks, depth)
}

fn inline_from(chars: &[char], marks: &[Value], depth: usize) -> Vec<Value> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut i = 0;
    let has_code_incompatible = marks.iter().any(|m| node_type(m) == "code");
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            '\\' if next.is_some_and(|n| n.is_ascii_punctuation()) => {
                buf.push(next.unwrap());
                i += 2;
            }
            '`' => {
                let n = chars[i..].iter().take_while(|x| **x == '`').count();
                let delim = vec!['`'; n];
                if let Some(end) = find_closing(chars, i + n, &delim) {
                    let inner: String = chars[i + n..end].iter().collect();
                    let inner = if inner.len() > 2 && inner.starts_with(' ') && inner.ends_with(' ') {
                        inner[1..inner.len() - 1].to_owned()
                    } else {
                        inner
                    };
                    push_text(&mut out, &mut buf, marks);
                    // `code` excludes every mark but `link`.
                    let mut m: Vec<Value> =
                        marks.iter().filter(|m| node_type(m) == "link").cloned().collect();
                    m.push(mark("code"));
                    out.push(text_node(&inner, &m));
                    i = end + n;
                } else {
                    buf.extend(std::iter::repeat_n('`', n));
                    i += n;
                }
            }
            '*' | '~' | '_' if depth < MAX_DEPTH => {
                let run = chars[i..].iter().take_while(|x| **x == c).count();
                let (len, kinds): (usize, &[&str]) = match (c, run) {
                    ('*', r) if r >= 3 => (3, &["strong", "em"]),
                    ('*', 2) => (2, &["strong"]),
                    ('~', r) if r >= 2 => (2, &["strike"]),
                    ('*', _) | ('_', _) => (1, &["em"]),
                    _ => (0, &[]),
                };
                let prev_alnum = i > 0 && chars[i - 1].is_alphanumeric();
                let opens = len > 0
                    && chars.get(i + len).is_some_and(|a| !a.is_whitespace())
                    && !(c == '_' && (prev_alnum || run > 1));
                let close = opens.then(|| find_close_emph(chars, i + len, c, len)).flatten();
                if let Some(end) = close {
                    push_text(&mut out, &mut buf, marks);
                    let mut m = marks.to_vec();
                    if !has_code_incompatible {
                        for kind in kinds {
                            if !m.iter().any(|x| node_type(x) == *kind) {
                                m.push(mark(kind));
                            }
                        }
                    }
                    out.extend(inline_from(&chars[i + len..end], &m, depth + 1));
                    i = end + len;
                } else {
                    buf.extend(std::iter::repeat_n(c, run.max(1)));
                    i += run.max(1);
                }
            }
            '[' if depth < MAX_DEPTH => {
                // [text](url)
                let close = find_closing(chars, i + 1, &[']']);
                let link = close.filter(|&e| chars.get(e + 1) == Some(&'(')).and_then(|e| {
                    chars[e + 2..]
                        .iter()
                        .position(|x| *x == ')')
                        .map(|p| (e, e + 2 + p))
                });
                match link {
                    Some((e, p)) => {
                        let href: String = chars[e + 2..p].iter().collect();
                        let href = href.trim().to_owned();
                        push_text(&mut out, &mut buf, marks);
                        let mut m = marks.to_vec();
                        m.push(json!({"type": "link", "attrs": {"href": href}}));
                        out.extend(inline_from(&chars[i + 1..e], &m, depth + 1));
                        i = p + 1;
                    }
                    None => {
                        buf.push('[');
                        i += 1;
                    }
                }
            }
            '<' => {
                // <https://...> autolink
                let end = chars[i + 1..].iter().position(|x| *x == '>');
                let url: Option<String> = end.map(|p| chars[i + 1..i + 1 + p].iter().collect());
                match (end, url) {
                    (Some(p), Some(u)) if u.starts_with("http://") || u.starts_with("https://") => {
                        push_text(&mut out, &mut buf, marks);
                        let mut m = marks.to_vec();
                        m.push(json!({"type": "link", "attrs": {"href": u}}));
                        out.push(text_node(&u, &m));
                        i += p + 2;
                    }
                    _ => {
                        buf.push('<');
                        i += 1;
                    }
                }
            }
            _ => {
                buf.push(c);
                i += 1;
            }
        }
    }
    push_text(&mut out, &mut buf, marks);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = include_str!("../../testdata/jira/description.adf.json");

    fn doc() -> Value {
        serde_json::from_str(DOC).unwrap()
    }

    #[test]
    fn realistic_document_renders_to_markdown() {
        let md = adf_to_markdown(&doc());
        let expected = "\
## Context

The sync job fails on *transient* errors. See [the runbook](https://wiki.example.com/runbook) and call `retry()`.

Must not **lose data**, even with ***both*** flags.

## Acceptance criteria

- Retries use **exponential** backoff
  - max 5 attempts
  - jitter of 10%
- Failures are logged

1. Reproduce locally
2. Write a failing test
3. Fix it

```rust
fn retry() {
    for _ in 0..5 {
        // try
    }
}
```

> Note: flaky on CI only.

| Name | Value |
| --- | --- |
| attempts | 5 |
| backoff | 2^n s |

---

First line  
second line, cc @Dev Eloper.

> Unknown containers keep their text.";
        assert_eq!(md, expected);
    }

    #[test]
    fn markdown_round_trip_is_stable_on_a_realistic_document() {
        let md1 = adf_to_markdown(&doc());
        let adf2 = markdown_to_adf(&md1);
        let md2 = adf_to_markdown(&adf2);
        assert_eq!(md1, md2);
        // And the rebuilt ADF has the structure Jira needs.
        let types: Vec<&str> = adf2["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            types,
            [
                "heading", "paragraph", "paragraph", "heading", "bulletList", "orderedList",
                "codeBlock", "blockquote", "table", "rule", "paragraph", "blockquote"
            ]
        );
        assert_eq!(adf2["content"][4]["content"][0]["content"][1]["type"], "bulletList");
        assert_eq!(adf2["content"][8]["content"][0]["content"][0]["type"], "tableHeader");
        assert_eq!(adf2["content"][6]["attrs"]["language"], "rust");
    }

    #[test]
    fn marks_and_links_survive_markdown_to_adf() {
        let adf = markdown_to_adf("a **b *c*** `d` ~~e~~ [f](https://x.y/z) <https://q.r>");
        let nodes = adf["content"][0]["content"].as_array().unwrap();
        let find = |t: &str| nodes.iter().find(|n| n["text"] == t).unwrap_or_else(|| panic!("{t} in {nodes:?}"));
        assert_eq!(find("b ")["marks"], json!([{"type": "strong"}]));
        assert_eq!(find("c")["marks"], json!([{"type": "strong"}, {"type": "em"}]));
        assert_eq!(find("d")["marks"], json!([{"type": "code"}]));
        assert_eq!(find("e")["marks"], json!([{"type": "strike"}]));
        assert_eq!(find("f")["marks"][0]["attrs"]["href"], "https://x.y/z");
        assert_eq!(find("https://q.r")["marks"][0]["attrs"]["href"], "https://q.r");
    }

    #[test]
    fn plain_text_with_markdown_characters_round_trips() {
        for text in [
            "snake_case and 2 * 3 = 6",
            "use [brackets] and `ticks` and \\ backslash",
            "# not a heading",
            "- not a list",
            "1. not an ordered list",
            "> not a quote",
            "~~ not strike",
        ] {
            let adf = json!({"type": "doc", "version": 1, "content": [
                {"type": "paragraph", "content": [{"type": "text", "text": text}]}]});
            let md = adf_to_markdown(&adf);
            let back = markdown_to_adf(&md);
            let node = &back["content"][0];
            assert_eq!(node["type"], "paragraph", "{text:?} -> {md:?}");
            let got: String = node["content"]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| n["text"].as_str().unwrap())
                .collect();
            assert_eq!(got, text, "{md:?}");
        }
    }

    #[test]
    fn code_block_with_backticks_and_empty_inputs() {
        let adf = markdown_to_adf("````\n```\ninner\n```\n````");
        assert_eq!(adf["content"][0]["content"][0]["text"], "```\ninner\n```");
        let md = adf_to_markdown(&adf);
        assert_eq!(markdown_to_adf(&md), adf);

        let empty = markdown_to_adf("");
        assert_eq!(empty["content"], json!([{"type": "paragraph", "content": []}]));
        assert_eq!(adf_to_markdown(&Value::Null), "");
        assert_eq!(adf_to_markdown(&json!("plain")), "plain");
    }

    #[test]
    fn unknown_nodes_degrade_to_text_and_never_panic() {
        let adf = json!({"type": "doc", "content": [
            {"type": "weirdBlock", "content": [
                {"type": "paragraph", "content": [{"type": "text", "text": "inside"}]}]},
            {"type": "paragraph", "content": [
                {"type": "futureInline", "content": [{"type": "text", "text": "kept"}]},
                {"type": "status", "attrs": {"text": "IN REVIEW"}},
                {"type": "emoji", "attrs": {"shortName": ":tada:"}}]},
            {"type": "mediaSingle", "content": [{"type": "media", "attrs": {"alt": "shot.png"}}]},
            {"type": "orderedList", "attrs": {"order": 3}, "content": [
                {"type": "listItem", "content": [
                    {"type": "paragraph", "content": [{"type": "text", "text": "x"}]},
                    {"type": "codeBlock", "content": [{"type": "text", "text": "y"}]}]}]}]});
        let md = adf_to_markdown(&adf);
        assert!(md.contains("inside") && md.contains("kept[IN REVIEW]:tada:"), "{md}");
        assert!(md.contains("[attachment: shot.png]"), "{md}");
        assert!(md.contains("3. x\n   ```\n   y\n   ```"), "{md}");
    }

    #[test]
    fn deep_nesting_is_bounded() {
        let md = format!("{}x", "> ".repeat(500));
        let adf = markdown_to_adf(&md);
        assert_eq!(adf["type"], "doc");
        let list = format!("{}- x", "  ".repeat(300));
        let _ = markdown_to_adf(&list);
        let emph = format!("{}x{}", "*a ".repeat(200), "*".repeat(200));
        let _ = markdown_to_adf(&emph);
    }

    #[test]
    fn table_cells_escape_pipes_and_pad_short_rows() {
        let adf = markdown_to_adf("| a | b |\n| --- | --- |\n| x \\| y |\n");
        let row = &adf["content"][0]["content"][1];
        assert_eq!(row["content"].as_array().unwrap().len(), 2);
        assert_eq!(row["content"][0]["content"][0]["content"][0]["text"], "x | y");
        assert!(adf_to_markdown(&adf).contains("| x \\| y |  |"));
    }
}
