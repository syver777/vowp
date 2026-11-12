//! Claude Code transcript importer (`vowp session watch`) and the app mirror's reader.
//!
//! Claude Code writes one JSON object per line to
//! `<config>/projects/<mangled-cwd>/<session-uuid>.jsonl`. This module maps those lines to
//! canonical [`TranscriptEntry`] values and reads them incrementally by byte offset.
//! Spec: `docs/design/sessions.md` §7; `docs/design/v0.2/home-and-surfaces.md` §7.5.
//!
//! The importer never writes to the transcript tree: files are opened read-only, and an
//! unknown line type is counted and skipped.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use vowp_protocol::Timestamp;

use crate::CoreError;
use crate::surface::native::{TranscriptEntry, TranscriptKind};

/// How far one importer pass has read into a transcript file.
///
/// The cursor is serialised as JSON under the node's `watch:<native>` key, so a later
/// pass reads only the suffix after the last complete line.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportCursor {
    /// Byte offset of the first byte not yet read.
    pub offset: u64,
    /// App-side id of the last complete line read, when the line carried one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_uuid: Option<String>,
}

/// One complete transcript line and the entries it produced.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedLine {
    /// The parsed JSON object.
    pub value: Value,
    /// Entries mapped from this line.
    pub entries: Vec<TranscriptEntry>,
}

/// One importer pass.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImportBatch {
    /// Entries mapped from the new complete lines.
    pub entries: Vec<TranscriptEntry>,
    /// Lines that were not JSON objects or had an unknown shape.
    pub skipped_lines: u64,
    /// The parsed lines behind `entries`, in file order.
    ///
    /// The CLI keeps each line's raw JSON next to the events it appends; the mirror only
    /// uses `entries`.
    pub lines: Vec<ImportedLine>,
}

/// Reads complete transcript lines after `cursor.offset`, advances the cursor and maps them.
///
/// A file that shrank below the cursor is read again from the start; the mirror's
/// `native_events` map still makes the repeated entries idempotent.
pub fn import_file(path: &Path, cursor: &mut ImportCursor) -> Result<ImportBatch, CoreError> {
    let mut file = File::open(path).map_err(|source| CoreError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let len = file
        .metadata()
        .map_err(|source| CoreError::Io {
            path: path.display().to_string(),
            source,
        })?
        .len();
    let start = if cursor.offset > len { 0 } else { cursor.offset };
    file.seek(SeekFrom::Start(start)).map_err(|source| CoreError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|source| CoreError::Io {
        path: path.display().to_string(),
        source,
    })?;

    let mut batch = ImportBatch::default();
    let mut position = 0usize;
    while let Some(relative) = bytes[position..].iter().position(|byte| *byte == b'\n') {
        let end = position + relative;
        let line = &bytes[position..end];
        let line_offset = start + position as u64;
        position = end + 1;
        if let Some(mapped) = map_bytes(line, line_offset, &mut batch.skipped_lines) {
            if mapped.uuid.is_some() {
                cursor.last_uuid.clone_from(&mapped.uuid);
            }
            batch.lines.push(ImportedLine {
                value: mapped.value,
                entries: mapped.entries.clone(),
            });
            batch.entries.extend(mapped.entries);
        }
    }
    cursor.offset = start + position as u64;
    Ok(batch)
}

/// Reads and maps a whole transcript file without changing it.
pub fn scan_file(path: &Path) -> Result<Vec<TranscriptEntry>, CoreError> {
    let bytes = std::fs::read(path).map_err(|source| CoreError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let mut entries = Vec::new();
    let mut skipped = 0u64;
    let mut position = 0usize;
    while let Some(relative) = bytes[position..].iter().position(|byte| *byte == b'\n') {
        let end = position + relative;
        let line = &bytes[position..end];
        let line_offset = position as u64;
        position = end + 1;
        if let Some(mapped) = map_bytes(line, line_offset, &mut skipped) {
            entries.extend(mapped.entries);
        }
    }
    if position < bytes.len() {
        let line = &bytes[position..];
        if let Some(mapped) = map_bytes(line, position as u64, &mut skipped) {
            entries.extend(mapped.entries);
        }
    }
    Ok(entries)
}

/// Maps one parsed Claude transcript line to zero or more entries.
///
/// `fallback_id` names the line when the transcript has no `uuid`; callers use the byte
/// offset so repeated scans of the same file produce the same native ids.
pub fn entries_from_value(value: &Value, fallback_id: &str) -> Vec<TranscriptEntry> {
    let Some(kind) = value.get("type").and_then(Value::as_str) else {
        return Vec::new();
    };
    let message = value.get("message");
    let base = value
        .get("uuid")
        .and_then(Value::as_str)
        .or_else(|| message.and_then(|message| message.get("id")).and_then(Value::as_str))
        .unwrap_or(fallback_id)
        .to_owned();
    let native_session = value
        .get("sessionId")
        .and_then(Value::as_str)
        .or_else(|| value.get("session_id").and_then(Value::as_str))
        .map(str::to_owned);
    let model = message
        .and_then(|message| message.get("model"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let ts = timestamp_of(value);
    let mut entries = Vec::new();
    match kind {
        "user" => {
            if let Some(content) = message.and_then(|message| message.get("content")) {
                push_content(
                    content,
                    &base,
                    ts,
                    &model,
                    &native_session,
                    &mut entries,
                    ContentRole::User,
                );
            }
        }
        "assistant" => {
            if let Some(content) = message.and_then(|message| message.get("content")) {
                push_content(
                    content,
                    &base,
                    ts,
                    &model,
                    &native_session,
                    &mut entries,
                    ContentRole::Assistant,
                );
            }
            if let Some(usage) = message.and_then(|message| message.get("usage")) {
                entries.push(cost_entry(format!("{base}#usage"), ts, &model, &native_session, usage));
            }
        }
        _ => {}
    }
    for (index, entry) in entries.iter_mut().enumerate() {
        if entry.native_id.is_empty() {
            entry.native_id = format!("{base}#{index}");
        }
    }
    entries
}

/// Returns the newest transcript for `cwd` under a Claude config root, when one exists.
///
/// Claude Code names a project directory after its working directory with every
/// non-alphanumeric character replaced by `-`.
pub fn find_latest_transcript(config_root: &Path, cwd: &Path) -> Option<PathBuf> {
    let directory = config_root.join("projects").join(mangle_cwd(cwd));
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    let entries = std::fs::read_dir(&directory).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "jsonl") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let modified = metadata.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if newest
            .as_ref()
            .is_none_or(|(best_modified, best_path)| (modified, &path) > (*best_modified, best_path))
        {
            newest = Some((modified, path));
        }
    }
    newest.map(|(_, path)| path)
}

/// Returns the Claude Code project directory name for `cwd`.
pub fn mangle_cwd(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect()
}

/// What a content array belongs to.
#[derive(Debug, Clone, Copy)]
enum ContentRole {
    User,
    Assistant,
}

/// Maps a message `content` value (string or block array) into entries.
#[allow(clippy::too_many_arguments)]
fn push_content(
    content: &Value,
    base: &str,
    ts: Timestamp,
    model: &Option<String>,
    native_session: &Option<String>,
    entries: &mut Vec<TranscriptEntry>,
    role: ContentRole,
) {
    if let Some(text) = content.as_str() {
        if !text.is_empty() {
            entries.push(TranscriptEntry {
                native_id: base.to_owned(),
                kind: match role {
                    ContentRole::User => TranscriptKind::UserMessage,
                    ContentRole::Assistant => TranscriptKind::AssistantMessage,
                },
                ts,
                text: Some(text.to_owned()),
                tool: None,
                input: None,
                output: None,
                path: None,
                call_id: None,
                model: model.clone(),
                native_session: native_session.clone(),
                ok: None,
                tokens: None,
                usd: None,
            });
        }
        return;
    }
    let Some(blocks) = content.as_array() else {
        return;
    };
    for block in blocks {
        let block_type = block.get("type").and_then(Value::as_str).unwrap_or_default();
        match block_type {
            "text" => {
                let Some(text) = block.get("text").and_then(Value::as_str) else {
                    continue;
                };
                if text.is_empty() {
                    continue;
                }
                entries.push(TranscriptEntry {
                    native_id: format!("{base}#{}", entries.len()),
                    kind: match role {
                        ContentRole::User => TranscriptKind::UserMessage,
                        ContentRole::Assistant => TranscriptKind::AssistantMessage,
                    },
                    ts,
                    text: Some(text.to_owned()),
                    tool: None,
                    input: None,
                    output: None,
                    path: None,
                    call_id: None,
                    model: model.clone(),
                    native_session: native_session.clone(),
                    ok: None,
                    tokens: None,
                    usd: None,
                });
            }
            "thinking" => {
                let Some(text) = block.get("thinking").and_then(Value::as_str) else {
                    continue;
                };
                entries.push(TranscriptEntry {
                    native_id: format!("{base}#{}", entries.len()),
                    kind: TranscriptKind::Reasoning,
                    ts,
                    text: Some(text.to_owned()),
                    tool: None,
                    input: None,
                    output: None,
                    path: None,
                    call_id: None,
                    model: model.clone(),
                    native_session: native_session.clone(),
                    ok: None,
                    tokens: None,
                    usd: None,
                });
            }
            "tool_use" => {
                let call_id = block.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
                let input = block.get("input").cloned().unwrap_or(Value::Null);
                entries.push(TranscriptEntry {
                    native_id: format!("{base}#{}", entries.len()),
                    kind: TranscriptKind::ToolCall,
                    ts,
                    text: None,
                    tool: block.get("name").and_then(Value::as_str).map(str::to_owned),
                    path: input
                        .get("file_path")
                        .or_else(|| input.get("path"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    input: Some(input),
                    output: None,
                    call_id: Some(call_id),
                    model: model.clone(),
                    native_session: native_session.clone(),
                    ok: None,
                    tokens: None,
                    usd: None,
                });
            }
            "tool_result" => {
                let call_id = block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let ok = !block.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                entries.push(TranscriptEntry {
                    native_id: format!("{base}#{}", entries.len()),
                    kind: TranscriptKind::ToolResult,
                    ts,
                    text: None,
                    tool: None,
                    input: None,
                    output: text_of(block.get("content")),
                    path: None,
                    call_id: Some(call_id),
                    model: model.clone(),
                    native_session: native_session.clone(),
                    ok: Some(ok),
                    tokens: None,
                    usd: None,
                });
            }
            _ => {}
        }
    }
}

/// Maps one usage object into a cost entry with unknown USD (a subscription session).
fn cost_entry(
    native_id: String,
    ts: Timestamp,
    model: &Option<String>,
    native_session: &Option<String>,
    usage: &Value,
) -> TranscriptEntry {
    let tokens = vowp_protocol::Tokens {
        input: u64_of(usage, &["input_tokens", "inputTokens"]),
        output: u64_of(usage, &["output_tokens", "outputTokens"]),
        reasoning: u64_of(usage, &["reasoning_tokens", "reasoningTokens"]),
        cache_read: u64_of(usage, &["cache_read_input_tokens", "cacheReadInputTokens"]),
        cache_write: u64_of(usage, &["cache_creation_input_tokens", "cacheCreationInputTokens"]),
    };
    TranscriptEntry {
        native_id,
        kind: TranscriptKind::Cost,
        ts,
        text: None,
        tool: None,
        input: None,
        output: None,
        path: None,
        call_id: None,
        model: model.clone(),
        native_session: native_session.clone(),
        ok: None,
        tokens: Some(tokens),
        usd: None,
    }
}

/// One parsed transcript line before it is added to an [`ImportBatch`].
struct MappedLine {
    value: Value,
    entries: Vec<TranscriptEntry>,
    uuid: Option<String>,
}

/// Returns the app-side id of one transcript line, when the line carries one.
///
/// Claude Code writes `uuid` on most lines; assistant lines may only carry
/// `message.id`. The importer uses this value as the cursor's last-complete-line mark.
pub fn line_uuid(value: &Value) -> Option<String> {
    value
        .get("uuid")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            value
                .get("message")
                .and_then(|message| message.get("id"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
}

/// Parses one UTF-8 line and maps it; returns the parsed value, entries and line uuid.
fn map_bytes(line: &[u8], line_offset: u64, skipped: &mut u64) -> Option<MappedLine> {
    let Ok(text) = std::str::from_utf8(line) else {
        *skipped += 1;
        return None;
    };
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        *skipped += 1;
        return None;
    };
    if !value.is_object() {
        *skipped += 1;
        return None;
    }
    let uuid = line_uuid(&value);
    let fallback = format!("line-{line_offset}");
    let entries = entries_from_value(&value, &fallback);
    if entries.is_empty() {
        *skipped += 1;
        return None;
    }
    Some(MappedLine { value, entries, uuid })
}

/// Reads a timestamp from the line, accepting RFC 3339 text and Unix milliseconds.
fn timestamp_of(value: &Value) -> Timestamp {
    if let Some(text) = value.get("timestamp").and_then(Value::as_str) {
        if let Ok(ts) = Timestamp::parse(text) {
            return ts;
        }
    }
    if let Some(millis) = value.get("timestamp").and_then(Value::as_i64) {
        return Timestamp::from_unix_millis(millis);
    }
    Timestamp::now()
}

/// Joins a string-or-block-array content value into one text.
fn text_of(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => {
            let parts: Vec<String> = blocks
                .iter()
                .filter_map(|block| {
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .or_else(|| block.as_str())
                        .map(str::to_owned)
                })
                .collect();
            if parts.is_empty() { None } else { Some(parts.join("\n")) }
        }
        other => Some(other.to_string()),
    }
}

/// Reads the first present unsigned field from `value`.
fn u64_of(value: &Value, names: &[&str]) -> u64 {
    for name in names {
        if let Some(number) = value.get(*name).and_then(Value::as_u64) {
            return number;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use serde_json::json;

    use super::*;

    /// A transcript line the importer maps to one user message.
    fn user_line(uuid: &str, text: &str) -> serde_json::Value {
        json!({
            "type": "user",
            "uuid": uuid,
            "sessionId": "claude-session",
            "message": {"role": "user", "content": text},
        })
    }

    #[test]
    fn import_file_reads_only_complete_lines() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("session.jsonl");
        let first = serde_json::to_string(&user_line("u1", "hello")).expect("line");
        std::fs::write(&path, format!("{first}\n")).expect("write first line");
        let after_first = std::fs::metadata(&path).expect("metadata").len();

        let mut cursor = ImportCursor::default();
        let batch = import_file(&path, &mut cursor).expect("first import");
        assert_eq!(batch.entries.len(), 1);
        assert_eq!(batch.entries[0].text.as_deref(), Some("hello"));
        assert_eq!(batch.lines.len(), 1);
        assert_eq!(batch.lines[0].value["uuid"], "u1");
        assert_eq!(cursor.offset, after_first);
        assert_eq!(cursor.last_uuid.as_deref(), Some("u1"));

        // A final line without its newline is not consumed, even when it is valid JSON.
        let second = serde_json::to_string(&user_line("u2", "not yet")).expect("line");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open transcript");
        file.write_all(second.as_bytes()).expect("write partial line");
        file.flush().expect("flush");
        drop(file);
        let batch = import_file(&path, &mut cursor).expect("partial import");
        assert!(batch.entries.is_empty(), "a partial line was imported");
        assert_eq!(cursor.offset, after_first);
        assert_eq!(cursor.last_uuid.as_deref(), Some("u1"));

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open transcript");
        file.write_all(b"\n").expect("complete the line");
        file.flush().expect("flush");
        drop(file);
        let batch = import_file(&path, &mut cursor).expect("completed import");
        assert_eq!(batch.entries.len(), 1);
        assert_eq!(batch.entries[0].text.as_deref(), Some("not yet"));
        assert_eq!(cursor.last_uuid.as_deref(), Some("u2"));
    }

    #[test]
    fn import_file_restarts_when_the_file_shrinks() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("session.jsonl");
        let first = serde_json::to_string(&user_line("u1", "one")).expect("line");
        let second = serde_json::to_string(&user_line("u2", "two")).expect("line");
        std::fs::write(&path, format!("{first}\n{second}\n")).expect("write transcript");

        let mut cursor = ImportCursor::default();
        let batch = import_file(&path, &mut cursor).expect("first import");
        assert_eq!(batch.entries.len(), 2);

        let shorter = serde_json::to_string(&user_line("u3", "restart")).expect("line");
        std::fs::write(&path, format!("{shorter}\n")).expect("truncate transcript");
        let batch = import_file(&path, &mut cursor).expect("import after truncation");
        assert_eq!(batch.entries.len(), 1, "a shorter file must restart from zero");
        assert_eq!(batch.entries[0].text.as_deref(), Some("restart"));
        assert_eq!(cursor.last_uuid.as_deref(), Some("u3"));
    }

    #[test]
    fn import_cursor_round_trips_through_json() {
        let cursor = ImportCursor {
            offset: 42,
            last_uuid: Some("uuid-42".to_owned()),
        };
        let text = serde_json::to_string(&cursor).expect("serialise cursor");
        assert_eq!(
            serde_json::from_str::<ImportCursor>(&text).expect("parse cursor"),
            cursor
        );

        let empty = ImportCursor::default();
        let text = serde_json::to_string(&empty).expect("serialise empty cursor");
        assert_eq!(
            serde_json::from_str::<ImportCursor>(&text).expect("parse cursor"),
            empty
        );
    }

    #[test]
    fn line_uuid_accepts_message_ids() {
        let value = json!({
            "type": "assistant",
            "message": {"id": "msg_1", "content": []},
        });
        assert_eq!(line_uuid(&value).as_deref(), Some("msg_1"));
        assert!(line_uuid(&json!({"type": "summary"})).is_none());
    }
}
