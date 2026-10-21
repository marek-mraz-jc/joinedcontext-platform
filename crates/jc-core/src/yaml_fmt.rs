//! One canonical layout for manifest YAML (T-5052): `jcctl fmt` and every Portal commit write
//! through it, so a hand edit and a Portal change look the same in a git diff.
//!
//! The layout: a manifest's sections in the order `apiVersion`, `kind`, `metadata` (`name`,
//! `namespace`, `title`, `description`, `labels`, then the rest), `spec`, then the rest; every
//! other map keeps its source order. Two spaces per level, sequences indented under their key.
//! Below a manifest's own sections, a map of at most three scalars becomes one flow line
//! `{ a: b }` and a list of scalars `[a, b]` when the line fits 100 columns; everything else is
//! block. Quotes only where YAML needs them (a string YAML 1.1 would read as another type keeps
//! its quotes). Comments stay where they stand and a run of blank lines becomes one.
//!
//! The formatter reads the block subset manifests are written in and refuses what it does not
//! (anchors, aliases, tags, complex keys, comments inside a flow collection). It never changes a
//! value: an output that does not parse to the same documents as its input is an error.

use serde::Deserialize;
use serde_norway::Value;

/// The widest line a flow collection may make.
const WIDTH: usize = 100;
/// The most keys a map written as one flow line holds.
const FLOW_KEYS: usize = 3;
/// The order of a manifest's own sections; the rest follow in source order.
const TOP_ORDER: &[&str] = &["apiVersion", "kind", "metadata", "spec"];
/// The order of `metadata`'s fields; the rest follow in source order.
const METADATA_ORDER: &[&str] = &["name", "namespace", "title", "description", "labels"];
/// Words YAML 1.1 readers (PyYAML, older Go) take for a boolean or null: kept quoted.
const RESERVED: &[&str] = &[
    "y", "n", "yes", "no", "on", "off", "true", "false", "null", "~",
];

/// Why a text could not be formatted. The file is then left as it is.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum FmtError {
    /// The input is not YAML.
    #[error("not valid YAML: {0}")]
    Parse(String),
    /// The input uses YAML the formatter does not rewrite.
    #[error("line {line}: {what} is not supported by the formatter")]
    Unsupported {
        /// The line of the input, from 1.
        line: usize,
        /// What the line holds.
        what: &'static str,
    },
    /// The formatted text would mean something else: a formatter defect, never written.
    #[error("formatting would change a value; the file is left as it is")]
    Changed,
}

type Result<T> = std::result::Result<T, FmtError>;

/// A blank line (`None`) or a comment (the text after `#`) above an entry.
type Trivia = Vec<Option<String>>;

/// Formats `src` (one or more YAML documents) into the canonical layout.
pub fn format(src: &str) -> Result<String> {
    let before = documents(src)?;
    let mut out = String::new();
    for (i, part) in split_documents(src).iter().enumerate() {
        if i > 0 || part.marked {
            out.push_str("---\n");
        }
        out.push_str(&format_document(&part.lines)?);
    }
    if documents(&out)? != before {
        return Err(FmtError::Changed);
    }
    Ok(out)
}

/// The documents `src` holds, as values, to compare a text with its formatted form.
fn documents(src: &str) -> Result<Vec<Value>> {
    serde_norway::Deserializer::from_str(src)
        .map(|doc| Value::deserialize(doc).map_err(|e| FmtError::Parse(e.to_string())))
        .collect()
}

/// One document's source lines, numbered from the file's first line.
struct Part {
    marked: bool,
    lines: Vec<Src>,
}

fn split_documents(src: &str) -> Vec<Part> {
    let mut parts = vec![Part {
        marked: false,
        lines: Vec::new(),
    }];
    for (i, raw) in src.lines().enumerate() {
        let raw = raw.trim_end_matches('\r');
        if raw == "---" || raw.starts_with("--- ") {
            let first = parts.len() == 1 && parts[0].lines.iter().all(Src::is_trivia);
            if first {
                parts[0].marked = true;
            } else {
                parts.push(Part {
                    marked: true,
                    lines: Vec::new(),
                });
            }
            continue;
        }
        if let Some(p) = parts.last_mut() {
            p.lines.push(Src::new(i + 1, raw));
        }
    }
    parts
}

fn format_document(lines: &[Src]) -> Result<String> {
    let mut p = Parser {
        lines: lines.to_vec(),
        pos: 0,
    };
    let doc = p.document()?;
    let mut out = Out::default();
    out.document(doc);
    Ok(out.text)
}

/// A source line: its number, its indentation and the text after it.
#[derive(Clone)]
struct Src {
    no: usize,
    indent: usize,
    raw: String,
}

impl Src {
    fn new(no: usize, raw: &str) -> Self {
        let indent = raw.len() - raw.trim_start_matches(' ').len();
        Self {
            no,
            indent,
            raw: raw.to_string(),
        }
    }
    fn text(&self) -> &str {
        &self.raw[self.indent.min(self.raw.len())..]
    }
    fn is_blank(&self) -> bool {
        self.raw.trim().is_empty()
    }
    fn is_trivia(&self) -> bool {
        self.is_blank() || self.text().starts_with('#')
    }
}

/// A parsed node. Scalars keep their source text; a string read from quotes or from several
/// lines keeps the shortest text that means the same.
#[derive(Debug, Clone)]
enum Node {
    Null,
    Scalar {
        text: String,
        flow_safe: bool,
    },
    Block {
        header: String,
        explicit: Option<usize>,
        lines: Vec<String>,
    },
    Map(Vec<Entry>),
    Seq(Vec<Entry>),
    /// The comments closing a collection, under its last entry: only ever an entry's value.
    Tail,
}

/// A map entry or a sequence item (`key` empty), with the comments above it and on its line.
#[derive(Debug, Clone)]
struct Entry {
    lead: Trivia,
    key: String,
    comment: Option<String>,
    value: Node,
}

struct Document {
    root: Option<Node>,
    tail: Trivia,
}

struct Parser {
    lines: Vec<Src>,
    pos: usize,
}

fn unsupported(line: usize, what: &'static str) -> FmtError {
    FmtError::Unsupported { line, what }
}

impl Parser {
    fn document(&mut self) -> Result<Document> {
        let root = match self.peek_content().map(|l| l.indent) {
            Some(indent) => Some(self.node_at(indent)?),
            None => None,
        };
        let tail = self.trivia();
        if let Some(l) = self.lines.get(self.pos) {
            return Err(unsupported(l.no, "indentation that closes no block"));
        }
        Ok(Document {
            root: root.map(reorder_manifest),
            tail,
        })
    }

    /// The next line that is neither blank nor a comment, without moving past anything.
    fn peek_content(&self) -> Option<&Src> {
        self.lines[self.pos..].iter().find(|l| !l.is_trivia())
    }

    /// Takes the blank and comment lines at the cursor.
    fn trivia(&mut self) -> Trivia {
        let mut out = Vec::new();
        while let Some(l) = self.lines.get(self.pos).filter(|l| l.is_trivia()) {
            out.push((!l.is_blank()).then(|| comment_text(l.text())));
            self.pos += 1;
        }
        out
    }

    /// The node whose first content line is at `indent`.
    fn node_at(&mut self, indent: usize) -> Result<Node> {
        let Some(line) = self.peek_content().cloned() else {
            return Ok(Node::Null);
        };
        if is_dash(line.text()) {
            return self.seq(indent);
        }
        if split_key(line.text()).is_some() {
            return self.map(indent);
        }
        let lead = self.trivia();
        if lead.iter().any(Option::is_some) {
            return Err(unsupported(
                line.no,
                "a comment between a key and its value",
            ));
        }
        self.pos += 1;
        let (node, comment) = self.inline(line.text(), indent.saturating_sub(1), line.no)?;
        match comment {
            Some(_) => Err(unsupported(
                line.no,
                "a comment after a value on its own line",
            )),
            None => Ok(node),
        }
    }

    fn map(&mut self, indent: usize) -> Result<Node> {
        let mut entries = Vec::new();
        loop {
            let save = self.pos;
            let lead = self.trivia();
            let Some(line) = self.at(indent).filter(|l| !is_dash(l.text())) else {
                entries.extend(self.tail(save, indent));
                break;
            };
            let (key, rest) = split_key(line.text()).ok_or(unsupported(
                line.no,
                "a line that is neither `key: value` nor `- item`",
            ))?;
            self.pos += 1;
            let (value, comment) = self.after(&rest, indent, true, line.no)?;
            entries.push(Entry {
                lead,
                key,
                comment,
                value,
            });
        }
        Ok(Node::Map(entries))
    }

    fn seq(&mut self, indent: usize) -> Result<Node> {
        let mut items = Vec::new();
        loop {
            let save = self.pos;
            let lead = self.trivia();
            let Some(line) = self.at(indent).filter(|l| is_dash(l.text())) else {
                items.extend(self.tail(save, indent));
                break;
            };
            let (value, comment) = self.item(&line, indent)?;
            items.push(Entry {
                lead,
                key: String::new(),
                comment,
                value,
            });
        }
        Ok(Node::Seq(items))
    }

    /// The comments from `start` indented at least as deep as the collection at `indent`: they
    /// close it, and stay under its last entry. The cursor ends after them.
    fn tail(&mut self, start: usize, indent: usize) -> Option<Entry> {
        let inside = |l: &Src| l.is_trivia() && (l.is_blank() || l.indent >= indent);
        let taken = self.lines[start..].iter().take_while(|l| inside(l)).count();
        let end = start
            + self.lines[start..start + taken]
                .iter()
                .rposition(|l| !l.is_blank())
                .map_or(0, |i| i + 1);
        self.pos = start;
        if end == start {
            return None;
        }
        let lead = self.lines[start..end]
            .iter()
            .map(|l| (!l.is_blank()).then(|| comment_text(l.text())));
        self.pos = end;
        Some(Entry {
            lead: lead.collect(),
            key: String::new(),
            comment: None,
            value: Node::Tail,
        })
    }

    /// The content line at the cursor when it sits at `indent`.
    fn at(&self, indent: usize) -> Option<Src> {
        self.lines
            .get(self.pos)
            .filter(|l| l.indent == indent && !l.is_trivia())
            .cloned()
    }

    /// A `- item` line: a map or sequence starting on it continues on the lines below.
    fn item(&mut self, line: &Src, indent: usize) -> Result<(Node, Option<String>)> {
        let after_dash = &line.text()[1..];
        let rest = after_dash.trim_start_matches(' ');
        if is_dash(rest) || split_key(rest).is_some() {
            let column = indent + 1 + after_dash.len() - rest.len();
            self.lines[self.pos] = Src::new(line.no, &format!("{}{rest}", " ".repeat(column)));
            return Ok((self.node_at(column)?, None));
        }
        self.pos += 1;
        self.after(rest, indent, false, line.no)
    }

    /// The value after `key:` or `- `: on the line itself or on the lines below.
    fn after(
        &mut self,
        rest: &str,
        parent: usize,
        in_map: bool,
        no: usize,
    ) -> Result<(Node, Option<String>)> {
        let rest = rest.trim();
        if rest.is_empty() || rest.starts_with('#') {
            let comment = (!rest.is_empty()).then(|| comment_text(rest));
            return Ok((self.below(parent, in_map)?, comment));
        }
        self.inline(rest, parent, no)
    }

    /// The node on the lines below a key with nothing after its colon.
    fn below(&mut self, parent: usize, in_map: bool) -> Result<Node> {
        match self.peek_content().map(|l| (l.indent, is_dash(l.text()))) {
            Some((indent, _)) if indent > parent => self.node_at(indent),
            Some((indent, true)) if in_map && indent == parent => self.seq(parent),
            _ => Ok(Node::Null),
        }
    }

    /// A value that starts on the current line, `rest` its text.
    fn inline(&mut self, rest: &str, parent: usize, no: usize) -> Result<(Node, Option<String>)> {
        match rest.as_bytes().first() {
            Some(b'|' | b'>') => self.block(rest, parent, no),
            Some(b'[' | b'{') => self.flow(rest, parent, no),
            Some(b'&' | b'*' | b'!') => Err(unsupported(no, "an anchor, alias or tag")),
            Some(b'?') => Err(unsupported(no, "a complex key")),
            Some(b'"' | b'\'') => self.quoted(rest, parent, no),
            _ => self.plain(rest, parent, no),
        }
    }

    /// A literal or folded block scalar: its lines are kept as written, re-indented.
    fn block(&mut self, rest: &str, parent: usize, no: usize) -> Result<(Node, Option<String>)> {
        let (header, comment) = split_comment(rest);
        let explicit = header[1..]
            .chars()
            .find_map(|c| c.to_digit(10))
            .map(|d| d as usize);
        let keep = header.contains('+');
        let start = self.pos;
        let mut end = self.pos;
        let mut base = explicit.map(|n| parent + n);
        while let Some(l) = self.lines.get(self.pos) {
            if !l.is_blank() && l.indent <= parent {
                break;
            }
            if !l.is_blank() {
                base.get_or_insert(l.indent);
                end = self.pos + 1;
            }
            self.pos += 1;
        }
        if keep {
            end = self.pos;
        }
        self.pos = end;
        let base = base.unwrap_or(parent + 2);
        if self.lines[start..end]
            .iter()
            .any(|l| !l.is_blank() && l.indent < base)
        {
            return Err(unsupported(
                no,
                "a block scalar indented less than its first line",
            ));
        }
        let lines = self.lines[start..end]
            .iter()
            .map(|l| l.raw.get(base..).unwrap_or("").to_string());
        let node = Node::Block {
            header: header.to_string(),
            explicit,
            lines: lines.collect(),
        };
        Ok((node, comment))
    }

    /// A flow collection, maybe over several lines: re-read as a value and laid out anew.
    fn flow(&mut self, rest: &str, parent: usize, no: usize) -> Result<(Node, Option<String>)> {
        let mut text = rest.to_string();
        while depth(&text) > 0 {
            let Some(l) = self
                .lines
                .get(self.pos)
                .filter(|l| l.indent > parent || l.is_blank())
            else {
                return Err(unsupported(no, "an unclosed flow collection"));
            };
            if l.text().starts_with('#') {
                return Err(unsupported(l.no, "a comment inside a flow collection"));
            }
            text.push(' ');
            text.push_str(l.text().trim_end());
            self.pos += 1;
        }
        let (body, comment) = split_comment(&text);
        let value: Value =
            serde_norway::from_str(body).map_err(|e| FmtError::Parse(format!("line {no}: {e}")))?;
        Ok((value_node(&value, no)?, comment))
    }

    /// A quoted scalar, maybe over several lines.
    fn quoted(&mut self, rest: &str, parent: usize, no: usize) -> Result<(Node, Option<String>)> {
        let mut text = rest.to_string();
        while closing_quote(&text).is_none() {
            let Some(l) = self
                .lines
                .get(self.pos)
                .filter(|l| l.indent > parent || l.is_blank())
            else {
                return Err(unsupported(no, "an unclosed quoted string"));
            };
            text.push('\n');
            text.push_str(l.text());
            self.pos += 1;
        }
        let close = closing_quote(&text).unwrap_or(text.len());
        let (quoted, after) = text.split_at(close);
        let (tail, comment) = split_comment(after);
        if !tail.trim().is_empty() {
            return Err(unsupported(no, "text after a closing quote"));
        }
        let value: String = serde_norway::from_str(quoted)
            .map_err(|e| FmtError::Parse(format!("line {no}: {e}")))?;
        Ok((string_node(&value), comment))
    }

    /// A plain scalar; one that goes on over more lines is re-read and written on one.
    fn plain(&mut self, rest: &str, parent: usize, no: usize) -> Result<(Node, Option<String>)> {
        let (text, comment) = split_comment(rest);
        let mut more = Vec::new();
        while comment.is_none() {
            let save = self.pos;
            let gap = self.lines[self.pos..]
                .iter()
                .take_while(|l| l.is_blank())
                .count();
            match self.lines.get(self.pos + gap) {
                Some(l) if l.indent > parent && !l.text().starts_with('#') => {
                    more.extend(std::iter::repeat_n(String::new(), gap));
                    more.push(l.text().trim_end().to_string());
                    self.pos += gap + 1;
                }
                _ => {
                    self.pos = save;
                    break;
                }
            }
        }
        if more.is_empty() {
            let flow_safe = flow_safe(text) && reads_back(text);
            return Ok((
                Node::Scalar {
                    text: text.to_string(),
                    flow_safe,
                },
                comment,
            ));
        }
        let joined = format!("_: {text}\n  {}\n", more.join("\n  "));
        let value: Value = serde_norway::from_str(&joined)
            .map_err(|e| FmtError::Parse(format!("line {no}: {e}")))?;
        Ok((value_node(&value["_"], no)?, None))
    }
}

/// Moves a manifest's sections, and its metadata's fields, into the canonical order. The file's
/// first comments stay at the top.
fn reorder_manifest(node: Node) -> Node {
    let Node::Map(mut entries) = node else {
        return node;
    };
    let is_manifest = ["apiVersion", "kind"]
        .iter()
        .all(|k| entries.iter().any(|e| e.key == *k));
    if !is_manifest {
        return Node::Map(entries);
    }
    let header = entries
        .first_mut()
        .map(|e| std::mem::take(&mut e.lead))
        .unwrap_or_default();
    for e in entries.iter_mut().filter(|e| e.key == "metadata") {
        if let Node::Map(fields) = &mut e.value {
            sort_by_order(fields, METADATA_ORDER);
        }
    }
    sort_by_order(&mut entries, TOP_ORDER);
    if let Some(first) = entries.first_mut() {
        let mut lead = header;
        lead.append(&mut first.lead);
        first.lead = lead;
    }
    Node::Map(entries)
}

/// Stable: keys named in `order` first, in that order, the rest after them as they were.
fn sort_by_order(entries: &mut [Entry], order: &[&str]) {
    entries.sort_by_key(|e| {
        order
            .iter()
            .position(|k| *k == e.key)
            .unwrap_or(order.len())
    });
}

/// A value read from a flow collection or a multi-line scalar, as a node.
fn value_node(value: &Value, no: usize) -> Result<Node> {
    Ok(match value {
        Value::Null => Node::Scalar {
            text: "null".into(),
            flow_safe: true,
        },
        Value::Bool(b) => Node::Scalar {
            text: b.to_string(),
            flow_safe: true,
        },
        Value::Number(n) => Node::Scalar {
            text: n.to_string(),
            flow_safe: true,
        },
        Value::String(s) => string_node(s),
        Value::Sequence(items) => Node::Seq(
            items
                .iter()
                .map(|v| Ok(entry(String::new(), value_node(v, no)?)))
                .collect::<Result<_>>()?,
        ),
        Value::Mapping(map) => Node::Map(
            map.iter()
                .map(|(k, v)| Ok(entry(key_text(k, no)?, value_node(v, no)?)))
                .collect::<Result<_>>()?,
        ),
        Value::Tagged(_) => return Err(unsupported(no, "a tag")),
    })
}

fn entry(key: String, value: Node) -> Entry {
    Entry {
        lead: Vec::new(),
        key,
        comment: None,
        value,
    }
}

fn key_text(key: &Value, no: usize) -> Result<String> {
    match value_node(key, no)? {
        Node::Scalar { text, .. } => Ok(text),
        _ => Err(unsupported(no, "a key that is not a scalar")),
    }
}

/// A string as the shortest scalar text that reads back as the same string.
fn string_node(s: &str) -> Node {
    if plain_ok(s) {
        return Node::Scalar {
            text: s.to_string(),
            flow_safe: flow_safe(s),
        };
    }
    Node::Scalar {
        text: quote(s),
        flow_safe: true,
    }
}

/// `s` in single quotes, or in double quotes with escapes when it holds a quote or a control
/// character.
fn quote(s: &str) -> String {
    if s.contains('\'') || s.chars().any(char::is_control) {
        return serde_json::to_string(s).unwrap_or_else(|_| format!("{s:?}"));
    }
    format!("'{s}'")
}

/// Whether `s` may be written without quotes: it reads back as the same string, also for a
/// YAML 1.1 reader, and holds nothing a line or a flow collection would cut.
fn plain_ok(s: &str) -> bool {
    let first = s.chars().next().unwrap_or('0');
    let risky = first.is_ascii_digit() || "+-.".contains(first) || s.ends_with(':');
    let cut = s.contains(": ") || s.contains(" #") || s.contains(['\n', '\r', '\t']);
    let edge = s.trim() != s;
    let reserved = RESERVED.iter().any(|w| s.eq_ignore_ascii_case(w));
    if risky || cut || edge || reserved || s.chars().any(char::is_control) {
        return false;
    }
    serde_norway::from_str::<Value>(s).is_ok_and(|v| v == Value::String(s.to_string()))
}

/// Whether a plain scalar is the text its value would be written as, so that moving it into a
/// flow collection, which is re-read on the next run, changes nothing then.
fn reads_back(text: &str) -> bool {
    let Ok(value) = serde_norway::from_str::<Value>(text) else {
        return false;
    };
    matches!(value_node(&value, 0), Ok(Node::Scalar { text: t, .. }) if t == text)
}

/// Whether a plain scalar may stand inside a flow collection unchanged.
fn flow_safe(text: &str) -> bool {
    let cut = text.contains([',', '[', ']', '{', '}', ':', '#', '?']);
    !cut && !text.contains(" '") && !text.contains(" \"") && !text.starts_with(['-', '\'', '"'])
}

fn is_dash(text: &str) -> bool {
    text == "-" || text.starts_with("- ")
}

/// The text after a comment's `#`, trailing spaces dropped.
fn comment_text(text: &str) -> String {
    text.trim().trim_start_matches('#').trim_end().to_string()
}

/// Splits `text` at a comment outside quotes: (`value`, comment text).
fn split_comment(text: &str) -> (&str, Option<String>) {
    let mut i = 0;
    let mut prev = ' ';
    while let Some(c) = text[i..].chars().next() {
        if (c == '"' || c == '\'') && " \t[{,".contains(prev) {
            i += closing_quote(&text[i..]).unwrap_or(text.len() - i);
            prev = c;
            continue;
        }
        if c == '#' && (prev == ' ' || prev == '\t') {
            return (text[..i].trim_end(), Some(comment_text(&text[i..])));
        }
        prev = c;
        i += c.len_utf8();
    }
    (text.trim_end(), None)
}

/// The byte after the closing quote of a scalar starting with a quote, if it is in `text`.
fn closing_quote(text: &str) -> Option<usize> {
    let quote = text.chars().next()?;
    let mut chars = text.char_indices().skip(1).peekable();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' if quote == '"' => {
                chars.next();
            }
            '\'' if quote == '\'' && chars.peek().is_some_and(|(_, n)| *n == '\'') => {
                chars.next();
            }
            c if c == quote => return Some(i + 1),
            _ => {}
        }
    }
    None
}

/// How many flow brackets `text` leaves open, quoted scalars skipped.
fn depth(text: &str) -> i64 {
    let mut level = 0i64;
    let mut i = 0;
    let mut prev = ' ';
    while let Some(c) = text[i..].chars().next() {
        if (c == '"' || c == '\'') && " \t[{,:".contains(prev) {
            i += closing_quote(&text[i..]).unwrap_or(text.len() - i);
            prev = c;
            continue;
        }
        level += match c {
            '[' | '{' => 1,
            ']' | '}' => -1,
            _ => 0,
        };
        prev = c;
        i += c.len_utf8();
    }
    level
}

/// Splits a `key: rest` line; `None` for a line that is no map entry.
fn split_key(text: &str) -> Option<(String, String)> {
    if text.starts_with(['"', '\'']) {
        let close = closing_quote(text)?;
        let after = text[close..].trim_start();
        let rest = after.strip_prefix(':')?;
        return (rest.is_empty() || rest.starts_with([' ', '\t']))
            .then(|| (text[..close].to_string(), rest.to_string()));
    }
    if text.starts_with(['[', '{', '&', '*', '!', '|', '>', '#', '%', '@', '`']) || is_dash(text) {
        return None;
    }
    let (body, _) = split_comment(text);
    let colon = body.char_indices().find(|&(i, c)| {
        c == ':'
            && body[i + 1..]
                .chars()
                .next()
                .is_none_or(|n| n == ' ' || n == '\t')
    })?;
    let key = body[..colon.0].trim_end();
    (!key.is_empty()).then(|| (key.to_string(), text[colon.0 + 1..].to_string()))
}

/// The formatted text being written.
#[derive(Default)]
struct Out {
    text: String,
}

impl Out {
    fn document(&mut self, doc: Document) {
        let first = self.text.len();
        match doc.root {
            Some(Node::Map(entries)) => self.entries(&entries, 0, 0),
            Some(Node::Seq(items)) => self.items(&items, 0, 0),
            Some(node) => self.value(String::new(), &node, 0, None, 0),
            None => {}
        }
        self.trivia(&doc.tail, 0, self.text.len() == first);
        while self.text.ends_with("\n\n") {
            self.text.pop();
        }
    }

    fn line(&mut self, text: &str) {
        self.text.push_str(text.trim_end());
        self.text.push('\n');
    }

    /// Comments above an entry, a run of blank lines as one; none above a collection's first.
    fn trivia(&mut self, lead: &Trivia, indent: usize, first: bool) {
        let mut blank = first;
        for t in lead {
            match t {
                None if !blank => self.line(""),
                None => {}
                Some(c) => self.line(&format!("{}#{c}", " ".repeat(indent))),
            }
            blank = t.is_none();
        }
    }

    fn entries(&mut self, entries: &[Entry], indent: usize, depth: usize) {
        for (i, e) in entries.iter().enumerate() {
            self.trivia(&e.lead, indent, i == 0);
            if matches!(e.value, Node::Tail) {
                continue;
            }
            let key = format!("{}{}:", " ".repeat(indent), e.key);
            self.value(key, &e.value, indent, e.comment.as_deref(), depth + 1);
        }
    }

    fn items(&mut self, items: &[Entry], indent: usize, depth: usize) {
        for (i, item) in items.iter().enumerate() {
            let mut item = item.clone();
            let (mut lead, comment) = hoist(&mut item);
            let mut all = std::mem::take(&mut item.lead);
            all.append(&mut lead);
            self.trivia(&all, indent, i == 0);
            if matches!(item.value, Node::Tail) {
                continue;
            }
            if let Some(c) = comment {
                self.line(&format!("{}#{c}", " ".repeat(indent)));
            }
            self.item(&item.value, indent, item.comment.as_deref(), depth + 1);
        }
    }

    /// A sequence item: a block map or sequence starts on the dash line.
    fn item(&mut self, node: &Node, indent: usize, comment: Option<&str>, depth: usize) {
        let dash = format!("{}-", " ".repeat(indent));
        let nested = match node {
            Node::Map(entries) if !entries.is_empty() => Some(Inner::Map(entries)),
            Node::Seq(items) if !items.is_empty() && flow_line(&dash, node).is_none() => {
                Some(Inner::Seq(items))
            }
            _ => None,
        };
        let Some(inner) = nested else {
            return self.value(dash, node, indent, comment, depth);
        };
        let start = self.text.len();
        match inner {
            Inner::Map(entries) => self.entries(entries, indent + 2, depth),
            Inner::Seq(items) => self.items(items, indent + 2, depth),
        }
        self.text
            .replace_range(start..start + indent + 2, &format!("{dash} "));
    }

    /// `prefix` (a `key:` or a `-`) and the value after it.
    fn value(
        &mut self,
        prefix: String,
        node: &Node,
        indent: usize,
        comment: Option<&str>,
        depth: usize,
    ) {
        let note = comment.map(|c| format!(" #{c}")).unwrap_or_default();
        let sep = if prefix.is_empty() { "" } else { " " };
        if let Some(flow) = (depth > 1).then(|| flow_line(&prefix, node)).flatten() {
            return self.line(&format!("{flow}{note}"));
        }
        match node {
            Node::Null | Node::Tail => self.line(&format!("{prefix}{note}")),
            Node::Scalar { text, .. } => self.line(&format!("{prefix}{sep}{text}{note}")),
            Node::Block {
                header,
                explicit,
                lines,
            } => {
                self.line(&format!("{prefix}{sep}{header}{note}"));
                let pad = " ".repeat(indent + explicit.unwrap_or(2));
                for l in lines {
                    self.text.push_str(if l.is_empty() { "" } else { &pad });
                    self.text.push_str(l);
                    self.text.push('\n');
                }
            }
            Node::Map(entries) if entries.is_empty() => {
                self.line(&format!("{prefix}{sep}{{}}{note}"))
            }
            Node::Seq(items) if items.is_empty() => self.line(&format!("{prefix}{sep}[]{note}")),
            Node::Map(entries) => {
                self.line(&format!("{prefix}{note}"));
                self.entries(entries, indent + 2, depth);
            }
            Node::Seq(items) => {
                self.line(&format!("{prefix}{note}"));
                self.items(items, indent + 2, depth);
            }
        }
    }
}

enum Inner<'a> {
    Map(&'a [Entry]),
    Seq(&'a [Entry]),
}

/// The comments of an item's first nested entry, moved above the item's dash line.
fn hoist(item: &mut Entry) -> (Trivia, Option<String>) {
    let comment = match &item.value {
        Node::Map(e) | Node::Seq(e) if !e.is_empty() => item.comment.take(),
        _ => None,
    };
    let lead = match &mut item.value {
        Node::Map(e) | Node::Seq(e) => e.first_mut().map(|f| std::mem::take(&mut f.lead)),
        _ => None,
    };
    (lead.unwrap_or_default(), comment)
}

/// `prefix` and `node` as one flow line, when the node is a short collection of scalars.
fn flow_line(prefix: &str, node: &Node) -> Option<String> {
    let scalar = |e: &Entry| match &e.value {
        Node::Scalar { text, flow_safe }
            if *flow_safe && e.lead.is_empty() && e.comment.is_none() =>
        {
            Some(text.clone())
        }
        _ => None,
    };
    let sep = if prefix.is_empty() { "" } else { " " };
    let body = match node {
        Node::Map(entries)
            if (1..=FLOW_KEYS).contains(&entries.len()) && !prefix.ends_with('-') =>
        {
            let pairs: Option<Vec<String>> = entries
                .iter()
                .map(|e| {
                    (flow_safe(&e.key) || e.key.starts_with(['"', '\''])).then_some(())?;
                    scalar(e).map(|v| format!("{}: {v}", e.key))
                })
                .collect();
            format!("{{ {} }}", pairs?.join(", "))
        }
        Node::Seq(items) if !items.is_empty() => {
            let values: Option<Vec<String>> = items.iter().map(scalar).collect();
            format!("[{}]", values?.join(", "))
        }
        _ => return None,
    };
    let line = format!("{prefix}{sep}{body}");
    (line.chars().count() <= WIDTH).then_some(line)
}

#[cfg(test)]
mod tests;
