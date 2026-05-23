//! `kb storage` — byte-usage report grouped by source extension and output kind.
//!
//! Aggregates the `files` and `outputs` tables and renders a formatted table:
//!
//! ```text
//! Sources:
//!   pdf:      45 files, 234.0 MB
//!   docx:     12 files,  56.0 MB
//!
//! Outputs:
//!   markdown: 57 files,   2.3 MB
//!   asset:   120 files, 450.0 MB
//!
//! Total: 57 sources, 177 outputs, 742.3 MB tracked
//! ```

use anyhow::Result;

use super::db::{fmt_bytes, open_store};

// ─── Entry point ─────────────────────────────────────────────────────────────

pub async fn run() -> Result<()> {
    let store = open_store().await?;
    let stats = store
        .get_storage_stats()
        .await
        .map_err(|e| anyhow::anyhow!("storage query failed: {e}"))?;

    // ── Sources section ───────────────────────────────────────────────────────
    println!("Sources:");
    if stats.by_ext.is_empty() {
        println!("  (no source files tracked)");
    } else {
        // Compute column widths for aligned output.
        let ext_w = stats
            .by_ext
            .iter()
            .map(|e| e.ext.len())
            .max()
            .unwrap_or(4)
            .max(4); // at least 4 chars wide

        let count_w = stats
            .by_ext
            .iter()
            .map(|e| format_count(e.count).len())
            .max()
            .unwrap_or(5)
            .max(5);

        for row in &stats.by_ext {
            println!(
                "  {ext:<ext_w$}  {count:>count_w$} file{s}, {bytes}",
                ext      = row.ext,
                count    = format_count(row.count),
                s        = if row.count == 1 { "" } else { "s" },
                bytes    = fmt_bytes(row.bytes),
                ext_w    = ext_w,
                count_w  = count_w,
            );
        }
    }

    // ── Outputs section ───────────────────────────────────────────────────────
    println!("\nOutputs:");
    if stats.by_kind.is_empty() {
        println!("  (no output files tracked)");
    } else {
        let kind_w = stats
            .by_kind
            .iter()
            .map(|k| k.kind.len())
            .max()
            .unwrap_or(4)
            .max(4);

        let count_w = stats
            .by_kind
            .iter()
            .map(|k| format_count(k.count).len())
            .max()
            .unwrap_or(5)
            .max(5);

        for row in &stats.by_kind {
            println!(
                "  {kind:<kind_w$}  {count:>count_w$} file{s}, {bytes}",
                kind     = row.kind,
                count    = format_count(row.count),
                s        = if row.count == 1 { "" } else { "s" },
                bytes    = fmt_bytes(row.bytes),
                kind_w   = kind_w,
                count_w  = count_w,
            );
        }
    }

    // ── Summary line ─────────────────────────────────────────────────────────
    println!();
    println!(
        "Total: {} source{}, {} output{}, {} tracked",
        format_count(stats.total_source_count),
        if stats.total_source_count == 1 { "" } else { "s" },
        format_count(stats.total_output_count),
        if stats.total_output_count == 1 { "" } else { "s" },
        fmt_bytes(stats.total_bytes),
    );

    Ok(())
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Format a count as a plain decimal string (no locale separators for now;
/// keeps the display simple and parseable).
fn format_count(n: i64) -> String {
    n.to_string()
}
