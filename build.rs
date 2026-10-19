//! Re-run the build when a migration is added or edited: `sqlx::migrate!`
//! embeds `migrations/` at compile time but cargo does not track that
//! directory on its own.

fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
