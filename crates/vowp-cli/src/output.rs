//! Human and JSON rendering shared by the commands.
//!
//! Spec: `docs/design/crates/vowp-cli.md` §2 and `docs/design/cli.md` §1. Tables are aligned
//! with two spaces between columns; `--json` writes one pretty-printed document with a
//! trailing newline and errors use the shape in `cli.md` §1.1.

use serde::Serialize;
use time::{OffsetDateTime, UtcOffset, macros::format_description};
use vowp_protocol::{ErrorKind, Timestamp};

use crate::{CliError, IoStreams};

/// An aligned text table.
#[derive(Debug, Clone, Default)]
pub struct Table {
    /// Column headers.
    pub headers: Vec<String>,
    /// Body rows; every row has one cell per header.
    pub rows: Vec<Vec<String>>,
}

impl Table {
    /// Creates an empty table with `headers`.
    pub fn new(headers: &[&str]) -> Self {
        Self {
            headers: headers.iter().map(|header| (*header).to_owned()).collect(),
            rows: Vec::new(),
        }
    }

    /// Appends one row.
    pub fn row(&mut self, cells: Vec<String>) {
        self.rows.push(cells);
    }

    /// Renders the table with aligned columns and no trailing whitespace.
    ///
    /// `color` is accepted for the frozen API; v0.1 emits no ANSI escapes, which is correct
    /// for non-TTY output and for every recorded golden.
    pub fn render(&self, color: bool) -> String {
        let _ = color;
        let mut widths = vec![0_usize; self.headers.len()];
        for (index, header) in self.headers.iter().enumerate() {
            widths[index] = widths[index].max(header.chars().count());
        }
        for row in &self.rows {
            for (index, cell) in row.iter().enumerate().take(widths.len()) {
                widths[index] = widths[index].max(cell.chars().count());
            }
        }

        let mut out = String::new();
        render_row(&mut out, &self.headers, &widths);
        for row in &self.rows {
            render_row(&mut out, row, &widths);
        }
        out
    }
}

/// Appends one padded table row.
fn render_row(out: &mut String, cells: &[String], widths: &[usize]) {
    let mut line = String::new();
    for (index, width) in widths.iter().enumerate() {
        let cell = cells.get(index).map(String::as_str).unwrap_or("");
        if index + 1 == widths.len() {
            line.push_str(cell);
        } else {
            line.push_str(&format!("{cell:<width$}  ", width = *width));
        }
    }
    out.push_str(line.trim_end());
    out.push('\n');
}

/// Writes `value` as pretty JSON with one trailing newline.
pub fn print_json<T: Serialize>(io: &mut dyn IoStreams, value: &T) -> Result<(), CliError> {
    let text = serde_json::to_string_pretty(value).map_err(vowp_protocol::ProtocolError::from)?;
    writeln!(io.out(), "{text}")?;
    Ok(())
}

/// Prints one error in the human or `--json` shape from `cli.md` §1.1.
pub fn print_error(io: &mut dyn IoStreams, error: &CliError, json: bool) {
    if json {
        let value = serde_json::json!({
            "error": {
                "kind": kind_name(error.kind()),
                "message": error.to_string(),
                "hint": error.hint(),
            }
        });
        if let Ok(text) = serde_json::to_string(&value) {
            let _ = writeln!(io.out(), "{text}");
        }
        return;
    }
    let _ = writeln!(io.err(), "vowp: {error}");
    if let Some(hint) = error.hint() {
        let _ = writeln!(io.err(), "hint: {hint}");
    }
}

/// Returns the snake_case name of an [`ErrorKind`].
pub fn kind_name(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::InvalidInput => "invalid_input",
        ErrorKind::NotFound => "not_found",
        ErrorKind::Conflict => "conflict",
        ErrorKind::Io => "io",
        ErrorKind::Db => "db",
        ErrorKind::Parse => "parse",
        ErrorKind::Config => "config",
        ErrorKind::AdapterUnavailable => "adapter_unavailable",
        ErrorKind::AdapterFailed => "adapter_failed",
        ErrorKind::Limit => "limit",
        ErrorKind::Auth => "auth",
        ErrorKind::Timeout => "timeout",
        ErrorKind::Guard => "guard",
        ErrorKind::Refused => "refused",
        ErrorKind::Remote => "remote",
        ErrorKind::Internal => "internal",
    }
}

/// Formats a timestamp in UTC: `HH:MM` when it is the same day as `now`, else `MM-DD HH:MM`.
pub fn local_time(ts: Timestamp, now: Timestamp) -> String {
    local_time_at(ts, now, 0)
}

/// Formats a timestamp in a fixed UTC offset, as `[node] tz_offset_minutes` defines it.
pub fn local_time_at(ts: Timestamp, now: Timestamp, tz_offset_minutes: i32) -> String {
    let offset = UtcOffset::from_whole_seconds(tz_offset_minutes.saturating_mul(60)).unwrap_or(UtcOffset::UTC);
    let Some(ts_local) = to_offset(ts, offset) else {
        return ts.as_rfc3339();
    };
    let Some(now_local) = to_offset(now, offset) else {
        return ts.as_rfc3339();
    };
    if ts_local.date() == now_local.date() {
        format_time(&ts_local, format_description!("[hour]:[minute]"))
    } else {
        format_time(&ts_local, format_description!("[month]-[day] [hour]:[minute]"))
    }
}

/// Converts a protocol timestamp to one fixed offset.
fn to_offset(ts: Timestamp, offset: UtcOffset) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp(ts.unix_secs())
        .ok()
        .map(|datetime| datetime.to_offset(offset))
}

/// Formats an instant with a format description, falling back to an empty string on error.
fn format_time(datetime: &OffsetDateTime, format: &[time::format_description::FormatItem<'_>]) -> String {
    datetime.format(format).unwrap_or_default()
}
