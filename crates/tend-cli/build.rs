//! Bake the Claude Code versions with a conformance record (`docs/conformance/<v>.md`)
//! into the binary, so `tend claude` can say when a version has not been checked.

fn main() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/conformance");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut versions: Vec<String> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            e.file_name()
                .to_str()
                .and_then(|n| n.strip_suffix(".md"))
                .map(str::to_string)
        })
        .collect();
    versions.sort();
    println!("cargo:rustc-env=TEND_CONFORMANT={}", versions.join(","));
}
