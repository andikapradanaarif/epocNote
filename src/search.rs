//! `epocnote_search`: path-only and text retrieval, ranking, response
//! budget, and coverage fields (plan.md section 8).

use crate::canon::{self, CanonPath, ImpactRank, PathSource};
use crate::db::{Index, map_error};
use crate::error::{Error, Warning, fit_warnings};
use crate::sync::{IndexSync, ScanReuse};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use unicode_normalization::char::is_combining_mark;

pub const MAX_RESULTS: usize = 10;
const DEFAULT_LIMIT: usize = 5;
const MAX_QUERY_CHARS: usize = 512;
const MAX_TOKENS: usize = 32;
/// Compact tool-result cap (section 8.5): structured content plus its text
/// copy plus a small envelope reserve.
const RESULT_BUDGET: usize = 48 * 1024;
const ENVELOPE_RESERVE: usize = 512;
const MAX_SUPERSEDED_BY: usize = 8;
const CATCH_UP: Duration = Duration::from_secs(2);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchInput {
    pub query: Option<String>,
    pub path_filter: Option<String>,
    #[serde(default, deserialize_with = "de_limit")]
    pub limit: Limit,
}

/// Field presence is significant: `limit` absent means default, while an
/// explicit null or non-integer value is a validation error (section 8.1).
#[derive(Debug, Default)]
pub enum Limit {
    #[default]
    Absent,
    Invalid,
    Value(i64),
}

fn de_limit<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Limit, D::Error> {
    use serde::Deserialize;
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::Number(n) => n.as_i64().map(Limit::Value).unwrap_or(Limit::Invalid),
        _ => Limit::Invalid,
    })
}

#[derive(Debug, Serialize)]
pub struct SearchResult {
    pub path: String,
    pub title: String,
    pub kind: String,
    pub scope: String,
    pub created_at: Option<String>,
    pub content_hash: String,
    pub heading: String,
    pub start_line: i64,
    pub end_line: i64,
    pub body: String,
    pub score: Option<f64>,
    #[serde(rename = "match")]
    pub match_tier: Option<&'static str>,
    pub superseded_by: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct SearchOutput {
    pub results: Vec<SearchResult>,
    pub mode: &'static str,
    pub truncated: bool,
    pub partial_index: bool,
    pub excluded_count: usize,
    pub warnings: Vec<Warning>,
    pub warnings_truncated: bool,
}

/// Synchronize the index, then run the section 8.3/8.4 retrieval.
pub fn search(
    index: &Index,
    sync: &IndexSync,
    input: SearchInput,
    reuse: ScanReuse,
) -> Result<SearchOutput, Error> {
    let deadline = Instant::now() + CATCH_UP;
    let parsed = validate(input)?;
    let coverage = sync.prepare_until(index, reuse, deadline)?;
    let conn = index.conn();
    let superseded = superseded_set(conn)?;
    let ranked = match &parsed.tokens {
        None => path_retrieval(conn, &parsed, &superseded)?,
        Some(tokens) => text_retrieval(conn, &parsed, tokens, &superseded)?,
    };
    let mut output = build_output(conn, ranked, parsed.limit, &coverage)?;
    pack_results(&mut output);
    Ok(output)
}

struct Parsed {
    filter: Option<CanonPath>,
    tokens: Option<Vec<String>>,
    limit: usize,
}

fn validate(input: SearchInput) -> Result<Parsed, Error> {
    if input.query.is_none() && input.path_filter.is_none() {
        return Err(Error::InvalidArgument(
            "query or path_filter is required".into(),
        ));
    }
    let tokens = match &input.query {
        None => None,
        Some(q) => {
            if q.chars().count() > MAX_QUERY_CHARS {
                return Err(Error::InvalidArgument(format!(
                    "query exceeds {MAX_QUERY_CHARS} characters"
                )));
            }
            let tokens = extract_tokens(q);
            if tokens.is_empty() {
                return Err(Error::InvalidArgument(
                    "query has no searchable tokens".into(),
                ));
            }
            if tokens.len() > MAX_TOKENS {
                return Err(Error::InvalidArgument(format!(
                    "query has more than {MAX_TOKENS} tokens"
                )));
            }
            Some(tokens)
        }
    };
    let filter = match &input.path_filter {
        None => None,
        Some(f) => Some(canon::canonicalize(f, PathSource::ToolArg)?),
    };
    let limit = match input.limit {
        Limit::Absent => DEFAULT_LIMIT,
        Limit::Value(v) if (1..=i32::MAX as i64).contains(&v) => (v as usize).min(MAX_RESULTS),
        _ => {
            return Err(Error::InvalidArgument(
                "limit must be an integer from 1 through 2147483647".into(),
            ));
        }
    };
    Ok(Parsed {
        filter,
        tokens,
        limit,
    })
}

/// Literal tokens: contiguous Unicode letters, numbers, combining marks, or
/// underscores; runs without a letter or number are discarded (section 8.4).
fn extract_tokens(q: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut has_alnum = false;
    let flush = |cur: &mut String, has_alnum: &mut bool, out: &mut Vec<String>| {
        if *has_alnum {
            out.push(cur.to_lowercase());
        }
        cur.clear();
        *has_alnum = false;
    };
    for c in q.chars() {
        if c.is_alphanumeric() || c == '_' || is_combining_mark(c) {
            cur.push(c);
            has_alnum |= c.is_alphanumeric();
        } else {
            flush(&mut cur, &mut has_alnum, &mut out);
        }
    }
    flush(&mut cur, &mut has_alnum, &mut out);
    out
}

fn fts_expr(tokens: &[String], joiner: &str) -> String {
    tokens
        .iter()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(joiner)
}

// ---------- eligibility ----------

/// `(context_path, target)` rows whose target matches the filter
/// lexically, fetched with `idx_impacts_target` ranges (section 8.2).
fn matching_impacts(
    conn: &Connection,
    filter: &CanonPath,
) -> Result<HashMap<String, Vec<CanonPath>>, Error> {
    let mut out: HashMap<String, Vec<CanonPath>> = HashMap::new();
    let f = filter.as_str();
    let (sql, binds) = if filter.is_root() {
        // The root matches every canonical path; load all impacts for
        // best-impact ranking.
        (
            String::from("SELECT context_path, target FROM impacts"),
            Vec::new(),
        )
    } else {
        let mut sql = String::from(
            "SELECT context_path, target FROM impacts WHERE target = ?1
             OR (target >= ?2 AND target < ?3)",
        );
        let mut binds: Vec<String> = vec![f.to_string(), format!("{f}/"), format!("{f}0")];
        // Ancestor impacts: equality on each proper component prefix,
        // including the canonical root `./`, which covers every filter.
        let mut prefix = String::new();
        for (i, comp) in f.split('/').enumerate() {
            if i > 0 {
                prefix.push('/');
            }
            prefix.push_str(comp);
            if prefix.len() < f.len() {
                binds.push(prefix.clone());
                sql.push_str(&format!(" OR target = ?{}", binds.len()));
            }
        }
        binds.push(canon::ROOT.to_string());
        sql.push_str(&format!(" OR target = ?{}", binds.len()));
        (sql, binds)
    };
    let mut stmt = conn.prepare(&sql).map_err(map_error)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(binds.iter()), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(map_error)?;
    for row in rows {
        let (doc, target) = row.map_err(map_error)?;
        if let Ok(c) = canon::canonicalize(&target, PathSource::Stored)
            && canon::paths_match(&c, filter)
        {
            out.entry(doc).or_default().push(c);
        }
    }
    Ok(out)
}

/// Rank key for a doc's best matching impact (section 8.3): exact, then
/// ancestor/descendant by component distance (ancestor preferred at equal
/// distance); global docs take the last class.
fn best_rank(impacts: &[CanonPath], filter: &CanonPath) -> (u8, u32, u8) {
    impacts
        .iter()
        .filter_map(|i| canon::impact_rank(i, filter))
        .map(|r| match r {
            ImpactRank::Exact => (0u8, 0u32, 0u8),
            ImpactRank::Ancestor { distance } => (1, distance, 0),
            ImpactRank::Descendant { distance } => (1, distance, 1),
        })
        .min()
        .unwrap_or((1, u32::MAX, 1))
}

fn superseded_set(conn: &Connection) -> Result<HashSet<String>, Error> {
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT s.target FROM supersedes s
             JOIN files f ON f.path = s.context_path",
        )
        .map_err(map_error)?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(map_error)?;
    let mut out = HashSet::new();
    for r in rows {
        out.insert(r.map_err(map_error)?);
    }
    Ok(out)
}

fn superseded_by(conn: &Connection, path: &str) -> Result<Vec<String>, Error> {
    let mut stmt = conn
        .prepare(
            "SELECT s.context_path FROM supersedes s
             JOIN files f ON f.path = s.context_path
             WHERE s.target = ?1 ORDER BY s.context_path LIMIT ?2",
        )
        .map_err(map_error)?;
    let rows = stmt
        .query_map(rusqlite::params![path, MAX_SUPERSEDED_BY as i64], |r| {
            r.get::<_, String>(0)
        })
        .map_err(map_error)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(map_error)?);
    }
    Ok(out)
}

// ---------- retrieval ----------

struct DocRow {
    path: String,
    title: String,
    kind: String,
    scope: String,
    created_at: i64,
    hash: String,
    heading: String,
    start_line: i64,
    end_line: i64,
    body: String,
    score: Option<f64>,
}

fn doc_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<DocRow> {
    Ok(DocRow {
        path: r.get(0)?,
        title: r.get(1)?,
        kind: r.get(2)?,
        scope: r.get(3)?,
        created_at: r.get(4)?,
        hash: r.get(5)?,
        heading: r.get(6)?,
        start_line: r.get(7)?,
        end_line: r.get(8)?,
        body: r.get(9)?,
        score: r.get(10).ok(),
    })
}

const DOC_COLS: &str = "f.path, f.title, f.kind, f.scope, f.created_at, f.hash,
     c.heading, c.start_line, c.end_line, c.body";

/// Best-impact rank: class (exact 0, related 1, global 2), component
/// distance, direction (ancestor 0, descendant 1).
type RankKey = (u8, u32, u8);

struct RankedDoc {
    doc: DocRow,
    tier: Option<&'static str>,
    rank: RankKey,
}

/// Ranked candidate docs, best chunk per doc, before limit and budget.
struct Ranked {
    docs: Vec<RankedDoc>,
    /// Eligible documents that matched, for `truncated`.
    eligible: usize,
    mode: &'static str,
}

fn eligible(doc: &DocRow, impacts: Option<&HashMap<String, Vec<CanonPath>>>) -> bool {
    match impacts {
        None => true,
        Some(m) => doc.scope == "global" || m.contains_key(&doc.path),
    }
}

fn path_retrieval(
    conn: &Connection,
    parsed: &Parsed,
    superseded: &HashSet<String>,
) -> Result<Ranked, Error> {
    let impacts = match &parsed.filter {
        Some(f) => Some(matching_impacts(conn, f)?),
        None => None,
    };
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {DOC_COLS}, NULL FROM files f
             JOIN chunks c ON c.id = (
                 SELECT c2.id FROM chunks c2 WHERE c2.file_path = f.path
                 ORDER BY c2.start_line, c2.id LIMIT 1)"
        ))
        .map_err(map_error)?;
    let rows = stmt
        .query_map([], doc_from)
        .map_err(map_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_error)?;

    let mut docs: Vec<RankedDoc> = Vec::new();
    for d in rows {
        let rank = match &parsed.filter {
            // No filter: all docs eligible, no impact ranking.
            None => (0, 0, 0),
            Some(f) => {
                if d.scope == "global" {
                    (2, 0, 0)
                } else if let Some(list) = impacts.as_ref().and_then(|m| m.get(&d.path)) {
                    best_rank(list, f)
                } else {
                    continue;
                }
            }
        };
        docs.push(RankedDoc {
            doc: d,
            tier: None,
            rank,
        });
    }
    let eligible = docs.len();
    docs.sort_by(|a, b| {
        let (da, ra) = (&a.doc, a.rank);
        let (db, rb) = (&b.doc, b.rank);
        let sa = superseded.contains(&da.path);
        let sb = superseded.contains(&db.path);
        (sa, ra, std::cmp::Reverse(da.created_at), &da.path).cmp(&(
            sb,
            rb,
            std::cmp::Reverse(db.created_at),
            &db.path,
        ))
    });
    Ok(Ranked {
        docs,
        eligible,
        mode: "path",
    })
}

fn text_retrieval(
    conn: &Connection,
    parsed: &Parsed,
    tokens: &[String],
    superseded: &HashSet<String>,
) -> Result<Ranked, Error> {
    let impacts = match &parsed.filter {
        Some(f) => Some(matching_impacts(conn, f)?),
        None => None,
    };
    let mut tiers: Vec<RankedDoc> = Vec::new();
    for (joiner, tier) in [(" AND ", "all"), (" OR ", "any")] {
        if tier == "any" && tokens.len() == 1 {
            break;
        }
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {DOC_COLS}, bm25(chunks_fts) FROM chunks_fts
                 JOIN chunks c ON c.id = chunks_fts.rowid
                 JOIN files f ON f.path = c.file_path
                 WHERE chunks_fts MATCH ?1
                 ORDER BY bm25(chunks_fts), c.start_line, c.id"
            ))
            .map_err(map_error)?;
        let rows = stmt
            .query_map(rusqlite::params![fts_expr(tokens, joiner)], doc_from)
            .map_err(map_error)?;
        let mut seen: HashSet<String> = tiers.iter().map(|r| r.doc.path.clone()).collect();
        for r in rows {
            let d = r.map_err(map_error)?;
            if !eligible(&d, impacts.as_ref()) || seen.contains(&d.path) {
                continue;
            }
            seen.insert(d.path.clone());
            tiers.push(RankedDoc {
                doc: d,
                tier: Some(tier),
                rank: (0, 0, 0),
            });
        }
    }
    let eligible = tiers.len();
    tiers.sort_by(|a, b| {
        let (da, ta) = (&a.doc, a.tier);
        let (db, tb) = (&b.doc, b.tier);
        (superseded.contains(&da.path), ta == Some("any"))
            .cmp(&(superseded.contains(&db.path), tb == Some("any")))
            .then(
                da.score
                    .unwrap_or(f64::INFINITY)
                    .partial_cmp(&db.score.unwrap_or(f64::INFINITY))
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(db.created_at.cmp(&da.created_at))
            .then(da.path.cmp(&db.path))
    });
    Ok(Ranked {
        docs: tiers,
        eligible,
        mode: "text",
    })
}

// ---------- response assembly (section 8.5) ----------

fn rfc3339(secs: i64) -> Option<String> {
    if secs <= 0 {
        return None;
    }
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

fn build_output(
    conn: &Connection,
    ranked: Ranked,
    limit: usize,
    coverage: &crate::sync::SyncOutcome,
) -> Result<SearchOutput, Error> {
    let mut results = Vec::new();
    for r in ranked.docs.into_iter().take(limit) {
        let (d, tier) = (r.doc, r.tier);
        results.push(SearchResult {
            superseded_by: superseded_by(conn, &d.path)?,
            path: d.path,
            title: d.title,
            kind: d.kind,
            scope: d.scope,
            created_at: rfc3339(d.created_at),
            content_hash: d.hash,
            heading: d.heading,
            start_line: d.start_line,
            end_line: d.end_line,
            body: d.body,
            score: d.score,
            match_tier: tier,
        });
    }
    let mut all_warnings = coverage.warnings.clone();
    if coverage.partial_index && coverage.remaining > 0 {
        all_warnings.insert(
            0,
            Warning::new(
                "INDEX_CATCHING_UP",
                None,
                format!(
                    "index is behind the latest scan; {} document(s) remain to be reconciled",
                    coverage.remaining
                ),
            ),
        );
    }
    let (warnings, warnings_truncated) = fit_warnings(all_warnings);
    Ok(SearchOutput {
        truncated: ranked.eligible > results.len(),
        results,
        mode: ranked.mode,
        partial_index: coverage.partial_index,
        excluded_count: coverage.excluded_count,
        warnings,
        warnings_truncated,
    })
}

/// Serialized size of the tool-result object: structured content plus its
/// JSON-escaped text copy plus a small envelope reserve (section 8.5).
fn serialized_size(out: &SearchOutput) -> usize {
    let json = serde_json::to_string(out).unwrap_or_default();
    let text_copy = serde_json::to_string(&json).unwrap_or_default();
    json.len() + text_copy.len() + ENVELOPE_RESERVE
}

/// Drop the lowest-ranked whole entries until both representations fit
/// inside the 48 KiB budget; never split a chunk body.
fn pack_results(out: &mut SearchOutput) {
    while !out.results.is_empty() && serialized_size(out) > RESULT_BUDGET {
        out.results.pop();
        out.truncated = true;
    }
}
