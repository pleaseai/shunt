//! A gate on how the other test binaries touch the process environment.
//!
//! `setenv` may reallocate the single global `environ` array, so a write to any
//! variable can be observed by a concurrent `getenv` for an unrelated one as a
//! missing value. Tests in one binary run concurrently, and the gateway reads
//! the environment as it resolves a config, so every env write in `tests/` has
//! to be serialized against every env read — see `tests/common/mod.rs`.
//!
//! Two earlier attempts at that failed the same way. `tests/multi_account.rs`
//! guarded only the tests that touched the refresh variables (#507), and
//! `tests/admin_surface.rs` had three locks, one per variable family, so a test
//! holding one still reallocated `environ` under a test holding another (#539).
//! Both looked serialized and excluded nothing.
//!
//! This gate is keyed on the *absence of the raw call* rather than on
//! recognizing a correct use of the guard. A pattern-matching gate only catches
//! the shapes it was taught: the equivalent tripwire for credential headers
//! missed two hand-rolled sites because it scanned for one idiom. There is no
//! hand-rolled spelling of "not `std::env::set_var`".

use std::path::Path;

/// Files this gate does not apply to, each with the reason it cannot take the
/// shared guard. Adding one means editing this list, which is visible in review
/// — the point of keeping the exemptions here instead of in a marker comment
/// the exempted file could grant itself.
const EXEMPT: &[(&str, &str)] = &[(
    "antigravity_process.rs",
    "writes once in a OnceLock initializer, and those values must outlive every \
     test in the binary — a guard that restores them when one test ends cannot \
     express that, and holding the lock for the life of the process would be a \
     lock nothing else can ever take",
)];

/// The calls that must not appear outside `tests/common/mod.rs`.
const RAW_WRITES: &[&str] = &["std::env::set_var", "std::env::remove_var"];

fn tests_dir() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests"))
}

/// Every `tests/*.rs` binary, as (file name, contents).
fn test_sources() -> Vec<(String, String)> {
    let mut sources: Vec<(String, String)> = std::fs::read_dir(tests_dir())
        .expect("tests/ is readable")
        .map(|entry| entry.expect("a readable directory entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .map(|path| {
            let name = path
                .file_name()
                .expect("a file name")
                .to_string_lossy()
                .into_owned();
            (
                name,
                std::fs::read_to_string(&path).expect("readable source"),
            )
        })
        .collect();
    sources.sort();
    sources
}

/// The lines of a source that are actually code, as (1-based number, line).
///
/// A line comment naming `std::env::set_var` is prose about the hazard, not a
/// call — this module's own header is full of it, and so is the doc comment on
/// the one exempt initializer. Both gates below have to agree on that, and for
/// opposite reasons: counting prose as a call would flag a file that only
/// *documents* the rule, and counting it as a call would also keep an exemption
/// alive after its last real write is gone. The second is how this was found —
/// stripping both `set_var` calls out of the exempt file left
/// `every_exemption_is_still_load_bearing` passing on the doc comment above
/// them.
fn code_lines(source: &str) -> impl Iterator<Item = (usize, &str)> {
    source
        .lines()
        .enumerate()
        .map(|(index, line)| (index + 1, line))
        .filter(|(_, line)| !line.trim_start().starts_with("//"))
}

#[test]
fn no_test_binary_writes_the_environment_outside_the_shared_guard() {
    let sources = test_sources();
    // Without this the gate passes by finding nothing — a renamed directory or a
    // path that stops resolving would read as "no violations" forever.
    assert!(
        sources.len() > 10,
        "expected to scan every tests/*.rs binary, found {}",
        sources.len()
    );

    let mut violations = Vec::new();
    for (name, source) in &sources {
        if name == "env_lock_coverage.rs" || EXEMPT.iter().any(|(exempt, _)| exempt == name) {
            continue;
        }
        for (number, line) in code_lines(source) {
            if RAW_WRITES.iter().any(|call| line.contains(call)) {
                violations.push(format!("{name}:{number}: {}", line.trim()));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "these write the process environment directly instead of through \
         `common::EnvVars`, which is what holds the lock across the write, the \
         read that resolves the config, and the restore (issue #539):\n  {}",
        violations.join("\n  ")
    );
}

#[test]
fn every_exemption_is_still_load_bearing() {
    for (name, reason) in EXEMPT {
        let path = tests_dir().join(name);
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("exempt file {name} no longer exists; drop its exemption"));
        assert!(
            code_lines(&source).any(|(_, line)| RAW_WRITES.iter().any(|call| line.contains(call))),
            "{name} no longer writes the environment directly, so its exemption \
             is stale and the gate should cover it again. The reason recorded \
             was: {reason}"
        );
    }
}
