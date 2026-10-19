//! Command classifier: what kind of output a shell command produces.

use serde::{Deserialize, Serialize};

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
    /// Set when the command is `winnow show <id>` (the agent re-opens a summary).
    pub show_target: Option<String>,
}

pub fn classify(command: &str) -> CommandInfo {
    let show_target = re!(r"(?:^|[\s;&|])winnow\s+show\s+(o-\d+)")
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

fn detect_runner(cmd: &str) -> Option<TestRunner> {
    if re!(r"\b(pytest|py\.test)\b").is_match(cmd) {
        Some(TestRunner::Pytest)
    } else if re!(r"\bvitest\b").is_match(cmd) {
        Some(TestRunner::Vitest)
    } else if re!(r"\bjest\b").is_match(cmd) {
        Some(TestRunner::Jest)
    } else if re!(r"\bcargo\s+(test|nextest)\b").is_match(cmd) {
        Some(TestRunner::Cargo)
    } else if re!(r"\bgo\s+test\b").is_match(cmd) {
        Some(TestRunner::Go)
    } else if re!(r"\b(npm|pnpm|yarn|bun)\s+(run\s+)?test\b|\b(mocha|rspec|phpunit|tox|ctest)\b|\b(mvn|gradle|gradlew)\s+test\b|\bmake\s+test\b").is_match(cmd) {
        Some(TestRunner::Other)
    } else {
        None
    }
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

    #[test]
    fn detects_reopen() {
        assert_eq!(
            classify("winnow show o-17").show_target.as_deref(),
            Some("o-17")
        );
        assert_eq!(classify("echo winnowshow o-1").show_target, None);
    }
}
