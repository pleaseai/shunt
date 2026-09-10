//! Offline, read-only OpenCodex credential discovery and private snapshot export.
//! Does not invoke OpenCodex loaders, refresh OAuth, or alter routing configuration.
mod decode;
mod storage;

use anyhow::{bail, Context};
use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
};

#[derive(Debug, clap::Args)]
pub struct Options {
    /// OpenCodex home containing config.json and auth.json; defaults to OPENCODEX_HOME or ~/.opencodex.
    #[arg(long = "from")]
    pub source: Option<PathBuf>,
    /// Select one provider; repeat to select several. Default: all compatible providers.
    #[arg(long)]
    pub provider: Vec<String>,
    /// Preview names and environment variables without writing any files.
    #[arg(long)]
    pub dry_run: bool,
    /// Confirm exporting selected credentials without an interactive prompt.
    #[arg(long)]
    pub yes: bool,
    /// Parent for a new private snapshot directory; defaults to ~/.shunt/imports.
    #[arg(long)]
    pub output_dir: Option<PathBuf>,
}

pub(crate) struct Entry {
    provider: String,
    variable: String,
    secret: String,
}

pub fn run(options: Options) -> anyhow::Result<()> {
    let source = options
        .source
        .or_else(|| std::env::var_os("OPENCODEX_HOME").map(PathBuf::from))
        .or_else(|| crate::auth::shared::home_dir().map(|p| p.join(".opencodex")))
        .context("Cannot determine OpenCodex home; pass --from")?;
    let source = source
        .canonicalize()
        .context("Cannot open source directory")?;
    let config = storage::read_json(&source.join("config.json"))?;
    let auth = storage::read_json(&source.join("auth.json"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis();
    let (entries, notes) =
        decode::discover(config.as_ref(), auth.as_ref(), &options.provider, now)?;
    for note in notes {
        println!("{note}");
    }
    for entry in &entries {
        println!("Ready: {} -> {}", entry.provider, entry.variable);
    }
    println!(
        "{} credential(s). Access tokens only; no refresh tokens, routing, or settings are copied.",
        entries.len()
    );
    if entries.is_empty() {
        bail!("No compatible selected credentials to import");
    }
    if options.dry_run {
        return Ok(());
    }
    if !options.yes {
        if !io::stdin().is_terminal() {
            bail!("Use --dry-run to preview, or --yes to confirm in a non-interactive session");
        }
        print!("Export these credentials to a new private snapshot? [y/N] ");
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            bail!("Import cancelled; no files written");
        }
    }
    let destination = options
        .output_dir
        .or_else(|| crate::auth::shared::home_dir().map(|p| p.join(".shunt/imports")))
        .context("Cannot determine output directory; pass --output-dir")?;
    let file = storage::export(&source, &destination, &entries)?;
    println!("Created private credential snapshot: {}", file.display());
    println!("Source the credentials.env file in your shell before starting shunt. API-key variables must match api_key_env in your existing config. Subscription snapshots expire; re-import or log in separately. Existing settings and credentials were not modified.");
    Ok(())
}
