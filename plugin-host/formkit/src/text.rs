//! Small string helpers shared by the layers.

/// Keep the tail: paths read right to left ("…/work/proj-a").
pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let tail: String = s
            .chars()
            .rev()
            .take(max.saturating_sub(1))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("…{tail}")
    }
}

/// Keep the head: names read left to right.
pub fn clip_end(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

/// Last non-empty path component ("" for "/" or "").
pub fn basename(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string()
}

/// The directory a path field completes in: everything up to the last
/// `/`. A value with no `/` completes in the home directory.
pub fn scan_base(value: &str) -> String {
    let v = value.trim();
    match v.rfind('/') {
        Some(0) => "/".to_string(),
        Some(p) => v[..p].to_string(),
        None => "~".to_string(),
    }
}

/// Sibling scheme: /path/to/repo -> /path/to/repo-worktrees/<name>.
pub fn dest_base(repo: &str) -> String {
    let repo = repo.trim_end_matches('/');
    if repo.is_empty() {
        return String::new();
    }
    format!("{repo}-worktrees/")
}

/// Single-quote for sh and for tmux command strings.
pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// `~` and `~/x` against a home directory. An empty or missing home
/// leaves the path alone.
pub fn expand_home(path: &str, home: Option<&str>) -> String {
    let Some(home) = home.filter(|h| !h.is_empty()) else {
        return path.to_string();
    };
    if path == "~" {
        return home.to_string();
    }
    match path.strip_prefix("~/") {
        Some(rest) => format!("{home}/{rest}"),
        None => path.to_string(),
    }
}

/// Rank a candidate against the typed fragment: 0 is the best match, and
/// `None` rejects the row. Prefix beats substring beats subsequence, so
/// the row you are spelling out stays at the top.
pub fn rank(hay: &str, needle: &str) -> Option<u8> {
    if needle.is_empty() {
        return Some(4);
    }
    if hay.starts_with(needle) {
        return Some(0);
    }
    let h = hay.to_lowercase();
    let n = needle.to_lowercase();
    if h.starts_with(&n) {
        return Some(1);
    }
    if h.contains(&n) {
        return Some(2);
    }
    let mut chars = h.chars();
    if n.chars().all(|c| chars.any(|x| x == c)) {
        return Some(3);
    }
    None
}

/// Short age for a commit time, e.g. `4m`, `2h`, `9d`.
pub fn age(now: i64, then: i64) -> String {
    let d = (now - then).max(0);
    if d < 90 {
        format!("{d}s")
    } else if d < 5400 {
        format!("{}m", d / 60)
    } else if d < 172800 {
        format!("{}h", d / 3600)
    } else if d < 63072000 {
        format!("{}d", d / 86400)
    } else {
        format!("{}y", d / 31536000)
    }
}

/// The last non-empty line of a job's output, for an error message.
pub fn last_line(output: &str, fallback: &str) -> String {
    output
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or(fallback)
        .to_string()
}
