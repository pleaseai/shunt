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
//! missed two hand-rolled sites because it scanned for one idiom.
//!
//! That is a claim about the *shape* of a call, and it has limits worth stating
//! rather than implying. The patterns below match the bare function name, so
//! they see every ordinary spelling — `std::env::set_var(..)`, the `use
//! std::env;` short form, and `use std::env::set_var;` — but a caller who
//! *renames* on import (`use std::env::set_var as sv;`) or reaches the same
//! libc call another way is outside what matching text can settle. Parsing the
//! source would not close that either: a rename is resolvable, but `libc::
//! setenv` through an FFI shim is not. What the gate buys is that the ordinary
//! spellings cannot appear unnoticed.

use std::path::{Path, PathBuf};

/// Files this gate does not apply to: the number of raw writes each is allowed
/// to contain, and the reason it cannot route them through the shared guard.
///
/// Keeping the exemptions here rather than in a marker comment means a file
/// cannot grant itself one — adding a row is visible in review. The count is
/// what makes the exemption a bound instead of a blanket: exempting the file
/// wholesale would let a later PR add an ordinary per-test `set_var` beside the
/// vetted one and stay green, which is the hazard this gate exists to catch.
const EXEMPT: &[Exemption] = &[
    Exemption {
        file: "common/mod.rs",
        writes: 4,
        within: Within::Anywhere,
        reason: "is the guard itself — two writes in `set`/`unset` and the two that \
                 put the previous values back in `Drop`, all under the lock this \
                 module owns",
    },
    Exemption {
        file: "env_guard.rs",
        writes: 2,
        within: Within::ABlockHoldingTheLock,
        reason: "seeds a pre-existing value so the `Some(..)` restore branch of \
                 `Drop` is reachable at all — which the guard cannot do for \
                 itself, since leaving nothing behind is its whole job. Both \
                 writes are taken under the lock",
    },
    Exemption {
        file: "antigravity_process.rs",
        writes: 2,
        within: Within::Region("STUB.get_or_init("),
        reason: "writes once in a OnceLock initializer, and those values must \
                 outlive every test in the binary — a guard that restores them \
                 when one test ends cannot express that, and holding the lock for \
                 the life of the process would be a lock nothing else can ever take",
    },
];

/// One file the gate does not apply to, and the bounds that keep the exemption
/// honest.
///
/// Keeping exemptions here rather than in a marker comment means a file cannot
/// grant itself one — adding a row is visible in review. `writes` and `within`
/// are what make a row a bound instead of a blanket: exempting the file
/// wholesale would let a later PR add an ordinary per-test `set_var` beside the
/// vetted one, or move a vetted one out of the construct that justified it, and
/// stay green.
struct Exemption {
    file: &'static str,
    /// How many raw writes this file is allowed to contain.
    writes: usize,
    /// Where they must sit for the recorded reason to still describe them.
    within: Within,
    reason: &'static str,
}

/// The region an exemption's raw writes must stay inside.
///
/// Naming the *enclosing function* was not enough, and the gap is worth stating
/// because it is the one this check exists to close: a write can stay in
/// `stub_agy` while moving out of `STUB.get_or_init`, or stay in the guard's
/// self-test while its `let _lock` is deleted, and in both cases the text is
/// still inside the named function while the synchronization that justified the
/// exemption is gone. So the bound is the construct that does the synchronizing,
/// not the function containing it.
enum Within {
    /// No bound — for the guard's own implementation, where every write *is* the
    /// mechanism under review.
    Anywhere,
    /// Every write sits inside the block opened at this anchor text, so a write
    /// that leaves the construct trips the gate even when the count still
    /// matches.
    Region(&'static str),
    /// Every write sits in a block that also takes the lock, which is what makes
    /// a deliberately-raw write safe rather than merely intentional.
    ABlockHoldingTheLock,
}

/// The calls that must not appear outside the exempt files.
///
/// The bare function name, so every path spelling is covered at once:
/// `std::env::set_var`, `env::set_var` after `use std::env;`, and a plain
/// `set_var(..)` after `use std::env::set_var;` all contain it. Nothing else in
/// `tests/` is named this — the guard's own methods are `set` and `unset`.
const RAW_WRITES: &[&str] = &["set_var", "remove_var"];

fn tests_dir() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests"))
}

/// Every `.rs` file under `tests/`, as (path relative to `tests/`, contents).
///
/// Recursive, because a test binary is not only `tests/*.rs`: cargo also builds
/// `tests/<dir>/main.rs`, and a support module in a subdirectory is compiled
/// into whichever binaries declare it. `tests/common/mod.rs` is the one that
/// exists today, and it is exempt by name below rather than by being out of
/// reach of the walk.
fn test_sources() -> Vec<(String, String)> {
    fn walk(dir: &Path, into: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("a readable directory under tests/") {
            let path = entry.expect("a readable directory entry").path();
            if path.is_dir() {
                walk(&path, into);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                into.push(path);
            }
        }
    }

    let mut paths = Vec::new();
    walk(tests_dir(), &mut paths);

    let mut sources: Vec<(String, String)> = paths
        .into_iter()
        .map(|path| {
            let name = path
                .strip_prefix(tests_dir())
                .expect("a path under tests/")
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
/// A line comment naming `env::set_var` is prose about the hazard, not a call —
/// this module's own header is full of it, and so is the doc comment on the one
/// exempt initializer. Both gates below have to agree on that, and for opposite
/// reasons: counting prose as a call would flag a file that only *documents* the
/// rule, and counting it as a call would keep an exemption alive after its last
/// real write is gone. The second is how this was found — stripping both
/// `set_var` calls out of the exempt file left `every_exemption_is_bounded_and_
/// still_load_bearing` passing on the doc comment above them.
fn code_lines(source: &str) -> impl Iterator<Item = (usize, &str)> {
    source
        .lines()
        .enumerate()
        .map(|(index, line)| (index + 1, line))
        .filter(|(_, line)| !line.trim_start().starts_with("//"))
}

/// Every raw write in one source, as `(occurrences on the line, "path:line: text")`.
///
/// Occurrences, not matching lines: two calls on one line are two writes, and a
/// count that scored them as one would let an exempt file take on an extra write
/// without tripping its bound.
fn raw_writes_in(name: &str, source: &str) -> Vec<(usize, usize, String)> {
    code_lines(source)
        .filter_map(|(number, line)| {
            let hits: usize = RAW_WRITES
                .iter()
                .map(|call| line.matches(call).count())
                .sum();
            (hits > 0).then(|| (hits, number, format!("{name}:{number}: {}", line.trim())))
        })
        .collect()
}

/// The byte range of the block opened at the first `{` at or after `anchor`.
fn block_at(source: &str, anchor: &str) -> Option<(usize, usize)> {
    let at = source.find(anchor)?;
    let open = source[at..].find('{')? + at;
    let mut depth = 0usize;
    for (offset, ch) in source[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((open, open + offset));
                }
            }
            _ => {}
        }
    }
    None
}

/// The innermost block containing `at`, as a byte range.
fn enclosing_block(source: &str, at: usize) -> Option<(usize, usize)> {
    let mut open = None;
    let mut depth = 0i32;
    for (offset, ch) in source[..at].char_indices().rev() {
        match ch {
            '}' => depth += 1,
            '{' => {
                if depth == 0 {
                    open = Some(offset);
                    break;
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    let open = open?;
    let mut depth = 0usize;
    for (offset, ch) in source[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((open, open + offset));
                }
            }
            _ => {}
        }
    }
    None
}

/// The byte offset of every raw write on a code line.
fn raw_write_offsets(source: &str) -> Vec<usize> {
    let mut at = Vec::new();
    let mut base = 0usize;
    for line in source.lines() {
        if !line.trim_start().starts_with("//") {
            for call in RAW_WRITES {
                let mut from = 0usize;
                while let Some(found) = line[from..].find(call) {
                    at.push(base + from + found);
                    from += found + call.len();
                }
            }
        }
        base += line.len() + 1;
    }
    at
}

#[test]
fn no_test_binary_writes_the_environment_outside_the_shared_guard() {
    let sources = test_sources();
    // Without this the gate passes by finding nothing — a renamed directory or a
    // path that stops resolving would read as "no violations" forever.
    assert!(
        sources.len() > 10,
        "expected to scan every test source under tests/, found {}",
        sources.len()
    );

    let mut violations = Vec::new();
    for (name, source) in &sources {
        if name == "env_lock_coverage.rs" || EXEMPT.iter().any(|e| e.file == name) {
            continue;
        }
        violations.extend(raw_writes_in(name, source).into_iter().map(|(_, _, at)| at));
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
fn every_exemption_is_bounded_and_still_load_bearing() {
    for exemption in EXEMPT {
        let Exemption {
            file,
            writes,
            within,
            reason,
        } = exemption;
        let path = tests_dir().join(file);
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("exempt file {file} no longer exists; drop its exemption"));
        let found = raw_writes_in(file, &source);
        let total: usize = found.iter().map(|(hits, _, _)| hits).sum();
        let listed = found
            .iter()
            .map(|(_, _, at)| at.clone())
            .collect::<Vec<_>>()
            .join("\n  ");

        assert!(
            total > 0,
            "{file} no longer writes the environment directly, so its exemption \
             is stale and the gate should cover it again. The reason recorded \
             was: {reason}"
        );
        assert_eq!(
            total, *writes,
            "{file} is exempt for {writes} raw write(s) and has {total}. A new one \
             is not covered by the recorded reason — route it through \
             `common::EnvVars`, or, if it genuinely cannot be, change the count \
             here and say why in the reason. The reason recorded was: \
             {reason}\n  {listed}"
        );

        // A write that leaves the construct the reason names is no longer the
        // write that was justified, even though the count still matches.
        match within {
            Within::Anywhere => {}
            Within::Region(anchor) => {
                let (start, end) = block_at(&source, anchor).unwrap_or_else(|| {
                    panic!("{file} no longer contains `{anchor}`, which its exemption is scoped to")
                });
                let strayed: Vec<_> = raw_write_offsets(&source)
                    .into_iter()
                    .filter(|at| *at < start || *at > end)
                    .collect();
                assert!(
                    strayed.is_empty(),
                    "{file} has {} raw write(s) outside the `{anchor}` block its \
                     exemption covers. Staying inside the enclosing function is \
                     not the same thing — the block is what does the \
                     synchronizing. The reason recorded was: {reason}\n  {listed}",
                    strayed.len()
                );
            }
            Within::ABlockHoldingTheLock => {
                for at in raw_write_offsets(&source) {
                    let (start, end) = enclosing_block(&source, at)
                        .unwrap_or_else(|| panic!("{file}: a raw write sits in no block at all"));
                    let block = &source[start..end];
                    assert!(
                        block.contains("common::set_env_blocking")
                            || block.contains("common::set_env")
                            || block.contains("common::env_lock"),
                        "{file} has a raw write in a block that does not take the \
                         lock, so it is merely deliberate rather than safe. The \
                         reason recorded was: {reason}\n  {listed}"
                    );
                }
            }
        }
    }
}
