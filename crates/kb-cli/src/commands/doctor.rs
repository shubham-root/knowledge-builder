//! `kb doctor` — validate configuration and environment prerequisites.
//!
//! Full implementation: T8.

use anyhow::Result;

pub async fn run() -> Result<()> {
    // TODO (T8): run 8-point startup validation, check processor executable,
    //            check SQLite opens, check log_dir creatable, etc.
    println!("kb doctor — not yet implemented (T8)");
    Ok(())
}
