//! Command classifier: what kind of output a shell command produces.

use serde::{Deserialize, Serialize};

use super::shell;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputKind {
    Test,
    Build,
    Install,
    Git,
    Search,
    Other,
}

impl OutputKind {
    pub const ALL: [OutputKind; 6] = [
        OutputKind::Test,
        OutputKind::Build,
        OutputKind::Install,
        OutputKind::Git,
        OutputKind::Search,
        OutputKind::Other,
    ];

    pub fn label(self) -> &'static str {
        match self {
            OutputKind::Test => "test",
            OutputKind::Build => "build",
            OutputKind::Install => "install",
            OutputKind::Git => "git",
            OutputKind::Search => "search",
            OutputKind::Other => "other",
        }
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
    /// Set when the command is `rein show <id>` (the agent re-opens a summary).
    pub show_target: Option<String>,
}

pub fn classify(command: &str) -> CommandInfo {
    let show_target = re!(r"(?:^|[\s;&|])rein\s+show\s+(o-\d+)")
        .captures(command)
        .map(|c| c[1].to_string());
    let runner = detect_runner(command);
    let kind = if runner.is_some() {
        OutputKind::Test
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
            classify("rein show o-17").show_target.as_deref(),
            Some("o-17")
        );
        assert_eq!(classify("echo reinshow o-1").show_target, None);
    }
}
