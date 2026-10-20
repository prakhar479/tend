//! Command classifier: what kind of output a shell command produces.

use serde::{Deserialize, Serialize};

use super::shell;
use crate::guard::is_test_path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputKind {
    Test,
    Build,
    Install,
    Git,
    Search,
    /// File views (`cat`, `head`, `sed -n`): the agent asked for this content.
    View,
    Other,
}

impl OutputKind {
    pub const ALL: [OutputKind; 7] = [
        OutputKind::Test,
        OutputKind::Build,
        OutputKind::Install,
        OutputKind::Git,
        OutputKind::Search,
        OutputKind::View,
        OutputKind::Other,
    ];

    pub fn label(self) -> &'static str {
        match self {
            OutputKind::Test => "test",
            OutputKind::Build => "build",
            OutputKind::Install => "install",
            OutputKind::Git => "git",
            OutputKind::Search => "search",
            OutputKind::View => "view",
            OutputKind::Other => "other",
        }
    }

    /// Kinds tend summarizes by default. The others hold what the agent asked to read
    /// (code, diffs, search hits), and the owner's data shows they are mostly file reads
    /// that a summary would cut (checkpoint C1).
    pub fn shaped_by_default(self) -> bool {
        matches!(
            self,
            OutputKind::Test | OutputKind::Build | OutputKind::Install
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestRunner {
    Pytest,
    Jest,
    Vitest,
    Cargo,
    Go,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandInfo {
    pub kind: OutputKind,
    pub runner: Option<TestRunner>,
    /// Set when the command is `tend show <id>` (the agent re-opens a summary).
    pub show_target: Option<String>,
}

pub fn classify(command: &str) -> CommandInfo {
    let show_target = re!(r"(?:^|[\s;&|])tend\s+show\s+(o-\d+)")
        .captures(command)
        .map(|c| c[1].to_string());
    let runner = detect_runner(command);
    let kind = if runner.is_some() {
        OutputKind::Test
    } else if is_view(command) {
        OutputKind::View
    } else if re!(BUILD).is_match(command) {
        OutputKind::Build
    } else if re!(INSTALL).is_match(command) {
        OutputKind::Install
    } else if re!(r"(?:^|[\s;&|(])git\s").is_match(command) {
        OutputKind::Git
    } else if re!(r"(?:^|[\s;&|(])(grep|rg|ag|find|fd|ls|tree)\b").is_match(command) {
        OutputKind::Search
    } else {
        OutputKind::Other
    };
    CommandInfo {
        kind,
        runner,
        show_target,
    }
}

const BUILD: &str = r"\bcargo\s+(build|check|clippy)\b|\btsc\b|\b(npm|pnpm|yarn|bun)\s+run\s+build\b|\bgo\s+(build|vet)\b|\bmake\b|\b(mvn|gradle|gradlew)\b|\b(webpack|next\s+build)\b|\bvite\s+build\b|\b(gcc|g\+\+|clang)\b";

const INSTALL: &str = r"\b(npm|pnpm|yarn|bun)\s+(install|i|add|ci)\b|\b(pip|pip3)\s+install\b|\buv\s+(sync|pip\s+install|add)\b|\bpoetry\s+install\b|\bcargo\s+(install|fetch)\b|\b(apt|apt-get|brew)\s+install\b|\bgo\s+(get|mod\s+download)\b";

/// Some simple command prints file content: `cat`, `head`, `tail`, `less`, `bat`, `nl`,
/// or `sed` without `-i`, with a file operand (`git log | head` reads a pipe, not a
/// file). Also inside `bash -c '…'`, as in `docker exec box bash -c 'cat -n f'`.
fn is_view(command: &str) -> bool {
    shell::parse(command)
        .segments
        .iter()
        .any(|seg| segment_views(shell::program_words(seg)))
}

fn segment_views(w: &[String]) -> bool {
    if let Some(i) = w
        .windows(2)
        .position(|p| matches!(shell::program_name(&p[0]), "bash" | "sh" | "zsh") && p[1] == "-c")
    {
        if w.get(i + 2).is_some_and(|script| is_view(script)) {
            return true;
        }
    }
    let Some(first) = w.first() else {
        return false;
    };
    let program = shell::program_name(first);
    // Operands: words that are not options, redirections, or the value of `-n`/`-c`/`-e`.
    let mut operands = Vec::new();
    let mut skip = false;
    let mut script_given = false;
    for x in &w[1..] {
        if std::mem::take(&mut skip) {
            continue;
        }
        if matches!(x.as_str(), ">" | ">>" | "<" | "2>" | "&>") {
            skip = true;
        } else if x.contains('>') || x.contains('<') {
        } else if matches!(x.as_str(), "-n" | "-c") && matches!(program, "head" | "tail") {
            skip = true;
        } else if matches!(x.as_str(), "-e" | "-f") && program == "sed" {
            skip = true;
            script_given = true;
        } else if !x.starts_with('-') || x == "-" {
            operands.push(x);
        }
    }
    match program {
        "cat" | "head" | "tail" | "less" | "more" | "bat" | "nl" => !operands.is_empty(),
        "sed" => {
            !w.iter().any(|x| x.starts_with("-i"))
                && operands.len() > if script_given { 0 } else { 1 }
        }
        _ => false,
    }
}

/// The runner of a simple command, from its program words (command position only:
/// `git commit -m "fix pytest"` runs git, not pytest).
fn runner_of(words: &[String]) -> Option<TestRunner> {
    let w = shell::program_words(words);
    let first = shell::program_name(w.first()?);
    let arg = |i: usize| w.get(i).map(String::as_str);
    match (first, arg(1)) {
        ("pytest" | "py.test", _) => Some(TestRunner::Pytest),
        ("vitest", _) => Some(TestRunner::Vitest),
        ("jest", _) => Some(TestRunner::Jest),
        ("cargo", Some("test" | "nextest")) => Some(TestRunner::Cargo),
        ("go", Some("test")) => Some(TestRunner::Go),
        ("npm" | "pnpm" | "yarn" | "bun", Some("test" | "t")) => Some(TestRunner::Other),
        ("npm" | "pnpm" | "yarn" | "bun", Some("run")) if arg(2) == Some("test") => {
            Some(TestRunner::Other)
        }
        ("mocha" | "rspec" | "phpunit" | "tox" | "nox" | "ctest", _) => Some(TestRunner::Other),
        ("mvn" | "gradle" | "gradlew", Some("test")) => Some(TestRunner::Other),
        ("make", Some("test" | "check")) => Some(TestRunner::Other),
        _ => None,
    }
}

fn detect_runner(cmd: &str) -> Option<TestRunner> {
    runner_segment(cmd).map(|(runner, _)| runner)
}

/// A key for "the same test run": the runner and its test selection, without output
/// options, pipes, redirections, or setup prefixes. Agents rarely repeat a command
/// exactly (`| tail -20` becomes `| tail -50`), so loops and re-runs compare keys (R1.3).
/// Commands without a test runner keep their text.
pub fn command_key(cmd: &str) -> String {
    let Some((_, seg)) = runner_segment(cmd) else {
        return cmd.trim().to_string();
    };
    let words = shell::program_words(&seg);
    let mut key: Vec<&str> = Vec::new();
    let mut skip_next = false;
    for (i, w) in words.iter().enumerate() {
        let w = w.as_str();
        if skip_next {
            skip_next = false;
            continue;
        }
        let name = if i == 0 { shell::program_name(w) } else { w };
        // Redirections and output-only options.
        let redirect = w.contains('>') || w.starts_with('<');
        let noise = matches!(
            w,
            "-q" | "-qq"
                | "-v"
                | "-vv"
                | "-vvv"
                | "-s"
                | "-x"
                | "--quiet"
                | "--verbose"
                | "--no-header"
                | "--nocapture"
                | "--color"
                | "--no-cov"
                | "-rA"
                | "--exitfirst"
        ) || ["--tb=", "--color=", "-r", "--maxfail=", "--durations="]
            .iter()
            .any(|p| w.starts_with(p) && w.len() > p.len());
        if w == "-p" {
            skip_next = true; // `-p no:asyncio`
            continue;
        }
        if !redirect && !noise {
            key.push(name);
        }
    }
    // A trailing `--` with nothing after it (cargo test -- --nocapture).
    while key.last() == Some(&"--") {
        key.pop();
    }
    key.join(" ")
}

/// The command (probably) writes files: `sed -i`, `perl -i`, `tee`, `cp`/`mv`/`rm`/
/// `patch`/`git apply`/`git checkout --`, an output redirection to a file, or a here-doc
/// script that writes (`open(...)`, `.write(`, `write_text`). Agents edit mostly through
/// Bash, so loop detection counts these as edits (R1.3). A heuristic: it errs toward
/// "writes".
pub fn writes_files(cmd: &str) -> bool {
    shell::parse(cmd)
        .segments
        .iter()
        .any(|seg| segment_writes(seg))
        || script_writes(cmd)
}

/// A simple command that writes files: a redirection to a file, or a writing program.
fn segment_writes(seg: &[String]) -> bool {
    let w = shell::program_words(seg);
    let Some(first) = w.first() else {
        return false;
    };
    let first = shell::program_name(first);
    let has = |flag: &str| w.iter().skip(1).any(|x| x == flag || x.starts_with(flag));
    // `>file`, `>> file`, `1>file`, `&>file`: a redirection to a real file.
    let redirect = seg.iter().enumerate().any(|(k, x)| {
        let t = x.trim_start_matches(|c: char| c.is_ascii_digit() || c == '&');
        if !t.starts_with('>') {
            return false;
        }
        let rest = t.trim_start_matches('>');
        let target = if rest.is_empty() {
            seg.get(k + 1).map(String::as_str).unwrap_or("")
        } else {
            rest
        };
        !target.is_empty() && !target.starts_with('&') && !target.starts_with("/dev/")
    });
    redirect
        || matches!(
            first,
            "tee" | "cp" | "mv" | "rm" | "patch" | "truncate" | "touch"
        )
        || (matches!(first, "sed" | "perl" | "ruby") && has("-i"))
        || (first == "git"
            && matches!(
                w.get(1).map(String::as_str),
                Some("apply" | "checkout" | "restore" | "stash")
            ))
}

/// A here-doc script that writes files (`python3 - <<EOF … open(p, "w") … EOF`).
fn script_writes(cmd: &str) -> bool {
    cmd.contains("<<")
        && (cmd.contains("open(") || cmd.contains(".write(") || cmd.contains("write_text("))
}

/// Most files one command can name for the Bash-write guard.
const MAX_WRITE_TARGETS: usize = 32;

/// Test files a shell command may write, as the command names them (R1.4): the words
/// of each writing command, and quoted strings in a writing script. A caller snapshots
/// them before the run and compares after, so listing too many costs a read, never a
/// false flag. A `*` may stand in the last path component; the caller expands it.
pub fn test_write_targets(cmd: &str) -> Vec<String> {
    write_targets(cmd, is_test_path)
}

/// Like `test_write_targets`, plus source files, which may hold tests (Rust unit tests
/// in `src/`, I2 G2). The caller keeps those whose text has test code.
pub fn code_write_targets(cmd: &str) -> Vec<String> {
    write_targets(cmd, |w| {
        is_test_path(w) || re!(r"\.(rs|py|go|[cm]?[jt]sx?|java|kt|cs|rb)$").is_match(w)
    })
}

fn write_targets(cmd: &str, keep: impl Fn(&str) -> bool) -> Vec<String> {
    let mut out = std::collections::BTreeSet::new();
    let mut consider = |w: &str| {
        let w = w.trim_start_matches(|c: char| c.is_ascii_digit() || c == '&' || c == '>');
        let (dir, _) = w.rsplit_once('/').unwrap_or(("", w));
        if keep(w)
            && !w.ends_with('/')
            && !w.contains(['$', '`', '?', '[', '{', '~'])
            && !dir.contains('*')
        {
            out.insert(w.to_string());
        }
    };
    for seg in shell::parse(cmd)
        .segments
        .iter()
        .filter(|s| segment_writes(s))
    {
        seg.iter().skip(1).for_each(|w| consider(w));
    }
    if script_writes(cmd) {
        for c in re!(r#"['"]([^'"\s]+)['"]"#).captures_iter(cmd) {
            consider(&c[1]);
        }
    }
    out.into_iter().take(MAX_WRITE_TARGETS).collect()
}

/// The first simple command that runs a test runner, and its words.
pub fn runner_segment(cmd: &str) -> Option<(TestRunner, Vec<String>)> {
    shell::parse(cmd)
        .segments
        .into_iter()
        .find_map(|seg| runner_of(&seg).map(|r| (r, seg)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_write_targets_fixture() {
        let text = include_str!("../../../../fixtures/commands/bash_writes.txt");
        let mut cases = 0;
        for case in text.split("\n== expect:").skip(1) {
            let (expect, cmd) = case.split_once('\n').unwrap();
            let want: Vec<&str> = expect.split_whitespace().collect();
            assert_eq!(test_write_targets(cmd.trim_end()), want, "{cmd}");
            cases += 1;
        }
        assert_eq!(cases, 11);
    }

    #[test]
    fn kinds() {
        assert_eq!(classify("pytest -q").kind, OutputKind::Test);
        assert_eq!(
            classify("cd api && npm test").runner,
            Some(TestRunner::Other)
        );
        assert_eq!(
            classify("cargo nextest run").runner,
            Some(TestRunner::Cargo)
        );
        assert_eq!(classify("cargo build --release").kind, OutputKind::Build);
        assert_eq!(classify("pnpm install").kind, OutputKind::Install);
        assert_eq!(classify("git log --oneline -20").kind, OutputKind::Git);
        assert_eq!(classify("rg TODO src").kind, OutputKind::Search);
        assert_eq!(classify("echo hi").kind, OutputKind::Other);
    }

    #[test]
    fn spots_bash_writes() {
        for cmd in [
            "sed -i 's/a/b/' src/app.py",
            "cat > src/app.py <<'EOF'\nx = 1\nEOF",
            "python3 - <<'EOF'\np='src/app.py'\ns=open(p).read()\nopen(p,'w').write(s)\nEOF",
            "echo x >> notes.txt",
            "git checkout -- src/app.py",
            "printf 'a' | tee src/a.py",
        ] {
            assert!(writes_files(cmd), "{cmd}");
        }
        for cmd in [
            "pytest -q 2>&1 | tail -20",
            "grep -rn foo src > /dev/null",
            "cat src/app.py",
            "sed -n '1,20p' src/app.py",
            "cargo test 2>&1 >&2",
        ] {
            assert!(!writes_files(cmd), "{cmd}");
        }
    }

    #[test]
    fn file_views() {
        for cmd in [
            "sed -n '1,80p' src/app.py",
            "cat README.md",
            "head -50 build.log",
            "cd /app && cat -n src/app.py | head -100",
            // Checkpoint C1: file reads behind another command.
            "cd /app; cat -n src/app.py",
            "cat -n src/app.py | sed -n 1,300p",
            "wc -l src/app.py && sed -n 180,260p src/app.py",
            "cd /app; cat src/models.py; grep -n device src/models.py",
            "S=/tmp/x; mkdir -p $S; tail -n 40 build.log",
            "docker exec box bash -c 'cat -n /srv/app/plot.R'",
        ] {
            assert_eq!(classify(cmd).kind, OutputKind::View, "{cmd}");
        }
        for cmd in [
            "git log --oneline | head -20",
            "git diff | sed -n 1,40p",
            "grep -rn foo src | head -n 50",
            "cat > notes.txt",
            "make 2>&1 | tail -5",
        ] {
            assert_ne!(classify(cmd).kind, OutputKind::View, "{cmd}");
        }
        assert_eq!(classify("pytest -q | tail -20").kind, OutputKind::Test);
        assert_eq!(classify("sed -i 's/a/b/' x.py").kind, OutputKind::Other);
    }

    /// R1.3: variants of one test run share a key; other targets do not.
    #[test]
    fn command_keys() {
        let same = [
            "pytest -q tests/test_api.py 2>&1 | tail -20",
            "python -m pytest tests/test_api.py -x --tb=short",
            "source .venv/bin/activate && pytest tests/test_api.py -v -p no:asyncio | grep FAIL",
            "cd /app && CI=1 timeout 120 uv run pytest tests/test_api.py -vv",
        ];
        for cmd in same {
            assert_eq!(command_key(cmd), "pytest tests/test_api.py", "{cmd}");
        }
        assert_ne!(
            command_key("pytest tests/a.py"),
            command_key("pytest tests/b.py")
        );
        assert_eq!(
            command_key("pytest -k 'total and tax' -q"),
            "pytest -k total and tax"
        );
        assert_eq!(
            command_key("cargo test orders -- --nocapture"),
            "cargo test orders"
        );
        assert_eq!(command_key("ls -la"), "ls -la");
    }

    /// R0.5 (D7): runners count only in command position.
    #[test]
    fn runner_in_command_position_only() {
        assert_eq!(classify("git commit -m 'fix pytest flake'").runner, None);
        assert_eq!(classify("grep jest package.json").runner, None);
        assert_eq!(classify("echo run pytest later").runner, None);
        assert_eq!(
            classify("source .venv/bin/activate && python -m pytest -q 2>&1 | tail -20").runner,
            Some(TestRunner::Pytest)
        );
        assert_eq!(
            classify("CI=1 npx vitest run").runner,
            Some(TestRunner::Vitest)
        );
        assert_eq!(classify("make check").kind, OutputKind::Test);
        assert_eq!(classify("pnpm run test").runner, Some(TestRunner::Other));
    }

    #[test]
    fn detects_reopen() {
        assert_eq!(
            classify("tend show o-17").show_target.as_deref(),
            Some("o-17")
        );
        assert_eq!(classify("echo reinshow o-1").show_target, None);
    }
}
