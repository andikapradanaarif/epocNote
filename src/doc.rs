//! Markdown document contract: frontmatter parsing and chunking
//! (plan.md sections 4, 7, and 10.4).
//!
//! The YAML frontmatter parser is saphyr-parser 0.3.4, driven at the event
//! level so aliases, merge keys, duplicate keys, non-mapping roots, and
//! nesting beyond 32 levels are rejected as malformed instead of expanded.
//! Chunking uses pulldown-cmark 0.13.6 for heading detection with source
//! positions.

use crate::canon::{self, CanonPath, PathSource};
use crate::error::Warning;
use pulldown_cmark::{Event as MdEvent, Options, Parser as MdParser, Tag, TagEnd};
use saphyr_parser::{Event as YamlEvent, Parser as YamlParser, ScalarStyle};
use std::collections::HashSet;
use std::ops::Range;

pub const MAX_TITLE_SCALARS: usize = 200;
pub const MAX_HEADING_SCALARS: usize = 300;
pub const MAX_CHUNK_SCALARS: usize = 1_500;
pub const MAX_IMPACTS: usize = 32;
pub const MAX_SUPERSEDES: usize = 8;
const MAX_YAML_DEPTH: u32 = 32;

type Yaml<'a> = YamlParser<'a, saphyr_parser::StrInput<'a>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocKind {
    Note,
    Adr,
    Architecture,
}

impl DocKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Note => "note",
            Self::Adr => "adr",
            Self::Architecture => "architecture",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Paths,
    Global,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Paths => "paths",
            Self::Global => "global",
        }
    }
}

#[derive(Debug)]
pub struct Chunk {
    pub heading: String,
    /// 1-based inclusive lines in the source file.
    pub start_line: u32,
    pub end_line: u32,
    pub text: String,
}

#[derive(Debug)]
pub struct Analyzed {
    pub title: String,
    pub kind: DocKind,
    pub scope: Scope,
    /// Unix seconds, or 0 for the undated fallback (section 4.1).
    pub created_at: i64,
    /// Canonical impact paths, deduplicated, at most 32.
    pub impacts: Vec<CanonPath>,
    /// Canonical context-document paths, deduplicated, at most 8.
    pub supersedes: Vec<CanonPath>,
    pub chunks: Vec<Chunk>,
    pub warnings: Vec<Warning>,
    pub malformed_frontmatter: bool,
}

/// Analyze one document: frontmatter contract plus body chunking.
/// `doc_path` is the canonical context-relative path (`".context/..."`),
/// used for self-supersession checks and the filename-stem title fallback.
pub fn analyze(text: &str, doc_path: &str) -> Analyzed {
    let mut warnings = Vec::new();
    let stripped = text.strip_prefix('\u{feff}').unwrap_or(text);

    let mut malformed = false;
    let mut title_field: Option<String> = None;
    let mut kind = DocKind::Note;
    let mut scope = Scope::Paths;
    let mut created_at = 0i64;
    let mut impacts = Vec::new();
    let mut supersedes = Vec::new();
    let mut body_start = 0usize;

    match locate_frontmatter(stripped) {
        FmScan::None => {}
        FmScan::Malformed => {
            malformed = true;
            warnings.push(Warning::new(
                "MALFORMED_FRONTMATTER",
                Some(doc_path.to_string()),
                "frontmatter opening has no closing delimiter; indexing raw text",
            ));
        }
        FmScan::Found { yaml, body } => match parse_frontmatter(&stripped[yaml]) {
            Err(reason) => {
                malformed = true;
                warnings.push(Warning::new(
                    "MALFORMED_FRONTMATTER",
                    Some(doc_path.to_string()),
                    format!("malformed frontmatter ({reason}); indexing raw text"),
                ));
            }
            Ok(fields) => {
                body_start = body.end;
                title_field = extract_title(&fields, doc_path, &mut warnings);
                kind = extract_kind(&fields, doc_path, &mut warnings);
                scope = extract_scope(&fields, doc_path, &mut warnings);
                created_at = extract_created_at(&fields, doc_path, &mut warnings);
                impacts = extract_path_list(
                    &fields,
                    "impacts",
                    MAX_IMPACTS,
                    doc_path,
                    &mut warnings,
                    |c| Ok(c.clone()),
                );
                supersedes = extract_path_list(
                    &fields,
                    "supersedes",
                    MAX_SUPERSEDES,
                    doc_path,
                    &mut warnings,
                    |c| {
                        if !c.as_str().starts_with(".context/") {
                            return Err("entry is outside .context/");
                        }
                        if c.as_str() == doc_path {
                            return Err("document supersedes itself");
                        }
                        Ok(c.clone())
                    },
                );
            }
        },
    }

    // When malformed, the body is the entire raw text (section 4.2). All
    // offsets are measured in `stripped` so positions stay consistent
    // whether or not a BOM was present.
    let (body, body_base) = if malformed {
        (stripped, 0usize)
    } else {
        (&stripped[body_start..], body_start)
    };
    let index = line_index(stripped);
    let mut chunks = Vec::new();
    let mut first_heading: Option<String> = None;
    for (s, e, heading) in sections(body) {
        if first_heading.is_none() && !heading.trim().is_empty() {
            first_heading = Some(heading.clone());
        }
        emit_chunks(
            stripped,
            &index,
            (body_base + s)..(body_base + e),
            &heading,
            doc_path,
            &mut warnings,
            &mut chunks,
        );
    }

    let title = match title_field {
        Some(t) => t,
        None => fallback_title(first_heading.as_deref(), doc_path, &mut warnings),
    };

    Analyzed {
        title,
        kind,
        scope,
        created_at,
        impacts,
        supersedes,
        chunks,
        warnings,
        malformed_frontmatter: malformed,
    }
}

// ---------- frontmatter location ----------

enum FmScan {
    /// First line is not `---`: no frontmatter at all.
    None,
    /// First line is `---` but no later line closes it.
    Malformed,
    /// Byte ranges of the YAML block and of the closing `---` line
    /// (the body starts at `body.end`).
    Found {
        yaml: Range<usize>,
        body: Range<usize>,
    },
}

/// The first line must be exactly `---`; a later line that is exactly `---`
/// closes it (section 4.2). `...` never closes the block.
fn locate_frontmatter(text: &str) -> FmScan {
    let Some((first, yaml_start)) = take_line(text, 0) else {
        return FmScan::None;
    };
    if first != "---" {
        return FmScan::None;
    }
    let mut pos = yaml_start;
    while let Some((line, after)) = take_line(text, pos) {
        if line == "---" {
            return FmScan::Found {
                yaml: yaml_start..pos,
                body: pos..after,
            };
        }
        pos = after;
    }
    FmScan::Malformed
}

/// One logical line: `\n`, `\r\n`, or a lone `\r` is a line break. The
/// returned content excludes the line ending; the second return value is
/// the offset of the next line.
fn take_line(text: &str, pos: usize) -> Option<(&str, usize)> {
    if pos >= text.len() {
        return None;
    }
    let bytes = &text.as_bytes()[pos..];
    for i in 0..bytes.len() {
        match bytes[i] {
            b'\n' => {
                let mut content_end = pos + i;
                if i > 0 && bytes[i - 1] == b'\r' {
                    content_end -= 1;
                }
                return Some((&text[pos..content_end], pos + i + 1));
            }
            b'\r' if i + 1 >= bytes.len() || bytes[i + 1] != b'\n' => {
                return Some((&text[pos..pos + i], pos + i + 1));
            }
            _ => {}
        }
    }
    Some((&text[pos..], text.len()))
}

// ---------- YAML frontmatter ----------

/// A minimal YAML node model carrying just what the contract validates.
enum YNode {
    Scalar {
        value: String,
        plain: bool,
        non_str: bool,
    },
    Seq(Vec<YNode>),
    Map(Vec<(String, YNode)>),
}

fn next_yaml<'a>(p: &mut Yaml<'a>) -> Result<YamlEvent<'a>, String> {
    match p.next_event() {
        None => Err("unexpected end of YAML".into()),
        Some(Err(e)) => Err(format!("YAML scan error: {e}")),
        Some(Ok((ev, _))) => Ok(ev),
    }
}

fn peek_yaml<'a>(p: &mut Yaml<'a>) -> Result<Option<YamlEvent<'a>>, String> {
    match p.peek() {
        None => Ok(None),
        Some(Err(e)) => Err(format!("YAML scan error: {e}")),
        Some(Ok((ev, _))) => Ok(Some(ev.clone())),
    }
}

/// Parse the frontmatter block. Structural problems — invalid YAML,
/// duplicate keys, aliases, merge keys, a non-mapping root, more than one
/// document, or nesting beyond 32 levels — make the whole frontmatter
/// malformed (section 4.2).
fn parse_frontmatter(yaml: &str) -> Result<Vec<(String, YNode)>, String> {
    let mut p = YamlParser::new_from_str(yaml);
    let mut saw_document = false;
    loop {
        let Some(event) = peek_yaml(&mut p)? else {
            return Err("empty YAML stream".into());
        };
        match event {
            YamlEvent::StreamStart | YamlEvent::DocumentEnd => {
                p.next_event();
            }
            YamlEvent::DocumentStart(_) => {
                if saw_document {
                    return Err("more than one YAML document".into());
                }
                saw_document = true;
                p.next_event();
            }
            YamlEvent::MappingStart(..) => {
                p.next_event();
                let YNode::Map(fields) = parse_map(&mut p, 1)? else {
                    unreachable!()
                };
                // A complete mapping does not end the frontmatter: consume
                // through the stream end and reject any trailing content.
                loop {
                    match peek_yaml(&mut p)? {
                        Some(YamlEvent::DocumentEnd) => {
                            p.next_event();
                        }
                        Some(YamlEvent::StreamEnd) => return Ok(fields),
                        Some(_) => {
                            return Err("unexpected content after the frontmatter mapping".into());
                        }
                        None => return Err("unclosed YAML stream".into()),
                    }
                }
            }
            YamlEvent::Alias(..) => return Err("YAML aliases are not allowed".into()),
            YamlEvent::SequenceStart(..) | YamlEvent::Scalar(..) => {
                return Err("frontmatter is not a mapping".into());
            }
            YamlEvent::StreamEnd => return Err("frontmatter has no mapping".into()),
            _ => return Err("unexpected YAML event".into()),
        }
    }
}

fn parse_node<'a>(p: &mut Yaml<'a>, depth: u32) -> Result<YNode, String> {
    if depth > MAX_YAML_DEPTH {
        return Err("YAML nesting exceeds 32 levels".into());
    }
    match next_yaml(p)? {
        YamlEvent::Scalar(value, style, _anchor, tag) => {
            let non_str = match tag {
                Some(t) => !(t.is_yaml_core_schema() && t.suffix == "str"),
                None => false,
            };
            Ok(YNode::Scalar {
                value: value.into_owned(),
                plain: matches!(style, ScalarStyle::Plain),
                non_str,
            })
        }
        YamlEvent::SequenceStart(..) => {
            let mut items = Vec::new();
            loop {
                match peek_yaml(p)? {
                    Some(YamlEvent::SequenceEnd) => {
                        p.next_event();
                        return Ok(YNode::Seq(items));
                    }
                    Some(_) => items.push(parse_node(p, depth + 1)?),
                    None => return Err("unclosed YAML sequence".into()),
                }
            }
        }
        YamlEvent::MappingStart(..) => parse_map(p, depth + 1),
        YamlEvent::Alias(..) => Err("YAML aliases are not allowed".into()),
        other => Err(format!("unexpected YAML event {other:?}")),
    }
}

/// Consume a mapping's key/value pairs (the MappingStart is already read).
fn parse_map<'a>(p: &mut Yaml<'a>, depth: u32) -> Result<YNode, String> {
    if depth > MAX_YAML_DEPTH {
        return Err("YAML nesting exceeds 32 levels".into());
    }
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    loop {
        match peek_yaml(p)? {
            Some(YamlEvent::MappingEnd) => {
                p.next_event();
                return Ok(YNode::Map(entries));
            }
            Some(_) => {}
            None => return Err("unclosed YAML mapping".into()),
        }
        // Keys must be scalars; complex keys are outside the contract.
        let key = match next_yaml(p)? {
            YamlEvent::Scalar(value, _style, _anchor, tag) => {
                if let Some(t) = &tag
                    && !(t.is_yaml_core_schema() && t.suffix == "str")
                {
                    return Err("non-string mapping key".into());
                }
                if value.as_ref() == "<<" {
                    return Err("YAML merge keys are not allowed".into());
                }
                value.into_owned()
            }
            YamlEvent::Alias(..) => return Err("YAML aliases are not allowed".into()),
            _ => return Err("non-scalar mapping key".into()),
        };
        if !seen.insert(key.clone()) {
            return Err(format!("duplicate key {key:?}"));
        }
        let value = parse_node(p, depth + 1)?;
        entries.push((key, value));
    }
}

/// A scalar usable as a contract string: quoted or plain literals that
/// resolve to `str` under the YAML 1.2 core schema. Plain nulls, booleans,
/// and numbers are invalid values, not strings (section 4.2).
fn scalar_string(node: &YNode) -> Option<&str> {
    let YNode::Scalar {
        value,
        plain,
        non_str,
    } = node
    else {
        return None;
    };
    if *non_str {
        return None;
    }
    if !plain {
        return Some(value);
    }
    match core_schema_kind(value) {
        CoreKind::Str => Some(value),
        _ => None,
    }
}

#[derive(PartialEq)]
enum CoreKind {
    Null,
    Bool,
    Num,
    Str,
}

/// Minimal YAML 1.2 core-schema resolution for plain scalars.
fn core_schema_kind(v: &str) -> CoreKind {
    if v.is_empty() || matches!(v, "~" | "null" | "Null" | "NULL") {
        return CoreKind::Null;
    }
    if matches!(v, "true" | "True" | "TRUE" | "false" | "False" | "FALSE") {
        return CoreKind::Bool;
    }
    if is_core_number(v) {
        return CoreKind::Num;
    }
    CoreKind::Str
}

fn is_core_number(v: &str) -> bool {
    let s = v.strip_prefix(['-', '+']).unwrap_or(v);
    if s.is_empty() {
        return false;
    }
    if let Some(hex) = s.strip_prefix("0x") {
        return !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit());
    }
    if let Some(oct) = s.strip_prefix("0o") {
        return !oct.is_empty() && oct.chars().all(|c| c.is_digit(8));
    }
    if matches!(s, ".inf" | ".Inf" | ".INF" | ".nan" | ".NaN" | ".NAN") {
        return true;
    }
    let mut seen_digit = false;
    let mut seen_dot = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            seen_digit = true;
        } else if c == '.' && !seen_dot {
            seen_dot = true;
        } else if (c == 'e' || c == 'E') && seen_digit {
            let mut tail = String::new();
            if matches!(chars.peek(), Some('-') | Some('+')) {
                chars.next();
            }
            tail.extend(chars.by_ref());
            return !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit());
        } else {
            return false;
        }
    }
    seen_digit
}

// ---------- field extraction ----------

fn field<'a>(fields: &'a [(String, YNode)], name: &str) -> Option<&'a YNode> {
    fields.iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

fn warn(warnings: &mut Vec<Warning>, code: &str, doc_path: &str, message: impl Into<String>) {
    warnings.push(Warning::new(code, Some(doc_path.to_string()), message));
}

fn extract_title(
    fields: &[(String, YNode)],
    doc_path: &str,
    warnings: &mut Vec<Warning>,
) -> Option<String> {
    let node = field(fields, "title")?;
    match scalar_string(node) {
        Some(s) => {
            let (t, truncated) = truncate_chars(s, MAX_TITLE_SCALARS);
            if truncated {
                warn(
                    warnings,
                    "TRUNCATED_METADATA",
                    doc_path,
                    "title exceeds 200 scalars; truncated",
                );
            }
            Some(t)
        }
        None => {
            warn(
                warnings,
                "INVALID_FIELD",
                doc_path,
                "title is not a string; using fallback",
            );
            None
        }
    }
}

fn extract_kind(
    fields: &[(String, YNode)],
    doc_path: &str,
    warnings: &mut Vec<Warning>,
) -> DocKind {
    let Some(node) = field(fields, "kind") else {
        return DocKind::Note;
    };
    match scalar_string(node) {
        Some("adr") => DocKind::Adr,
        Some("architecture") => DocKind::Architecture,
        Some("note") => DocKind::Note,
        Some(_) | None => {
            warn(
                warnings,
                "INVALID_FIELD",
                doc_path,
                "invalid kind; using 'note'",
            );
            DocKind::Note
        }
    }
}

fn extract_scope(fields: &[(String, YNode)], doc_path: &str, warnings: &mut Vec<Warning>) -> Scope {
    let Some(node) = field(fields, "scope") else {
        return Scope::Paths;
    };
    match scalar_string(node) {
        Some("global") => Scope::Global,
        Some("paths") => Scope::Paths,
        Some(_) | None => {
            warn(
                warnings,
                "INVALID_FIELD",
                doc_path,
                "invalid scope; using 'paths'",
            );
            Scope::Paths
        }
    }
}

fn extract_created_at(
    fields: &[(String, YNode)],
    doc_path: &str,
    warnings: &mut Vec<Warning>,
) -> i64 {
    let Some(node) = field(fields, "created_at") else {
        return 0;
    };
    let Some(s) = scalar_string(node) else {
        warn(
            warnings,
            "INVALID_FIELD",
            doc_path,
            "created_at is not a string; stored undated",
        );
        return 0;
    };
    match chrono::DateTime::parse_from_rfc3339(s) {
        Ok(dt) if dt.timestamp() > 0 => dt.timestamp(),
        _ => {
            warn(
                warnings,
                "INVALID_FIELD",
                doc_path,
                "unparseable created_at; stored undated",
            );
            0
        }
    }
}

/// Extract a list field of canonical paths. A non-sequence value makes the
/// field invalid; each non-string or non-canonical entry is warned and
/// omitted (section 4.1).
fn extract_path_list(
    fields: &[(String, YNode)],
    name: &str,
    cap: usize,
    doc_path: &str,
    warnings: &mut Vec<Warning>,
    extra_check: impl Fn(&CanonPath) -> Result<CanonPath, &'static str>,
) -> Vec<CanonPath> {
    let Some(node) = field(fields, name) else {
        return Vec::new();
    };
    let YNode::Seq(items) = node else {
        warn(
            warnings,
            "INVALID_FIELD",
            doc_path,
            format!("{name} is not an array; treated as empty"),
        );
        return Vec::new();
    };
    let mut out: Vec<CanonPath> = Vec::new();
    for item in items {
        let Some(s) = scalar_string(item) else {
            warn(
                warnings,
                "INVALID_FIELD",
                doc_path,
                format!("{name} entry is not a string; omitted"),
            );
            continue;
        };
        match canon::canonicalize(s, PathSource::Stored) {
            Ok(c) => match extra_check(&c) {
                Ok(c) => {
                    if out.iter().any(|e| e == &c) {
                        continue;
                    }
                    if out.len() >= cap {
                        warn(
                            warnings,
                            "EXCESS_ENTRIES",
                            doc_path,
                            format!("{name} exceeds {cap} entries; extras ignored"),
                        );
                        break;
                    }
                    out.push(c);
                }
                Err(msg) => warn(
                    warnings,
                    "INVALID_FIELD",
                    doc_path,
                    format!("{name} entry {s:?} omitted: {msg}"),
                ),
            },
            Err(e) => warn(
                warnings,
                "INVALID_FIELD",
                doc_path,
                format!("{name} entry {s:?} omitted: {e}"),
            ),
        }
    }
    out
}

// ---------- title fallback ----------

fn fallback_title(
    first_heading: Option<&str>,
    doc_path: &str,
    warnings: &mut Vec<Warning>,
) -> String {
    let stem = || {
        let name = doc_path.rsplit('/').next().unwrap_or(doc_path);
        name.strip_suffix(".md").unwrap_or(name).to_string()
    };
    let raw = match first_heading {
        Some(h) if !h.trim().is_empty() => h.trim().to_string(),
        _ => stem(),
    };
    let (t, truncated) = truncate_chars(&raw, MAX_TITLE_SCALARS);
    if truncated {
        warn(
            warnings,
            "TRUNCATED_METADATA",
            doc_path,
            "fallback title exceeds 200 scalars; truncated",
        );
    }
    t
}

fn truncate_chars(s: &str, n: usize) -> (String, bool) {
    if s.chars().count() <= n {
        return (s.to_string(), false);
    }
    (s.chars().take(n).collect(), true)
}

// ---------- chunking ----------

/// Byte offsets where each line starts; `\r\n` counts as one break.
fn line_index(text: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    let b = text.as_bytes();
    for i in 0..b.len() {
        match b[i] {
            b'\n' => starts.push(i + 1),
            b'\r' if i + 1 >= b.len() || b[i + 1] != b'\n' => starts.push(i + 1),
            _ => {}
        }
    }
    starts
}

fn line_of(index: &[usize], offset: usize) -> u32 {
    index.partition_point(|s| *s <= offset) as u32
}

/// Section spans (body-relative byte ranges) with their heading text.
/// ATX and Setext headings come from CommonMark parsing; heading-like text
/// inside fenced or indented code never produces a section.
fn sections(body: &str) -> Vec<(usize, usize, String)> {
    let mut heads: Vec<(usize, String)> = Vec::new();
    let mut in_heading = false;
    let mut head_start = 0usize;
    let mut head_text = String::new();
    for (event, range) in MdParser::new_ext(body, Options::empty()).into_offset_iter() {
        match event {
            MdEvent::Start(Tag::Heading { .. }) => {
                in_heading = true;
                head_start = range.start;
                head_text.clear();
            }
            MdEvent::End(TagEnd::Heading(_)) => {
                in_heading = false;
                heads.push((head_start, std::mem::take(&mut head_text)));
            }
            MdEvent::Text(t) | MdEvent::Code(t) | MdEvent::InlineMath(t) if in_heading => {
                head_text.push_str(&t)
            }
            _ => {}
        }
    }

    if heads.is_empty() {
        return if body.is_empty() {
            Vec::new()
        } else {
            vec![(0, body.len(), String::new())]
        };
    }
    let mut secs = Vec::new();
    if heads[0].0 > 0 {
        secs.push((0, heads[0].0, String::new()));
    }
    for (i, (start, text)) in heads.iter().enumerate() {
        let end = heads.get(i + 1).map(|h| h.0).unwrap_or(body.len());
        secs.push((*start, end, text.clone()));
    }
    secs
}

/// Split a section span into ≤1500-scalar chunks (section 7.2): paragraph
/// boundaries first, then line boundaries, then scalar boundaries inside a
/// single long line. Chunks are whitespace-trimmed contiguous substrings;
/// whitespace-only chunks are skipped.
fn emit_chunks(
    doc: &str,
    index: &[usize],
    span: Range<usize>,
    heading: &str,
    doc_path: &str,
    warnings: &mut Vec<Warning>,
    out: &mut Vec<Chunk>,
) {
    let (heading, truncated) = truncate_chars(heading, MAX_HEADING_SCALARS);
    if truncated {
        warn(
            warnings,
            "TRUNCATED_METADATA",
            doc_path,
            "section heading exceeds 300 scalars; truncated",
        );
    }
    if char_count(&doc[span.clone()]) <= MAX_CHUNK_SCALARS {
        push_chunk(doc, index, span, &heading, out);
        return;
    }
    for unit in pack_spans(doc, paragraph_spans(doc, span)) {
        if char_count(&doc[unit.clone()]) <= MAX_CHUNK_SCALARS {
            push_chunk(doc, index, unit, &heading, out);
            continue;
        }
        for line in pack_spans(doc, line_spans(doc, unit)) {
            if char_count(&doc[line.clone()]) <= MAX_CHUNK_SCALARS {
                push_chunk(doc, index, line, &heading, out);
            } else {
                for piece in char_pieces(&doc[line.clone()], line.start) {
                    push_chunk(doc, index, piece, &heading, out);
                }
            }
        }
    }
}

/// Greedily merge adjacent spans while the merged range stays within the
/// scalar cap. A span that is itself oversized stands alone for the next
/// splitting stage.
fn pack_spans(doc: &str, spans: Vec<Range<usize>>) -> Vec<Range<usize>> {
    let mut packed: Vec<Range<usize>> = Vec::new();
    let mut open: Option<Range<usize>> = None;
    for span in spans {
        let oversized = char_count(&doc[span.clone()]) > MAX_CHUNK_SCALARS;
        if oversized {
            if let Some(o) = open.take() {
                packed.push(o);
            }
            packed.push(span);
            continue;
        }
        match &mut open {
            None => open = Some(span),
            Some(o) => {
                let candidate = o.start..span.end;
                if char_count(&doc[candidate.clone()]) <= MAX_CHUNK_SCALARS {
                    *o = candidate;
                } else {
                    packed.push(o.clone());
                    open = Some(span);
                }
            }
        }
    }
    if let Some(o) = open {
        packed.push(o);
    }
    packed
}

/// Paragraph spans: maximal runs of non-blank lines. Blank runs separate
/// paragraphs and are never emitted as chunk content.
fn paragraph_spans(doc: &str, span: Range<usize>) -> Vec<Range<usize>> {
    let mut paras = Vec::new();
    let mut cur: Option<Range<usize>> = None;
    for line in line_spans(doc, span) {
        if doc[line.clone()].trim().is_empty() {
            if let Some(c) = cur.take() {
                paras.push(c);
            }
        } else {
            match &mut cur {
                None => cur = Some(line),
                Some(c) => c.end = line.end,
            }
        }
    }
    if let Some(c) = cur {
        paras.push(c);
    }
    paras
}

fn line_spans(doc: &str, span: Range<usize>) -> Vec<Range<usize>> {
    let mut lines = Vec::new();
    let mut pos = span.start;
    while pos < span.end {
        let after = take_line(&doc[..span.end], pos)
            .map(|(_, a)| a.min(span.end))
            .unwrap_or(span.end);
        lines.push(pos..after);
        pos = after;
    }
    lines
}

/// Split a single long line at Unicode scalar boundaries.
fn char_pieces(text: &str, base_offset: usize) -> Vec<Range<usize>> {
    let mut pieces = Vec::new();
    let mut start = 0usize;
    let mut count = 0usize;
    for (i, _) in text.char_indices().skip(1) {
        count += 1;
        if count == MAX_CHUNK_SCALARS {
            pieces.push(base_offset + start..base_offset + i);
            start = i;
            count = 0;
        }
    }
    if start < text.len() {
        pieces.push(base_offset + start..base_offset + text.len());
    }
    pieces
}

fn push_chunk(doc: &str, index: &[usize], span: Range<usize>, heading: &str, out: &mut Vec<Chunk>) {
    let raw = &doc[span.clone()];
    // Trim boundary whitespace; positions then describe the stored text.
    let lead = raw.len() - raw.trim_start().len();
    let trail = raw.len() - raw.trim_end().len();
    if lead + trail >= raw.len() {
        return; // whitespace-only chunk
    }
    let start = span.start + lead;
    let end = span.end - trail;
    out.push(Chunk {
        heading: heading.to_string(),
        start_line: line_of(index, start),
        end_line: line_of(index, end - 1),
        text: doc[start..end].to_string(),
    });
}

fn char_count(s: &str) -> usize {
    s.chars().count()
}
