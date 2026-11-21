//! Read-only checks of context frontmatter (plan.md section 5.4).

use crate::advisory::{ImpactTarget, inspect_impact};
use crate::canon::CanonPath;
use crate::doc;
use crate::error::Error;
use crate::root::{ContextStatus, validate_context_dir};
use crate::scan::{self, EntryKind, MAX_SOURCE_BYTES, ReadOutcome};
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::Path;

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Finding {
    pub path: String,
    pub code: String,
    pub detail: String,
}

impl Finding {
    fn new(path: &str, code: impl Into<String>, detail: String) -> Self {
        Self {
            path: path.to_owned(),
            code: code.into(),
            detail,
        }
    }
}

/// Read eligible Markdown directly. If inspection is incomplete, report an
/// I/O error instead of treating an unreadable target as a missing document.
pub fn check(root: &Path) -> Result<Vec<Finding>, Error> {
    if matches!(validate_context_dir(root)?, ContextStatus::NotInitialized) {
        return Err(Error::NotInitialized);
    }
    let mut findings = Vec::new();
    let mut supersedes = BTreeMap::new();
    for entry in scan::scan_context(root)? {
        if !matches!(entry.kind, EntryKind::Candidate { .. }) {
            continue;
        }
        let path = entry.rel_path;
        let bytes = match scan::stable_read(&root.join(&path), MAX_SOURCE_BYTES)
            .map_err(|e| io::Error::new(e.kind(), format!("could not read {path}: {e}")))?
        {
            ReadOutcome::Content { bytes, .. } => bytes,
            ReadOutcome::Oversize => continue,
            _ => return Err(io::Error::other(format!("could not read {path} stably")).into()),
        };
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };

        let metadata = doc::frontmatter(text, &path);
        findings.extend(
            metadata
                .warnings
                .into_iter()
                .map(|w| Finding::new(&path, w.code, w.message)),
        );
        for impact in metadata.impacts {
            if inspect_impact(root, &impact) == ImpactTarget::Missing {
                findings.push(Finding::new(
                    &path,
                    "STALE_IMPACT",
                    format!("target {:?} does not exist", impact.as_str()),
                ));
            }
        }
        supersedes.insert(path, metadata.supersedes);
    }

    check_supersedes(&supersedes, &mut findings);
    findings.sort();
    findings.dedup();
    Ok(findings)
}

fn check_supersedes(documents: &BTreeMap<String, Vec<CanonPath>>, findings: &mut Vec<Finding>) {
    let paths: Vec<_> = documents.keys().map(String::as_str).collect();
    let indices: HashMap<_, _> = paths.iter().copied().zip(0..paths.len()).collect();
    let mut edges = vec![Vec::new(); paths.len()];
    for (node, (path, targets)) in documents.iter().enumerate() {
        for target in targets {
            if let Some(&to) = indices.get(target.as_str()) {
                edges[node].push(to);
            } else {
                findings.push(Finding::new(
                    path,
                    "UNRESOLVED_SUPERSEDES",
                    format!(
                        "target {:?} is not an eligible context document",
                        target.as_str()
                    ),
                ));
            }
        }
    }
    for node in cycle_members(&edges) {
        findings.push(Finding::new(
            paths[node],
            "SUPERSEDES_CYCLE",
            "document is part of a supersession cycle".into(),
        ));
    }
}

/// Find strongly connected groups with two iterative DFS passes. A group
/// with multiple documents is a cycle. The parser already rejects self-links.
fn cycle_members(edges: &[Vec<usize>]) -> Vec<usize> {
    let mut reverse = vec![Vec::new(); edges.len()];
    for (node, targets) in edges.iter().enumerate() {
        for &to in targets {
            reverse[to].push(node);
        }
    }

    let mut visited = vec![false; edges.len()];
    let mut finished = Vec::with_capacity(edges.len());
    let mut stack = Vec::new();
    for start in 0..edges.len() {
        if std::mem::replace(&mut visited[start], true) {
            continue;
        }
        stack.push((start, edges[start].iter()));
        while let Some((node, targets)) = stack.last_mut() {
            match targets.next() {
                Some(&to) if !std::mem::replace(&mut visited[to], true) => {
                    stack.push((to, edges[to].iter()));
                }
                Some(_) => {}
                None => {
                    finished.push(*node);
                    stack.pop();
                }
            }
        }
    }

    visited.fill(false);
    let mut cyclic = Vec::new();
    let mut group = Vec::new();
    for start in finished.into_iter().rev() {
        if std::mem::replace(&mut visited[start], true) {
            continue;
        }
        group.clear();
        group.push(start);
        let mut next = 0;
        while let Some(&node) = group.get(next) {
            next += 1;
            for &to in &reverse[node] {
                if !std::mem::replace(&mut visited[to], true) {
                    group.push(to);
                }
            }
        }
        if group.len() > 1 {
            cyclic.extend_from_slice(&group);
        }
    }
    cyclic
}
