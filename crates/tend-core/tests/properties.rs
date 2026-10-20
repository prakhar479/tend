//! Property tests (I3, T4). A small seeded generator stands in for a property-testing
//! crate: each case is reproducible from its seed, printed on failure.

use std::collections::BTreeSet;

use tend_core::perception::{command_key, extract_failures, normalize_message, shell};
use tend_core::shape::shape;

/// xorshift64*: enough randomness for test inputs, no dependency.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[self.below(v.len())]
    }
}

const FIXTURES: [&str; 8] = [
    include_str!("../../../fixtures/outputs/pytest_fail.txt"),
    include_str!("../../../fixtures/outputs/cargo_fail.txt"),
    include_str!("../../../fixtures/outputs/go_test_fail.txt"),
    include_str!("../../../fixtures/outputs/go_test_v_fail.txt"),
    include_str!("../../../fixtures/outputs/jest_fail.txt"),
    include_str!("../../../fixtures/outputs/vitest_fail.txt"),
    include_str!("../../../fixtures/outputs/repeated_traces.txt"),
    include_str!("../../../fixtures/outputs/pytest_cut_by_claude.txt"),
];

/// Noise an agent's output may hold between real lines.
fn noise(rng: &mut Rng) -> String {
    let n = rng.below(400);
    let kinds = [
        "tests/test_mod.py::test_ok PASSED",
        "test orders::tests::ok ... ok",
        "=== RUN   TestOk",
        "--- PASS: TestOk (0.00s)",
        "  ✓ renders (3 ms)",
        "",
        "Downloading crates ...",
        "\u{1b}[32mok\u{1b}[0m",
    ];
    (0..n).map(|_| format!("{}\n", rng.pick(&kinds))).collect()
}

fn signatures(text: &str) -> BTreeSet<String> {
    extract_failures(text)
        .into_iter()
        .map(|f| f.signature)
        .collect()
}

/// Shaping never loses a failure, never adds lines, and keeps a truncation marker.
#[test]
fn shape_keeps_every_failure() {
    for seed in 0..300u64 {
        let mut rng = Rng::new(seed);
        let fixture = *rng.pick(&FIXTURES);
        let mut text = noise(&mut rng) + fixture + "\n" + &noise(&mut rng);
        if rng.below(3) == 0 {
            text = text.replace('\n', "\r\n");
        }
        let level = 1 + rng.below(3) as u8;
        let Some(s) = shape(&text, level, 1.0) else {
            continue;
        };
        let (before, after) = (signatures(&text), signatures(&s.text));
        assert!(
            before.is_subset(&after),
            "seed {seed} level {level}: lost {:?}\n{}",
            before.difference(&after).collect::<Vec<_>>(),
            s.text
        );
        assert!(s.to_lines <= s.from_lines, "seed {seed}");
        if text.contains("characters truncated] ...") {
            assert!(s.text.contains("characters truncated] ..."), "seed {seed}");
        }
    }
}

/// Run-to-run details do not change a signature; a different value does.
#[test]
fn normalization_ignores_noise_and_keeps_values() {
    let locations = [
        "/home/dev/shop/tests/test_api.py:12",
        "/tmp/ci-81/shop/tests/test_api.py:907",
        "C:\\work\\shop\\tests\\test_api.py:3",
    ];
    for seed in 0..300u64 {
        let mut rng = Rng::new(seed);
        // Both messages get the same kind of detail, with different values.
        let kind = rng.below(5);
        let at = |rng: &mut Rng| match kind {
            0 => rng.pick(&locations).to_string(),
            1 => format!("0x{:x}", rng.next()),
            2 => format!(
                "2026-10-{:02}T{:02}:13:44.5Z",
                1 + rng.below(28),
                rng.below(24)
            ),
            3 => format!("{}.{:02}s", rng.below(100), rng.below(100)),
            _ => format!(
                "{:08x}-{:04x}-4{:03x}-a{:03x}-{:012x}",
                rng.next() as u32,
                rng.below(65536),
                rng.below(4096),
                rng.below(4096),
                rng.next() & 0xffff_ffff_ffff
            ),
        };
        let value = rng.below(1000);
        let (a, b) = (at(&mut rng), at(&mut rng));
        let one = format!("{a}: AssertionError: assert total() == {value} (in run {b})");
        let (c, d) = (at(&mut rng), at(&mut rng));
        let two = format!("{c}: AssertionError: assert total() == {value} (in run {d})");
        assert_eq!(
            normalize_message(&one),
            normalize_message(&two),
            "seed {seed}\n{one}\n{two}"
        );
        let other = format!(
            "{a}: AssertionError: assert total() == {} (in run {b})",
            value + 1
        );
        assert_ne!(
            normalize_message(&one),
            normalize_message(&other),
            "seed {seed}"
        );
    }
}

/// Output options and pipes do not change a command key; the selection does.
#[test]
fn command_keys_follow_the_selection() {
    let decor = [
        "",
        " -q",
        " -v",
        " 2>&1 | tail -20",
        " | grep FAIL",
        " --tb=short",
        " -x",
        " > /tmp/out.log",
    ];
    let prefixes = ["", "source .venv/bin/activate && ", "CI=1 ", "timeout 120 "];
    for seed in 0..300u64 {
        let mut rng = Rng::new(seed);
        let target = format!("tests/test_{}.py", rng.below(50));
        let run = |rng: &mut Rng, t: &str| {
            format!(
                "{}pytest {t}{}{}",
                rng.pick(&prefixes),
                rng.pick(&decor),
                rng.pick(&decor)
            )
        };
        let (x, y) = (run(&mut rng, &target), run(&mut rng, &target));
        assert_eq!(command_key(&x), command_key(&y), "seed {seed}: {x} | {y}");
        let z = run(&mut rng, &format!("{target}::test_other"));
        assert_ne!(command_key(&x), command_key(&z), "seed {seed}: {x} | {z}");
    }
}

/// The shell reader never panics, and its prefix lengths are valid slice points.
#[test]
fn shell_reader_never_panics() {
    let atoms = [
        "pytest",
        " ",
        "'",
        "\"",
        "\\",
        "$(",
        ")",
        "`",
        "&&",
        "||",
        ";",
        "|",
        "&",
        "\n",
        "#",
        "<<EOF",
        "EOF",
        "<<<",
        ">",
        "2>&1",
        "cd ",
        "/tmp",
        "{",
        "}",
        "$'",
        "é",
        "日本",
        "=",
        "sed -i ",
        "tests/test_a.py",
        "*",
        "-p",
        "--",
    ];
    for seed in 0..3000u64 {
        let mut rng = Rng::new(seed);
        let n = rng.below(24);
        let cmd: String = (0..n).map(|_| *rng.pick(&atoms)).collect();
        let _ = shell::parse(&cmd);
        let (prefix, _) = shell::leading_cd(&cmd);
        assert!(cmd.is_char_boundary(prefix), "seed {seed}: {cmd:?}");
        let _ = command_key(&cmd);
        let _ = tend_core::perception::test_write_targets(&cmd);
        let _ = tend_core::perception::classify::classify(&cmd);
    }
}
