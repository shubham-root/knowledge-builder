//! `kb prune [--before <date>] [--status done|failed|skipped] [--dry-run]`
//!
//! Deletes file rows (and their cascade-deleted output rows) from the database
//! that match the given filter criteria.  Does **not** delete actual files from
//! disk.
//!
//! ## Date formats accepted by `--before`
//! - ISO date: `2025-01-01` (treated as midnight UTC)
//! - Relative:  `30d` (30 days ago), `7d` (7 days ago), etc.

use anyhow::{Context, Result};
use clap::Args;
use kb_core::Status;

use super::db::{fmt_bytes, fmt_ts, open_store, truncate_path};

// ─── CLI args ─────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct PruneArgs {
    /// Delete rows whose `updated_at` is before this date.
    ///
    /// Accepts either an ISO-8601 date (`2025-01-01`) or a relative duration
    /// suffix (`30d` = 30 days ago, `7d` = 7 days ago).
    #[arg(long, value_name = "DATE")]
    pub before: Option<String>,

    /// Only prune rows with this status.  Allowed values: `done`, `failed`,
    /// `skipped`.  Defaults to `done`.
    #[arg(long, default_value = "done", value_name = "STATUS")]
    pub status: String,

    /// Preview what would be deleted without making any DB changes.
    #[arg(long)]
    pub dry_run: bool,
}

// ─── Entry point ─────────────────────────────────────────────────────────────

pub async fn run(args: PruneArgs) -> Result<()> {
    // ── 1. Validate --status ─────────────────────────────────────────────────
    let status: Status = args.status.parse().with_context(|| {
        format!(
            "invalid status {:?} — allowed values: done, failed, skipped",
            args.status
        )
    })?;

    // Guard against statuses that should never be pruned via this command.
    match status {
        Status::Done | Status::Failed | Status::Skipped => {}
        other => {
            anyhow::bail!(
                "pruning status {other:?} is not allowed; \
                 use one of: done, failed, skipped"
            );
        }
    }

    // ── 2. Parse --before ────────────────────────────────────────────────────
    let before_ts: Option<i64> = args
        .before
        .as_deref()
        .map(parse_before)
        .transpose()
        .context("invalid --before value")?;

    // ── 3. Open state store ──────────────────────────────────────────────────
    let store = open_store().await?;

    // ── 4. Run prune (or dry-run preview) ───────────────────────────────────
    let result = store
        .prune_files(before_ts, status.clone(), args.dry_run)
        .await
        .context("prune query failed")?;

    // ── 5. Print results ─────────────────────────────────────────────────────
    if args.dry_run {
        if result.file_count == 0 {
            println!("Dry run: no matching records found.");
            return Ok(());
        }

        println!(
            "Dry run: would prune {} file record(s) and {} output record(s):\n",
            result.file_count, result.output_count
        );

        // Column widths for the table.
        let id_w     = 6;
        let status_w = 10;
        let path_w   = 45;
        let size_w   = 9;
        let date_w   = 19;

        println!(
            "  {:<id_w$}  {:<status_w$}  {:<path_w$}  {:>size_w$}  {:<date_w$}",
            "ID", "Status", "Path", "Size", "Updated",
            id_w     = id_w,
            status_w = status_w,
            path_w   = path_w,
            size_w   = size_w,
            date_w   = date_w,
        );
        println!(
            "  {:-<id_w$}  {:-<status_w$}  {:-<path_w$}  {:-<size_w$}  {:-<date_w$}",
            "", "", "", "", "",
            id_w     = id_w,
            status_w = status_w,
            path_w   = path_w,
            size_w   = size_w,
            date_w   = date_w,
        );

        for row in &result.files {
            let path_display =
                truncate_path(&row.path.to_string_lossy(), path_w);
            let size_display = row
                .size
                .map(fmt_bytes)
                .unwrap_or_else(|| "—".to_string());
            println!(
                "  {:<id_w$}  {:<status_w$}  {:<path_w$}  {:>size_w$}  {:<date_w$}",
                row.id,
                row.status.as_str(),
                path_display,
                size_display,
                fmt_ts(row.updated_at),
                id_w     = id_w,
                status_w = status_w,
                path_w   = path_w,
                size_w   = size_w,
                date_w   = date_w,
            );
        }

        println!("\n(Dry run — no changes made.)");
    } else {
        if result.file_count == 0 {
            println!("No matching records found; nothing pruned.");
        } else {
            println!(
                "Pruned {} file record(s) and {} output record(s).",
                result.file_count, result.output_count
            );
        }
    }

    Ok(())
}

// ─── Date parsing helper ──────────────────────────────────────────────────────

/// Parse a `--before` argument into a Unix epoch-second timestamp.
///
/// Supported formats:
/// - `"30d"` → 30 days before now
/// - `"7d"`  → 7 days before now
/// - `"2025-01-01"` → midnight UTC on that date
fn parse_before(s: &str) -> Result<i64> {
    // Try relative format first: "<N>d"
    if let Some(days_str) = s.strip_suffix('d') {
        let days: i64 = days_str
            .parse()
            .with_context(|| format!("invalid number of days in {s:?}"))?;
        if days < 0 {
            anyhow::bail!("relative duration must be positive, got {days}d");
        }
        let now = chrono::Utc::now().timestamp();
        return Ok(now - days * 86_400);
    }

    // Try relative format: "<N>w" (weeks)
    if let Some(weeks_str) = s.strip_suffix('w') {
        let weeks: i64 = weeks_str
            .parse()
            .with_context(|| format!("invalid number of weeks in {s:?}"))?;
        if weeks < 0 {
            anyhow::bail!("relative duration must be positive, got {weeks}w");
        }
        let now = chrono::Utc::now().timestamp();
        return Ok(now - weeks * 7 * 86_400);
    }

    // Try ISO-8601 date: "YYYY-MM-DD"
    let date = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").with_context(|| {
        format!(
            "expected ISO date (YYYY-MM-DD) or relative duration (e.g. 30d, 7d, 2w); got {s:?}"
        )
    })?;
    // Treat as midnight UTC.
    let dt = date
        .and_hms_opt(0, 0, 0)
        .expect("00:00:00 is always a valid time");
    use chrono::TimeZone as _;
    Ok(chrono::Utc.from_utc_datetime(&dt).timestamp())
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_before_relative_days() {
        let now = chrono::Utc::now().timestamp();
        let ts  = parse_before("30d").unwrap();
        let expected = now - 30 * 86_400;
        // Allow ±2 s for test execution time.
        assert!((ts - expected).abs() <= 2, "ts={ts}, expected≈{expected}");
    }

    #[test]
    fn parse_before_relative_weeks() {
        let now = chrono::Utc::now().timestamp();
        let ts  = parse_before("2w").unwrap();
        let expected = now - 14 * 86_400;
        assert!((ts - expected).abs() <= 2, "ts={ts}, expected≈{expected}");
    }

    #[test]
    fn parse_before_iso_date() {
        let ts = parse_before("2025-01-01").unwrap();
        // 2025-01-01T00:00:00Z
        assert_eq!(ts, 1_735_689_600);
    }

    #[test]
    fn parse_before_invalid_rejects() {
        assert!(parse_before("notadate").is_err());
        assert!(parse_before("30").is_err());    // no suffix
        assert!(parse_before("-5d").is_err());   // negative
    }
}
