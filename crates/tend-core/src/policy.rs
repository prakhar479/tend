//! Repo policy: `.tend.toml`, committed with the project (I6 E7).
//!
//! ```toml
//! test_paths = ["e2e/**", "checks/*.py"]          # also test files, for the guard
//! protected = ["migrations/**", "tests/golden/**"] # edits are guarded like test weakening
//! audit_ignore = ["vendor/**"]                     # left out of `tend audit`
//! ```
//!
//! Globs are relative to the repo root: `*` stays within a path component, `**` crosses
//! them, and a pattern without a `/` matches the file name anywhere. Pure: the caller
//! reads the file and says where the root is.

use regex::Regex;

#[derive(Debug, Clone, Default)]
pub struct Policy {
    /// Absolute paths are made relative to this before matching.
    pub root: String,
    pub test_paths: Vec<Glob>,
    pub protected: Vec<Glob>,
    pub audit_ignore: Vec<Glob>,
}

#[derive(Debug, Clone)]
pub struct Glob {
    pub pattern: String,
    re: Regex,
}

impl Glob {
    pub fn new(pattern: &str) -> Option<Glob> {
        let p = pattern.trim().trim_start_matches("./");
        // `dir/` means the directory: same as `dir`.
        let anchored = p.trim_end_matches('/').contains('/');
        let p = p.trim_end_matches('/');
        if p.is_empty() {
            return None;
        }
        let mut re = String::from(if anchored { "^" } else { "^(?:.*/)?" });
        let mut chars = p.trim_start_matches('/').chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '*' if chars.peek() == Some(&'*') => {
                    chars.next();
                    if chars.peek() == Some(&'/') {
                        chars.next();
                        re.push_str("(?:.*/)?");
                    } else {
                        re.push_str(".*");
                    }
                }
                '*' => re.push_str("[^/]*"),
                '?' => re.push_str("[^/]"),
                c => re.push_str(&regex::escape(&c.to_string())),
            }
        }
        // A directory pattern covers what is under it.
        re.push_str("(?:/.*)?$");
        Some(Glob {
            pattern: pattern.to_string(),
            re: Regex::new(&re).ok()?,
        })
    }

    pub fn matches(&self, rel: &str) -> bool {
        self.re.is_match(rel)
    }
}

impl Policy {
    fn rel<'a>(&self, path: &'a str) -> &'a str {
        let root = self.root.trim_end_matches('/');
        if !root.is_empty() {
            if let Some(rest) = path.strip_prefix(root) {
                return rest.trim_start_matches('/');
            }
        }
        path.trim_start_matches("./")
    }

    fn first<'g>(&self, globs: &'g [Glob], path: &str) -> Option<&'g Glob> {
        let rel = self.rel(path);
        globs.iter().find(|g| g.matches(rel))
    }

    pub fn is_test(&self, path: &str) -> bool {
        self.first(&self.test_paths, path).is_some()
    }

    /// The protecting pattern, if any.
    pub fn protected_by(&self, path: &str) -> Option<&str> {
        self.first(&self.protected, path)
            .map(|g| g.pattern.as_str())
    }

    pub fn audit_ignores(&self, path: &str) -> bool {
        self.first(&self.audit_ignore, path).is_some()
    }

    pub fn is_empty(&self) -> bool {
        self.test_paths.is_empty() && self.protected.is_empty() && self.audit_ignore.is_empty()
    }
}

/// Parse `.tend.toml`: `key = ["glob", …]` lines and `#` comments. Returns the policy
/// and one warning per line it could not use.
pub fn parse(text: &str, root: &str) -> (Policy, Vec<String>) {
    let mut p = Policy {
        root: root.to_string(),
        ..Default::default()
    };
    let mut warnings = Vec::new();
    // An array may span lines: join until the brackets close.
    let mut pending = String::new();
    let mut start = 0;
    for (n, raw) in text.lines().enumerate() {
        let line = strip_comment(raw).trim().to_string();
        if line.is_empty() && pending.is_empty() {
            continue;
        }
        if pending.is_empty() {
            start = n + 1;
        }
        pending.push_str(&line);
        pending.push(' ');
        if pending.contains('[') && !pending.contains(']') {
            continue;
        }
        let stmt = std::mem::take(&mut pending);
        let Some((key, value)) = stmt.split_once('=') else {
            warnings.push(format!("line {start}: expected `key = [\"glob\", …]`"));
            continue;
        };
        let key = key.trim();
        let globs: Vec<Glob> = match string_array(value.trim()) {
            Some(items) => items.iter().filter_map(|s| Glob::new(s)).collect(),
            None => {
                warnings.push(format!("line {start}: {key}: expected a list of strings"));
                continue;
            }
        };
        match key {
            "test_paths" => p.test_paths.extend(globs),
            "protected" => p.protected.extend(globs),
            "audit_ignore" => p.audit_ignore.extend(globs),
            other => warnings.push(format!("line {start}: unknown key `{other}`")),
        }
    }
    if !pending.trim().is_empty() {
        warnings.push(format!("line {start}: unclosed list"));
    }
    (p, warnings)
}

/// A line without its `#` comment (a `#` inside quotes stays).
fn strip_comment(line: &str) -> &str {
    let mut quoted = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '#' if !quoted => return &line[..i],
            _ => {}
        }
    }
    line
}

/// `["a", "b"]` → `["a", "b"]`; a bare string is a list of one.
fn string_array(v: &str) -> Option<Vec<String>> {
    let v = v.trim();
    let inner = if let Some(rest) = v.strip_prefix('[') {
        rest.strip_suffix(']')?.trim()
    } else {
        v
    };
    let mut out = Vec::new();
    for item in inner.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let s = item.strip_prefix('"')?.strip_suffix('"')?;
        out.push(s.to_string());
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        let g = |p: &str| Glob::new(p).unwrap();
        assert!(g("migrations/**").matches("migrations/0001_init.sql"));
        assert!(g("migrations/**").matches("migrations/a/b.py"));
        assert!(!g("migrations/**").matches("app/migrations.py"));
        assert!(g("*.snap").matches("web/__snapshots__/a.snap"));
        assert!(g("checks/*.py").matches("checks/a.py"));
        assert!(!g("checks/*.py").matches("checks/sub/a.py"));
        assert!(g("tests/golden").matches("tests/golden/out.txt"));
        assert!(g("**/fixtures/**").matches("a/b/fixtures/c.json"));
        assert!(g("src/?.rs").matches("src/a.rs") && !g("src/?.rs").matches("src/ab.rs"));
    }

    #[test]
    fn trailing_slash_means_the_directory() {
        let (p, _) = parse("protected = [\"db/migrations/\"]", "/home/dev/shop");
        assert!(p
            .protected_by("/home/dev/shop/db/migrations/0001.sql")
            .is_some());
        assert!(p.protected_by("db/migrations/a/b.sql").is_some());
    }

    #[test]
    fn parse_and_match() {
        let text = r#"
# repo policy
test_paths = ["e2e/**", "checks/*.py"]
protected = [
    "migrations/**",   # schema history
    "tests/golden/**",
]
audit_ignore = "vendor/**"
colour = ["x"]
broken = [1, 2]
"#;
        let (p, w) = parse(text, "/home/dev/shop");
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(w[0].contains("unknown key `colour`") && w[1].contains("list of strings"));
        assert!(p.is_test("/home/dev/shop/e2e/login.spec"));
        assert!(p.is_test("checks/a.py"));
        assert_eq!(
            p.protected_by("/home/dev/shop/migrations/0003.sql"),
            Some("migrations/**")
        );
        assert_eq!(p.protected_by("/home/dev/shop/src/app.py"), None);
        assert!(p.audit_ignores("vendor/lib/x.go"));
        assert!(parse("", "/").0.is_empty());
    }
}
