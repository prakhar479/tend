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
    } else {
        // Programs in command position only: `grep -rn make src/` searches, it does not
        // build (I3, S1).
        let segments = shell::parse(command).segments;
        let kinds: Vec<OutputKind> = segments
            .iter()
            .filter_map(|seg| kind_of(shell::program_words(seg)))
            .collect();
        [
            OutputKind::Build,
            OutputKind::Install,
            OutputKind::Git,
            OutputKind::Search,
        ]
        .into_iter()
        .find(|k| kinds.contains(k))
        .unwrap_or(OutputKind::Other)
    };
    CommandInfo {
        kind,
        runner,
        show_target,
    }
}

/// The kind of output a simple command makes, from its program (not a runner or a view).
fn kind_of(w: &[String]) -> Option<OutputKind> {
    let first = shell::program_name(w.first()?);
    let arg = |i: usize| w.get(i).map(String::as_str);
    let pkg = matches!(first, "npm" | "pnpm" | "yarn" | "bun");
    let kind = match (first, arg(1)) {
        ("cargo", Some("build" | "check" | "clippy" | "b" | "c" | "doc")) => OutputKind::Build,
        (
            "tsc" | "webpack" | "make" | "mvn" | "gradle" | "gradlew" | "gcc" | "g++" | "clang"
            | "clang++" | "cc" | "cmake" | "ninja" | "javac",
            _,
        ) => OutputKind::Build,
        ("go", Some("build" | "vet")) | ("next" | "vite", Some("build")) => OutputKind::Build,
        (_, Some("build")) if pkg => OutputKind::Build,
        (_, Some("run"))
            if pkg && arg(2).is_some_and(|s| s == "build" || s.starts_with("build:")) =>
        {
            OutputKind::Build
        }
        (_, Some("install" | "i" | "add" | "ci")) if pkg => OutputKind::Install,
        ("pip" | "pip3" | "conda" | "mamba" | "gem", Some("install")) => OutputKind::Install,
        ("uv", Some("sync" | "add")) | ("poetry", Some("install" | "add")) => OutputKind::Install,
        ("uv", Some("pip")) if arg(2) == Some("install") => OutputKind::Install,
        ("cargo", Some("install" | "fetch")) | ("bundle", Some("install")) => OutputKind::Install,
        ("apt" | "apt-get" | "brew", Some("install")) | ("go", Some("get")) => OutputKind::Install,
        ("go", Some("mod")) if arg(2) == Some("download") => OutputKind::Install,
        ("git", _) => OutputKind::Git,
        (
            "grep" | "egrep" | "fgrep" | "rg" | "ag" | "ack" | "find" | "fd" | "ls" | "tree"
            | "locate",
            _,
        ) => OutputKind::Search,
        _ => return None,
    };
    Some(kind)
}

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
    // Options of a package manager or make before the script or target:
    // `pnpm --filter api test`, `make -C api test`, `npm --prefix web test`.
    let rest = skip_options(first, &w[1..]);
    let arg = |i: usize| rest.get(i).map(String::as_str);
    let test_name =
        |s: &str| s == "test" || s == "t" || s.starts_with("test:") || s.starts_with("test-");
    match (first, arg(0)) {
        ("pytest" | "py.test", _) => Some(TestRunner::Pytest),
        ("vitest", _) => Some(TestRunner::Vitest),
        ("jest", _) => Some(TestRunner::Jest),
        ("cargo", Some("test" | "nextest" | "t")) => Some(TestRunner::Cargo),
        ("go", Some("test")) => Some(TestRunner::Go),
        // `pnpm vitest run`, `yarn jest`: the package manager runs the runner.
        ("npm" | "pnpm" | "yarn" | "bun", Some("vitest")) => Some(TestRunner::Vitest),
        ("npm" | "pnpm" | "yarn" | "bun", Some("jest")) => Some(TestRunner::Jest),
        ("npm" | "pnpm" | "yarn" | "bun", Some(s)) if test_name(s) => Some(TestRunner::Other),
        ("npm" | "pnpm" | "yarn" | "bun", Some("run")) if arg(1).is_some_and(test_name) => {
            Some(TestRunner::Other)
        }
        ("unittest" | "mocha" | "rspec" | "phpunit" | "tox" | "nox" | "ctest" | "nose2", _) => {
            Some(TestRunner::Other)
        }
        // `python manage.py test` (Django).
        (py, Some(script))
            if (py.starts_with("python") || py == "./manage.py" || py == "manage.py")
                && (script.ends_with("manage.py") && arg(1) == Some("test")
                    || py.ends_with("manage.py") && script == "test") =>
        {
            Some(TestRunner::Other)
        }
        ("mvn" | "gradle" | "gradlew", Some(s)) if s == "test" || s == "check" => {
            Some(TestRunner::Other)
        }
        ("make", Some(s))
            if s == "test" || s == "check" || test_name(s) || s.starts_with("check-") =>
        {
            Some(TestRunner::Other)
        }
        ("deno" | "dotnet" | "swift" | "mix", Some("test")) => Some(TestRunner::Other),
        ("rake", Some("test" | "spec")) => Some(TestRunner::Other),
        _ => None,
    }
}

/// Words after leading options that take a value (or not) for package managers and make.
fn skip_options<'a>(program: &str, mut w: &'a [String]) -> &'a [String] {
    let with_value: &[&str] = match program {
        "npm" | "pnpm" | "yarn" | "bun" => &[
            "--filter",
            "-F",
            "--prefix",
            "-C",
            "--dir",
            "--workspace",
            "-w",
            "--cwd",
        ],
        "make" => &["-C", "-f", "-j", "--directory", "--file"],
        _ => return w,
    };
    while let Some(o) = w.first().filter(|o| o.starts_with('-')) {
        let n = if with_value.contains(&o.as_str()) {
            2
        } else {
            1
        };
        w = &w[n.min(w.len())..];
    }
    w
}

fn detect_runner(cmd: &str) -> Option<TestRunner> {
    runner_segment(cmd).map(|(runner, _)| runner)
}

/// A key for "the same test run": the runner and its test selection, without output
/// options, pipes, redirections, or setup prefixes. Agents rarely repeat a command
/// exactly (`| tail -20` becomes `| tail -50`), so loops and re-runs compare keys (R1.3).
/// Commands without a test runner keep their text.
pub fn command_key(cmd: &str) -> String {
    let Some((runner, seg)) = runner_segment(cmd) else {
        return cmd.trim().to_string();
    };
    let pytest = runner == TestRunner::Pytest;
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
        // Redirections and output-only options. A bare `>` takes the next word with it.
        let redirect = w.contains('>') || w.starts_with('<');
        if redirect
            && w.trim_start_matches(|c: char| c.is_ascii_digit() || c == '&')
                .trim_start_matches('>')
                .trim_start_matches('<')
                .is_empty()
        {
            skip_next = true;
            continue;
        }
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
                | "--exitfirst"
        ) || ["--tb=", "--color=", "--maxfail=", "--durations="]
            .iter()
            .any(|p| w.starts_with(p) && w.len() > p.len())
            // pytest's report options (`-rA`, `-rfE`); go's `-run=TestA` is a selection.
            || (pytest && re!(r"^-r[a-zA-Z]+$").is_match(w));
        // pytest's `-p no:asyncio` loads a plugin; cargo's `-p crate` selects a package.
        if pytest && w == "-p" {
            skip_next = true;
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
    let key = key.join(" ");
    // With no selection, the directory is the selection: `cd api && npm test` and
    // `cd web && npm test` run different suites (I3, L3).
    let bare = key
        .split(' ')
        .skip(1)
        .all(|w| matches!(w, "test" | "t" | "run" | "check" | "nextest"));
    let (_, dirs) = shell::leading_cd(cmd);
    match dirs.last() {
        Some(d) if bare => format!("{key} (in {})", d.trim_end_matches('/')),
        _ => key,
    }
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
        !target.is_empty() && !target.starts_with('&') && !is_scratch(target)
    });
    // `pytest | tee /tmp/t.log` keeps a copy of the output; it edits nothing (I3, L2).
    let tee = first == "tee"
        && w.iter()
            .skip(1)
            .any(|x| !x.starts_with('-') && !is_scratch(x));
    redirect
        || tee
        || matches!(first, "cp" | "mv" | "rm" | "patch" | "truncate" | "touch")
        || (matches!(first, "sed" | "perl" | "ruby") && has("-i"))
        || (first == "git"
            && matches!(
                w.get(1).map(String::as_str),
                Some("apply" | "checkout" | "restore" | "stash")
            ))
}

/// A place for logs and scratch output, not the project's files.
fn is_scratch(target: &str) -> bool {
    target.starts_with("/dev/")
        || target.starts_with("/tmp/")
        || target.starts_with("/var/tmp/")
        || target.starts_with("$TMPDIR")
        || target.starts_with("${TMPDIR")
        || target.ends_with(".log")
        || target.ends_with(".out")
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
    write_targets_where(cmd, is_test_path)
}

/// Like `test_write_targets`, plus source files, which may hold tests (Rust unit tests
/// in `src/`, I2 G2). The caller keeps those whose text has test code.
pub fn code_write_targets(cmd: &str) -> Vec<String> {
    write_targets_where(cmd, crate::guard::may_hold_tests)
}

/// Files a shell command may write, as it names them, that `keep` accepts.
pub fn write_targets_where(cmd: &str, keep: impl Fn(&str) -> bool) -> Vec<String> {
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

/// A command that rewrites expected snapshots or golden files wholesale (I6 E5):
/// `jest -u`, `vitest -u`, `--update-snapshots`, `cargo insta accept`,
/// `pytest --snapshot-update`, `--force-regen`, `go test -update`, `UPDATE_GOLDEN=1`.
/// Returns the part that matched.
pub fn snapshot_update(cmd: &str) -> Option<String> {
    let runner_flag =
        re!(r"\b(jest|vitest)\b[^|;&]*?\s(-u|--update(Snapshot)?|--updateSnapshot)\b");
    let other = re!(
        r"--update-snapshots?\b|--updateSnapshot\b|--snapshot-update\b|--force-regen\b|--update-goldens?\b|\binsta\s+(accept|review)\b|\bcargo\s+insta\s+test\b[^|;&]*--accept\b|\bINSTA_UPDATE=(always|new)\b|\bUPDATE_(GOLDEN|GOLDENS|SNAPSHOT|SNAPSHOTS|EXPECT)=\S+|\bgo\s+test\b[^|;&]*\s-update\b"
    );
    runner_flag
        .find(cmd)
        .or_else(|| other.find(cmd))
        .map(|m| m.as_str().trim().to_string())
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

    /// I3: build, install, git, and search come from the program in command position.
    #[test]
    fn kinds_come_from_the_program() {
        for (cmd, kind) in [
            ("grep -rn make src/", OutputKind::Search),
            ("rg 'gcc' docs", OutputKind::Search),
            ("git commit -m 'make it faster'", OutputKind::Git),
            ("ls ~/.gradle", OutputKind::Search),
            ("echo make sure", OutputKind::Other),
            ("grep -rn 'pip install' docs", OutputKind::Search),
            (
                "source .venv/bin/activate && black --check src",
                OutputKind::Other,
            ),
            ("cargo build --release 2>&1 | tail -5", OutputKind::Build),
            ("npx tsc --noEmit", OutputKind::Build),
            ("pnpm run build:web", OutputKind::Build),
            ("make -j8", OutputKind::Build),
            ("python -m pip install -e .", OutputKind::Install),
            ("uv pip install requests", OutputKind::Install),
            ("cd web && npm ci", OutputKind::Install),
            ("git status && git diff --stat", OutputKind::Git),
        ] {
            assert_eq!(classify(cmd).kind, kind, "{cmd}");
        }
    }

    /// I3, L7: runners behind package managers, options, and other frameworks.
    #[test]
    fn more_runners() {
        for (cmd, runner) in [
            ("python -m unittest discover -s tests", TestRunner::Other),
            ("python manage.py test shop", TestRunner::Other),
            ("./manage.py test", TestRunner::Other),
            ("pnpm vitest run", TestRunner::Vitest),
            ("yarn jest --ci", TestRunner::Jest),
            ("bunx vitest", TestRunner::Vitest),
            ("make -C api test", TestRunner::Other),
            ("make test-unit", TestRunner::Other),
            ("npm run test:unit", TestRunner::Other),
            (
                "uv run --with pytest-xdist --frozen pytest -n 4",
                TestRunner::Pytest,
            ),
            ("pnpm --filter api test", TestRunner::Other),
            ("deno test", TestRunner::Other),
            ("dotnet test", TestRunner::Other),
            ("cargo t", TestRunner::Cargo),
        ] {
            assert_eq!(classify(cmd).runner, Some(runner), "{cmd}");
        }
        assert_eq!(classify("python manage.py migrate").runner, None);
        assert_eq!(classify("make build").runner, None);
    }

    /// I3, L3: different selections give different keys.
    #[test]
    fn keys_keep_the_selection() {
        assert_ne!(
            command_key("cargo test -p tend-core"),
            command_key("cargo test -p tend-cli")
        );
        assert_ne!(
            command_key("go test -run=TestA ./pkg"),
            command_key("go test -run=TestB ./pkg")
        );
        assert_ne!(
            command_key("cd api && npm test"),
            command_key("cd web && npm test")
        );
        assert_eq!(command_key("cd api && npm test"), "npm test (in api)");
        assert_eq!(
            command_key("pytest -rfE -p no:x tests/a.py"),
            "pytest tests/a.py"
        );
    }

    /// E5: commands that rewrite snapshots wholesale.
    #[test]
    fn snapshot_updates() {
        for cmd in [
            "npx jest -u",
            "pnpm vitest run --update",
            "npm test -- --updateSnapshot",
            "cargo insta accept",
            "cargo insta test --accept",
            "pytest --snapshot-update tests/",
            "pytest --force-regen",
            "UPDATE_GOLDEN=1 go test ./...",
            "go test ./pkg -update",
            "npx playwright test --update-snapshots",
        ] {
            assert!(snapshot_update(cmd).is_some(), "{cmd}");
        }
        for cmd in [
            "npx jest",
            "git log -u",
            "pytest -q",
            "cargo update",
            "apt-get update",
        ] {
            assert!(snapshot_update(cmd).is_none(), "{cmd}");
        }
    }

    /// I3, L2: keeping a copy of the output is not an edit.
    #[test]
    fn output_logs_are_not_writes() {
        for cmd in [
            "pytest -q 2>&1 | tee /tmp/t.log",
            "cargo test > /tmp/out.txt; tail -5 /tmp/out.txt",
            "make check 2>&1 | tee build.log",
            "pytest > run.out",
        ] {
            assert!(!writes_files(cmd), "{cmd}");
        }
        for cmd in ["echo x | tee src/a.py", "echo x >> requirements.txt"] {
            assert!(writes_files(cmd), "{cmd}");
        }
    }

    /// I3, S3: a comment ends at the newline; the next line is its own command.
    #[test]
    fn comment_does_not_join_lines() {
        assert_eq!(
            classify("cd api  # go there\npytest -q").runner,
            Some(TestRunner::Pytest)
        );
        assert_eq!(
            test_write_targets("cd /app  # repo root\nsed -i 's/==/!=/' tests/test_a.py"),
            ["tests/test_a.py"]
        );
        // S4: ANSI-C quoting does not swallow the rest of the line.
        assert_eq!(
            classify("echo $'a\\'b'; pytest -q").runner,
            Some(TestRunner::Pytest)
        );
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
