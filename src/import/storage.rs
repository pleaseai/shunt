use super::Entry;
use anyhow::{bail, Context};
use serde_json::Value;
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

const MAX_FILE: u64 = 4 * 1024 * 1024;

pub(super) fn read_json(path: &Path) -> anyhow::Result<Option<Value>> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => bail!("Cannot safely read source credential file"),
    };
    let metadata = file.metadata().context("Cannot inspect source file")?;
    if !metadata.is_file() || metadata.len() > MAX_FILE {
        bail!("Source must be a regular file no larger than 4 MiB");
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE + 1)
        .read_to_end(&mut bytes)
        .context("Cannot read source file")?;
    if bytes.len() as u64 > MAX_FILE {
        bail!("Source exceeds 4 MiB");
    }
    // Never include serde's error: unexpected values can themselves contain secrets.
    let value = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("Malformed source JSON; source left untouched"))?;
    Ok(Some(value))
}

// Resolve existing ancestors before any mkdir so even an output through a
// symlink cannot land within the read-only source tree.
#[cfg(unix)]
fn future_path(path: &Path) -> anyhow::Result<PathBuf> {
    use std::path::Component;
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        bail!("Output path cannot contain parent traversal");
    }
    if path.exists() {
        return path.canonicalize().context("Cannot resolve output path");
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Ok(future_path(parent)?.join(path.file_name().context("Invalid output path")?))
}

#[cfg(unix)]
pub(super) fn export(
    source: &Path,
    destination: &Path,
    entries: &[Entry],
) -> anyhow::Result<PathBuf> {
    use std::{fmt::Write, os::unix::fs::DirBuilderExt};
    let destination = future_path(destination)?;
    if destination.starts_with(source) {
        bail!("Output directory must be outside the OpenCodex source directory");
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&destination)
        .context("Cannot create output parent")?;
    let snapshot = destination.join(format!("opencodex-{}", uuid::Uuid::new_v4()));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&snapshot)
        .context("Cannot create private snapshot")?;
    let path = snapshot.join("credentials.env");
    let mut contents =
        String::from("# Private shunt import. Do not commit or share. No refresh tokens.\n");
    for entry in entries {
        // Single-quote POSIX shell escaping prevents command substitution and
        // newline injection; variable names are generated, not source code.
        let escaped = entry.secret.replace('\'', "'\\''");
        writeln!(contents, "export {}='{}'", entry.variable, escaped)?;
    }
    crate::atomic_file::write_private_atomic_in_existing_dir(&path, contents.as_bytes())
        .context("Cannot finish private credential snapshot")?;
    fs::File::open(&destination)?.sync_all()?;
    Ok(path)
}

#[cfg(not(unix))]
pub(super) fn export(
    _source: &Path,
    _destination: &Path,
    _entries: &[Entry],
) -> anyhow::Result<PathBuf> {
    bail!("Credential export currently requires Unix owner-only file permissions; --dry-run remains available")
}
