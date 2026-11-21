//! Canonical project-root-relative paths for `impacts` and `path_filter`
//! (plan.md section 3.3) and the lexical matching rules they use (section 8.2).

use crate::error::Error;

pub const MAX_CANONICAL_PATH_BYTES: usize = 1_024;
pub const MAX_RAW_TOOL_PATH_BYTES: usize = 4_096;
pub const ROOT: &str = "./";

/// Where a path string came from. The Windows backslash adapter and the 4 KiB
/// raw-length limit apply only to tool arguments; stored frontmatter is checked
/// by the same canonical rules on every host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathSource {
    ToolArg,
    Stored,
}

/// A canonical path: either `ROOT` (`"./"`) or `"/"`-joined components with no
/// empty or `.` segments. Always valid UTF-8 and at most
/// `MAX_CANONICAL_PATH_BYTES` bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonPath(String);

impl CanonPath {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0 == ROOT
    }

    /// Number of path components; the root has depth zero.
    pub fn depth(&self) -> u32 {
        if self.is_root() {
            0
        } else {
            self.0.matches('/').count() as u32 + 1
        }
    }

    /// Components in order; empty for the root.
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/').filter(|c| !c.is_empty() && *c != ".")
    }
}

impl AsRef<str> for CanonPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CanonPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Canonicalize a path string. `windows_host` is `cfg!(windows)` in production
/// and a parameter so the backslash adapter can be tested on any host.
pub fn canonicalize_with_host(
    raw: &str,
    source: PathSource,
    windows_host: bool,
) -> Result<CanonPath, Error> {
    if source == PathSource::ToolArg && raw.len() > MAX_RAW_TOOL_PATH_BYTES {
        return Err(Error::InvalidArgument(format!(
            "path exceeds {} bytes",
            MAX_RAW_TOOL_PATH_BYTES
        )));
    }

    let converted;
    let input: &str = if raw.contains('\\') {
        match (source, windows_host) {
            (PathSource::ToolArg, true) => {
                converted = raw.replace('\\', "/");
                converted.as_str()
            }
            (PathSource::ToolArg, false) => {
                return Err(Error::InvalidArgument(
                    "backslashes are not valid path separators; use '/'".into(),
                ));
            }
            (PathSource::Stored, _) => {
                return Err(Error::InvalidArgument(
                    "backslashes are not valid in stored paths; use '/'".into(),
                ));
            }
        }
    } else {
        raw
    };

    if input.trim().is_empty() {
        return Err(Error::InvalidArgument("path must not be empty".into()));
    }
    if input.chars().any(|c| c.is_control()) {
        return Err(Error::InvalidArgument(
            "path must not contain NUL or control characters".into(),
        ));
    }
    if input.starts_with('/') {
        return Err(Error::InvalidArgument(
            "absolute paths are not allowed; use a root-relative path".into(),
        ));
    }
    let bytes = input.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Err(Error::InvalidArgument(
            "drive-qualified paths are not allowed".into(),
        ));
    }

    let mut components: Vec<&str> = Vec::new();
    for comp in input.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                return Err(Error::InvalidArgument(
                    "parent traversal components are not allowed".into(),
                ));
            }
            _ => components.push(comp),
        }
    }

    let canonical = if components.is_empty() {
        ROOT.to_string()
    } else {
        components.join("/")
    };
    if canonical.len() > MAX_CANONICAL_PATH_BYTES {
        return Err(Error::InvalidArgument(format!(
            "canonical path exceeds {} bytes",
            MAX_CANONICAL_PATH_BYTES
        )));
    }
    Ok(CanonPath(canonical))
}

pub fn canonicalize(raw: &str, source: PathSource) -> Result<CanonPath, Error> {
    canonicalize_with_host(raw, source, cfg!(windows))
}

/// Lexical eligibility match between a recorded impact and a path filter
/// (section 8.2). Equal paths, or either being a component-boundary ancestor
/// of the other, match; the root `./` matches every canonical path in either
/// position because it denotes the whole project subtree.
pub fn paths_match(impact: &CanonPath, filter: &CanonPath) -> bool {
    impact_rank(impact, filter).is_some()
}

/// The match relationship used for path-only ordering (section 8.3). `Ancestor`
/// means the impact covers the filter (`src/` vs `src/auth/x.rs`); `Descendant`
/// means it sits beneath it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImpactRank {
    Exact,
    Ancestor { distance: u32 },
    Descendant { distance: u32 },
}

pub fn impact_rank(impact: &CanonPath, filter: &CanonPath) -> Option<ImpactRank> {
    let (i, f) = (impact.as_str(), filter.as_str());
    if i == f {
        return Some(ImpactRank::Exact);
    }
    if impact.is_root() {
        return Some(ImpactRank::Ancestor {
            distance: filter.depth(),
        });
    }
    if filter.is_root() {
        return Some(ImpactRank::Descendant {
            distance: impact.depth(),
        });
    }
    let (ib, fb) = (format!("{i}/"), format!("{f}/"));
    if f.starts_with(&ib) {
        Some(ImpactRank::Ancestor {
            distance: filter.depth() - impact.depth(),
        })
    } else if i.starts_with(&fb) {
        Some(ImpactRank::Descendant {
            distance: impact.depth() - filter.depth(),
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use PathSource::{Stored, ToolArg};

    fn ok(raw: &str) -> CanonPath {
        canonicalize_with_host(raw, ToolArg, false).unwrap()
    }

    #[test]
    fn canonicalizes_to_component_form() {
        for (raw, want) in [
            ("src/auth/login.go", "src/auth/login.go"),
            ("src/auth/", "src/auth"),
            ("./src/x", "src/x"),
            ("src//x", "src/x"),
            ("src/./x", "src/x"),
            ("a..b/c", "a..b/c"),
            ("x ", "x "),
        ] {
            assert_eq!(ok(raw).as_str(), want, "{raw}");
        }
    }

    #[test]
    fn root_forms() {
        for raw in [".", "./", "./.", "./."] {
            assert_eq!(ok(raw).as_str(), ROOT, "{raw}");
            assert!(ok(raw).is_root());
            assert_eq!(ok(raw).depth(), 0);
        }
    }

    #[test]
    fn rejects_invalid_paths() {
        for raw in [
            "",
            "  ",
            "..",
            "src/../x",
            "src/../../y",
            "/abs",
            "//host/share",
            "C:",
            "C:/win",
            "c:relative",
            "a\0b",
            "line\nbreak",
            "tab\there",
        ] {
            assert!(
                canonicalize_with_host(raw, ToolArg, false).is_err(),
                "{raw:?} should be rejected"
            );
        }
    }

    #[test]
    fn windows_backslash_adapter() {
        let p = canonicalize_with_host("src\\auth\\x.rs", ToolArg, true).unwrap();
        assert_eq!(p.as_str(), "src/auth/x.rs");
        for raw in ["src\\..\\outside", "C:\\win", "\\\\srv\\x", "\\rooted"] {
            assert!(
                canonicalize_with_host(raw, ToolArg, true).is_err(),
                "{raw:?} should be rejected"
            );
        }
        // Non-Windows tool input and stored paths reject backslashes everywhere.
        assert!(canonicalize_with_host("a\\b", ToolArg, false).is_err());
        assert!(canonicalize_with_host("a\\b", Stored, true).is_err());
        assert!(canonicalize_with_host("a\\b", Stored, false).is_err());
    }

    #[test]
    fn length_limits() {
        let long = "a/".repeat(600) + "x";
        assert!(canonicalize_with_host(&long, ToolArg, false).is_err()); // >1 KiB canonical
        let raw_over_4k = "b".repeat(4_097);
        assert!(canonicalize_with_host(&raw_over_4k, ToolArg, false).is_err());
        assert!(canonicalize_with_host(&raw_over_4k, Stored, false).is_err()); // still >1 KiB canonical
        let within = format!("{}.md", "a".repeat(1_018));
        assert_eq!(
            canonicalize_with_host(&within, Stored, false)
                .unwrap()
                .as_str()
                .len(),
            1021
        );
    }

    #[test]
    fn spec_match_table() {
        let cases = [
            ("src/auth/login.go", "src/auth/login.go", true),
            ("src/auth", "src/auth/login.go", true),
            ("src/auth/login.go", "src/auth", true),
            ("src/auth", "src/auth/providers/", true),
            ("src/auth", "src/author.go", false),
            ("src/auth/login.go", "src/auth/login.go.bak", false),
            ("src/authx", "src/auth", false),
            ("src", "srcx", false),
        ];
        for (impact, filter, want) in cases {
            assert_eq!(
                paths_match(&ok(impact), &ok(filter)),
                want,
                "{impact} vs {filter}"
            );
        }
        // Root matches in either position.
        assert!(paths_match(&ok("./"), &ok("src/x")));
        assert!(paths_match(&ok("src/x"), &ok("./")));
        assert!(paths_match(&ok("./"), &ok("./")));
    }

    #[test]
    fn rank_distance_and_direction() {
        assert_eq!(
            impact_rank(&ok("src"), &ok("src/auth/x.rs")),
            Some(ImpactRank::Ancestor { distance: 2 })
        );
        assert_eq!(
            impact_rank(&ok("src/auth/x.rs"), &ok("src")),
            Some(ImpactRank::Descendant { distance: 2 })
        );
        assert_eq!(
            impact_rank(&ok("./"), &ok("a/b")),
            Some(ImpactRank::Ancestor { distance: 2 })
        );
        assert_eq!(ok("src").depth(), 1);
        assert_eq!(ok("a/b/c").depth(), 3);
    }
}
