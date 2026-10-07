//! Path matching globs for `include` and `exclude` crawl patterns (T-3052).
//!
//! Patterns are path globs starting with `/`:
//! - `*` matches any run of characters except `/`.
//! - `**` matches any run of characters including `/`.
//! - Exclude rules always take precedence over include rules.
//! - An empty include list matches all paths.

/// Evaluates whether a `path` matches the glob `pattern`.
///
/// Dynamic programming over (pattern position, path position), so a hostile path cannot make
/// a pattern with several `*` or `**` backtrack exponentially: the work is at most
/// `pattern.len() × path.len()`, and both are bounded (patterns by MF-51, paths by the URL).
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let (p, s) = (pattern.as_bytes(), path.as_bytes());
    // `reach[j]`: the pattern consumed so far matches `s[..j]`.
    let mut reach = vec![false; s.len() + 1];
    reach[0] = true;
    let mut i = 0;
    while i < p.len() {
        let mut next = vec![false; s.len() + 1];
        if p[i] == b'*' {
            let crosses = p.get(i + 1) == Some(&b'*');
            // Once reachable, a star extends over following bytes; `*` stops at `/`.
            let mut open = false;
            for j in 0..=s.len() {
                if reach[j] {
                    open = true;
                }
                next[j] = open;
                if open && !crosses && s.get(j) == Some(&b'/') {
                    open = false;
                }
            }
            i += if crosses { 2 } else { 1 };
        } else {
            for j in 0..s.len() {
                next[j + 1] = reach[j] && s[j] == p[i];
            }
            i += 1;
        }
        reach = next;
    }
    reach[s.len()]
}

/// Checks whether a given path is included based on include and exclude patterns.
///
/// An excluded pattern immediately returns false. If include patterns are specified,
/// at least one must match; otherwise any non-excluded path is included.
pub fn is_included(path: &str, include: &[String], exclude: &[String]) -> bool {
    for ex in exclude {
        if glob_match(ex, path) {
            return false;
        }
    }
    if include.is_empty() {
        return true;
    }
    include.iter().any(|inc| glob_match(inc, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_patterns_match_expected_paths() {
        // /a/**
        assert!(glob_match("/a/**", "/a/b"));
        assert!(glob_match("/a/**", "/a/b/c"));
        assert!(glob_match("/a/**", "/a/"));
        assert!(!glob_match("/a/**", "/b/c"));
        assert!(!glob_match("/a/**", "/a"));

        // /a/*/c
        assert!(glob_match("/a/*/c", "/a/b/c"));
        assert!(glob_match("/a/*/c", "/a/x/c"));
        assert!(!glob_match("/a/*/c", "/a/b/d/c"));
        assert!(!glob_match("/a/*/c", "/a/c"));
    }

    #[test]
    fn exclude_beats_include() {
        let inc = vec!["/a/**".to_string()];
        let exc = vec!["/a/private/**".to_string()];
        assert!(is_included("/a/public/page", &inc, &exc));
        assert!(!is_included("/a/private/page", &inc, &exc));
        assert!(!is_included("/other", &inc, &exc));
    }

    #[test]
    fn empty_include_includes_everything_unless_excluded() {
        let inc = Vec::new();
        let exc = vec!["/admin/**".to_string()];
        assert!(is_included("/index.html", &inc, &exc));
        assert!(is_included("/about", &inc, &exc));
        assert!(!is_included("/admin/dashboard", &inc, &exc));
    }

    #[test]
    fn a_hostile_path_cannot_make_a_pattern_backtrack() {
        // Twelve `**` against a long path of one letter: exponential for a backtracking matcher,
        // a few hundred thousand steps here.
        let pattern = format!("/{}b", "**a".repeat(12));
        let path = format!("/{}", "a".repeat(4_000));
        let started = std::time::Instant::now();
        assert!(!glob_match(&pattern, &path));
        assert!(glob_match(&pattern, &format!("{path}b")));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }
}
