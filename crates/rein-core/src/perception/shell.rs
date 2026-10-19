//! A small reader for shell command lines: enough to find which program runs where.
//! It is not a full shell parser. When in doubt it reports `complex`, and callers
//! then do less (for example, they do not wrap the command).

/// A command line split at unquoted `&&`, `||`, `;`, `|`, `&`, and newlines.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandLine {
    /// Each simple command as words, quotes removed.
    pub segments: Vec<Vec<String>>,
    /// Groups, subshells, command substitution, here-docs, or more than one line.
    pub complex: bool,
}

/// Split `cmd` into simple commands and words.
pub fn parse(cmd: &str) -> CommandLine {
    let mut line = CommandLine::default();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = cmd.trim().chars().peekable();
    let end_word = |words: &mut Vec<String>, word: &mut String, in_word: &mut bool| {
        if *in_word {
            words.push(std::mem::take(word));
            *in_word = false;
        }
    };
    let end_segment = |line: &mut CommandLine, words: &mut Vec<String>| {
        if !words.is_empty() {
            line.segments.push(std::mem::take(words));
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                for q in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                    word.push(q);
                }
            }
            '"' => {
                in_word = true;
                while let Some(q) = chars.next() {
                    match q {
                        '"' => break,
                        '\\' => {
                            if let Some(n) = chars.next() {
                                word.push(n);
                            }
                        }
                        '$' if chars.peek() == Some(&'(') => {
                            line.complex = true;
                            word.push(q);
                        }
                        '`' => {
                            line.complex = true;
                            word.push(q);
                        }
                        _ => word.push(q),
                    }
                }
            }
            '\\' => {
                in_word = true;
                match chars.next() {
                    Some('\n') => line.complex = true,
                    Some(n) => word.push(n),
                    None => {}
                }
            }
            ' ' | '\t' => end_word(&mut words, &mut word, &mut in_word),
            // A comment runs to the end of the line.
            '#' if !in_word => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        break;
                    }
                }
            }
            '\n' => {
                line.complex = true;
                end_word(&mut words, &mut word, &mut in_word);
                end_segment(&mut line, &mut words);
            }
            ';' | '|' | '&' => {
                // `&&`, `||`, `|&`, `;;`: one separator.
                if matches!(chars.peek(), Some('&' | '|' | ';')) {
                    chars.next();
                }
                end_word(&mut words, &mut word, &mut in_word);
                end_segment(&mut line, &mut words);
            }
            '(' | ')' | '`' => {
                line.complex = true;
                end_word(&mut words, &mut word, &mut in_word);
            }
            '{' | '}' if !in_word => {
                line.complex = true;
            }
            '$' if chars.peek() == Some(&'(') => {
                line.complex = true;
                in_word = true;
                word.push(c);
            }
            '<' if chars.peek() == Some(&'<') => {
                line.complex = true;
                end_word(&mut words, &mut word, &mut in_word);
                words.push("<<".into());
                chars.next();
            }
            _ => {
                in_word = true;
                word.push(c);
            }
        }
    }
    end_word(&mut words, &mut word, &mut in_word);
    end_segment(&mut line, &mut words);
    line
}

/// `NAME=value` (a shell variable assignment).
pub fn is_assignment(w: &str) -> bool {
    match w.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !name.starts_with(|c: char| c.is_ascii_digit())
        }
        None => false,
    }
}

/// The words of a simple command after prefixes that only change how it runs:
/// `VAR=x`, `env`, `time`, `nice`, `timeout <d>`, `uv run`, `poetry run`, `pipenv run`,
/// `npx`, `pnpm exec`, `bundle exec`, `python -m`.
pub fn program_words(segment: &[String]) -> &[String] {
    let mut w = segment;
    loop {
        match w {
            [first, rest @ ..] if is_assignment(first) => w = rest,
            [first, rest @ ..] if matches!(first.as_str(), "env" | "time" | "nice" | "npx") => {
                w = rest
            }
            [first, _, rest @ ..] if first == "timeout" => w = rest,
            [a, b, rest @ ..]
                if matches!(
                    (a.as_str(), b.as_str()),
                    ("uv" | "poetry" | "pipenv" | "hatch", "run")
                        | ("pnpm" | "bundle" | "yarn", "exec")
                ) =>
            {
                w = rest
            }
            [py, m, rest @ ..]
                if m == "-m" && (py.starts_with("python") || py.ends_with("/python")) =>
            {
                w = rest
            }
            _ => return w,
        }
    }
}

/// The last path component of a program word: `/usr/bin/pytest` → `pytest`.
pub fn program_name(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segs(cmd: &str) -> Vec<Vec<&'static str>> {
        parse(cmd)
            .segments
            .into_iter()
            .map(|s| {
                s.into_iter()
                    .map(|w| &*Box::leak(w.into_boxed_str()))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn splits_at_operators_outside_quotes() {
        assert_eq!(
            segs(r#"source .venv/bin/activate && pytest -k "a && b" | tail -5; echo 'x;y'"#),
            vec![
                vec!["source", ".venv/bin/activate"],
                vec!["pytest", "-k", "a && b"],
                vec!["tail", "-5"],
                vec!["echo", "x;y"],
            ]
        );
        assert!(!parse("cargo test 2>&1 | tail").complex);
        assert_eq!(segs("false # pytest -v"), vec![vec!["false"]]);
        assert_eq!(segs("echo a#b"), vec![vec!["echo", "a#b"]]);
    }

    #[test]
    fn flags_what_it_cannot_follow() {
        for cmd in [
            "cd api\nnpm test",
            "{ cd api; npm test; }",
            "(cd api && npm test)",
            "pytest $(git diff --name-only)",
            "cat <<EOF\nx\nEOF",
            "echo `date`",
        ] {
            assert!(parse(cmd).complex, "{cmd}");
        }
    }

    #[test]
    fn finds_the_program_behind_prefixes() {
        let words = |cmd: &str| {
            let line = parse(cmd);
            program_words(&line.segments[0]).to_vec()
        };
        assert_eq!(words("CI=1 timeout 60 uv run pytest -q")[0], "pytest");
        assert_eq!(words("python3 -m pytest -x")[0], "pytest");
        assert_eq!(words("/opt/venv/bin/python -m pytest")[0], "pytest");
        assert_eq!(
            words("env RUST_LOG=debug cargo test")[..2],
            ["cargo", "test"]
        );
        assert_eq!(words("npx vitest run")[0], "vitest");
        assert_eq!(words("git commit -m 'fix pytest'")[0], "git");
        assert_eq!(program_name("/usr/local/bin/pytest"), "pytest");
    }
}
