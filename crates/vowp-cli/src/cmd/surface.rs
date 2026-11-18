//! `surface`: create, list, rotate and revoke the home side of a paired local surface.
//!
//! Spec: `docs/design/v0.4/local-orchestrator.md` §4.3, §4.6 and §12.1. The commands are
//! home-only and use the global store. `surface pair` creates a `pending` surface and prints
//! a ten-minute one-time code exactly once; `surface ls` reports state, scopes and expiry but
//! never a token; `surface rotate` issues a replacement token, keeps the old token for a
//! ten-minute overlap and queues `SurfaceIssued` for the surface's node; `surface revoke`
//! revokes every token and marks the surface revoked.
//!
//! The v4 store read model returns one surface by id. This module keeps the ids it has
//! created in `<home>/surfaces.json` (surface ids are not secrets) and re-reads the surface,
//! its state, its scopes and its expiry from the store for every listing.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;
use vowp_core::agent::pair::{
    RotationPolicy, SurfaceListRow, create_pending_surface, replace_pending_surface, revoke_surface,
    rotate_surface_with_policy, surface_list_row,
};
use vowp_protocol::{
    NodeName, NodePresence, SessionId, SurfaceId, SurfaceInfo, SurfaceKind, SurfaceScope, SurfaceState, Timestamp,
};
use vowp_store::{Store, SurfaceToken};

use crate::cli::{SurfaceCommand, SurfaceLsArgs, SurfacePairArgs, SurfaceRevokeArgs, SurfaceRotateArgs};
use crate::node_ctx::NodeRole;
use crate::output::{Table, local_time_at, print_json};
use crate::{CliError, Ctx, IoStreams};

/// Longest warning window for a surface token before it expires.
const EXPIRY_WARNING: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Private file holding the surface ids this home CLI created.
const SURFACE_INDEX_FILE: &str = "surfaces.json";

/// Runs one surface subcommand. `node` is the global `--node <name>` value: for these
/// commands it names the surface's node, not an SSH redirect.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, command: &SurfaceCommand, node: Option<&str>) -> Result<i32, CliError> {
    match command {
        SurfaceCommand::Pair(args) => pair(ctx, io, args, node),
        SurfaceCommand::Ls(args) => list(ctx, io, args, node),
        SurfaceCommand::Revoke(args) => revoke(ctx, io, args),
        SurfaceCommand::Rotate(args) => rotate(ctx, io, args, node),
    }
}

fn pair(ctx: &Ctx, io: &mut dyn IoStreams, args: &SurfacePairArgs, node: Option<&str>) -> Result<i32, CliError> {
    require_home(ctx)?;
    let node = node
        .ok_or_else(|| CliError::Invalid("surface pair needs --node <name> to name the surface's node".to_owned()))?;
    let node = NodeName::parse(node).map_err(|_| CliError::Invalid(format!("invalid node name {node:?}")))?;
    let session = SessionId::parse(&args.session)
        .map_err(|_| CliError::Invalid(format!("invalid session id {:?}", args.session)))?;
    let kind = parse_kind(&args.kind)?;
    let replace = args.replace.as_deref().map(parse_surface_id).transpose()?;
    let store = ctx.fleet_store()?;
    let now = ctx.clock().now();
    let pending = match &replace {
        Some(old) => replace_pending_surface(&store, ctx.deps(), old, &node, &session, kind, args.app.as_deref(), now)?,
        None => {
            if let Some(existing) = pending_duplicate(&store, ctx, &node, &session, kind, args.app.as_deref())? {
                return Err(CliError::Conflict(format!(
                    "surface {} is already pending for session {session}; revoke it before pairing again",
                    existing.id
                )));
            }
            create_pending_surface(&store, &node, &session, kind, args.app.as_deref(), now)?
        }
    };
    append_index(ctx, &pending.surface.id)?;
    if let Some(old) = &replace {
        append_index(ctx, old)?;
    }

    let command = agent_pair_command(&pending.surface.id, &session, args.app.as_deref());
    let output = SurfacePairOutput {
        surface: pending.surface.id.clone(),
        node: node.clone(),
        session: session.clone(),
        kind,
        app: args.app.clone(),
        code: pending.code.clone(),
        expires_at: pending.expires_at,
        command: command.clone(),
        replaced: replace.clone(),
    };
    if ctx.json() {
        print_json(io, &output)?;
    } else {
        if let Some(old) = &replace {
            writeln!(io.out(), "revoked surface {old} and created its replacement")?;
        }
        writeln!(
            io.out(),
            "created pending surface {} on {node} ({})",
            pending.surface.id,
            args.kind
        )?;
        writeln!(io.out(), "session: {session}")?;
        writeln!(io.out(), "code: {}", pending.code)?;
        writeln!(io.out(), "expires: {}", pending.expires_at.as_rfc3339())?;
        writeln!(io.out(), "run on the Mac:")?;
        writeln!(io.out(), "  {command}")?;
    }
    Ok(0)
}

fn list(ctx: &Ctx, io: &mut dyn IoStreams, _args: &SurfaceLsArgs, node: Option<&str>) -> Result<i32, CliError> {
    require_home(ctx)?;
    let filter = match node {
        Some(value) => {
            Some(NodeName::parse(value).map_err(|_| CliError::Invalid(format!("invalid node name {value:?}")))?)
        }
        None => None,
    };
    let store = ctx.fleet_store()?;
    let now = ctx.clock().now();
    let mut entries = Vec::new();
    for id in read_index(ctx)? {
        let Some(row) = surface_list_row(&store, ctx.deps(), &id, now)? else {
            continue;
        };
        if filter.as_ref().is_some_and(|node| row.surface.node != *node) {
            continue;
        }
        let tokens = store.list_surface_tokens(&id)?;
        entries.push(SurfaceEntry::from_row(row, &tokens, now));
    }
    entries.sort_by(|left, right| left.id.0.cmp(&right.id.0));

    if ctx.json() {
        print_json(io, &entries)?;
        return Ok(0);
    }
    let mut table = Table::new(&["ID", "NODE", "KIND", "STATE", "SCOPES", "PREFIX", "EXPIRES", "OVERLAP"]);
    for entry in &entries {
        table.row(vec![
            entry.id.to_string(),
            entry.node.to_string(),
            wire_name(&entry.kind),
            wire_name(&entry.state),
            join_or_dash(&entry.scopes),
            entry.prefix.clone().unwrap_or_else(|| "-".to_owned()),
            entry
                .expires_at
                .map(|expiry| {
                    let marker = if entry.expired { " expired" } else { "" };
                    format!("{}{marker}", local_time_at(expiry, now, ctx.tz_offset_minutes()))
                })
                .unwrap_or_else(|| "-".to_owned()),
            overlap_cell(entry, now, ctx.tz_offset_minutes()),
        ]);
    }
    writeln!(io.out(), "{}", table.render(ctx.color()))?;
    warn_expiring_surface_tokens(io, &entries, now)?;
    Ok(0)
}

/// Returns the `OVERLAP` cell for one surface row.
///
/// An undelivered rotation shows `PENDING` plus the replacement token's delivery deadline.
/// Otherwise an older token that is still valid during the overlap shows its deadline.
fn overlap_cell(entry: &SurfaceEntry, now: Timestamp, tz_offset_minutes: i32) -> String {
    if entry.pending {
        let deadline = entry
            .delivery_deadline
            .map(|at| format!(" until {}", local_time_at(at, now, tz_offset_minutes)))
            .unwrap_or_default();
        return format!("PENDING{deadline}");
    }
    match entry.overlap_until {
        Some(until) => format!("until {}", local_time_at(until, now, tz_offset_minutes)),
        None => "-".to_owned(),
    }
}

/// Prints one warning line when a surface token expires within seven days.
fn warn_expiring_surface_tokens(
    io: &mut dyn IoStreams,
    entries: &[SurfaceEntry],
    now: Timestamp,
) -> Result<(), CliError> {
    let expiring: Vec<&SurfaceEntry> = entries
        .iter()
        .filter(|entry| {
            entry
                .expires_at
                .is_some_and(|expiry| expiry > now && expiry.since(now) <= EXPIRY_WARNING)
        })
        .collect();
    if expiring.is_empty() {
        return Ok(());
    }
    let expiry = expiring
        .iter()
        .filter_map(|entry| entry.expires_at)
        .min()
        .map(|expiry| expiry.as_rfc3339())
        .unwrap_or_else(|| "-".to_owned());
    writeln!(
        io.err(),
        "warning: {} surface token(s) expire within 7 days (earliest {expiry}); rotate with `vowp surface rotate <id>`",
        expiring.len()
    )?;
    Ok(())
}

fn revoke(ctx: &Ctx, io: &mut dyn IoStreams, args: &SurfaceRevokeArgs) -> Result<i32, CliError> {
    require_home(ctx)?;
    let id = parse_surface_id(&args.id)?;
    if !args.yes {
        return Err(CliError::Guard(format!(
            "refusing to revoke surface {id} without --yes"
        )));
    }
    let store = ctx.fleet_store()?;
    let revoked = revoke_surface(&store, ctx.deps(), &id, ctx.clock().now())?;
    append_index(ctx, &id)?;
    if ctx.json() {
        print_json(
            io,
            &SurfaceRevokeOutput {
                surface: id.clone(),
                status: "revoked".to_owned(),
                tokens_revoked: revoked,
            },
        )?;
    } else {
        writeln!(io.out(), "revoked surface {id}; {revoked} token(s) revoked")?;
    }
    Ok(0)
}

/// Rotates one surface token and queues its replacement for the surface's node.
///
/// The command never prints the token. A node that is not online receives the queued frame
/// on its next authenticated connection; the command still succeeds and warns on stderr.
fn rotate(ctx: &Ctx, io: &mut dyn IoStreams, args: &SurfaceRotateArgs, node: Option<&str>) -> Result<i32, CliError> {
    require_home(ctx)?;
    let id = parse_surface_id(&args.id)?;
    let filter = match node {
        Some(value) => {
            Some(NodeName::parse(value).map_err(|_| CliError::Invalid(format!("invalid node name {value:?}")))?)
        }
        None => None,
    };
    let grace = match args.grace.as_deref() {
        Some(value) => parse_duration_arg(value, "--grace")?,
        None => RotationPolicy::default().grace,
    };
    let wait = args
        .wait
        .as_deref()
        .map(|value| parse_duration_arg(value, "--wait"))
        .transpose()?;
    let store = ctx.fleet_store()?;
    let existing = store.surface_by_id(&id)?;
    if let Some(surface) = &existing {
        if let Some(filter) = &filter
            && surface.node != *filter
        {
            return Err(CliError::Conflict(format!(
                "surface {id} belongs to node {}, not {filter}",
                surface.node
            )));
        }
        if surface.state == SurfaceState::Pending {
            return Err(CliError::Conflict(format!(
                "surface {id} is pending; pair it before rotating its token"
            )));
        }
    }

    let now = ctx.clock().now();
    let rotation = rotate_surface_with_policy(&store, ctx.deps(), &id, RotationPolicy { grace, wait }, now)?;
    let rotated = &rotation.rotated;
    append_index(ctx, &id)?;

    if let Some(wait) = wait
        && !rotation.delivered
    {
        return Err(CliError::Remote(format!(
            "rotation_pending: the rotation was queued for node {} but its frame was not drained within {}; \
             run `vowp surface ls` to see the delivery deadline",
            rotated.node,
            format_duration(wait)
        )));
    }
    let offline = node_is_offline(&store, &rotated.node, now)?;
    let output = SurfaceRotateOutput {
        surface: rotated.surface.id.clone(),
        node: rotated.node.clone(),
        prefix: rotated.prefix.clone(),
        expires_at: rotated.expires_at,
        old_expires_at: rotated.old_expires_at,
        pending: !rotation.delivered,
        queued_at: Some(rotation.queued_at),
    };
    if ctx.json() {
        print_json(io, &output)?;
    } else {
        writeln!(io.out(), "rotated surface {} on {}", output.surface, output.node)?;
        writeln!(io.out(), "prefix: {}", output.prefix)?;
        writeln!(io.out(), "expires: {}", output.expires_at.as_rfc3339())?;
        writeln!(io.out(), "old token expires: {}", output.old_expires_at.as_rfc3339())?;
        if output.pending {
            writeln!(io.out(), "delivery: pending")?;
        }
    }
    if offline && !rotation.delivered {
        writeln!(
            io.err(),
            "warning: node {} is offline; the agent must re-pair before {}",
            rotated.node,
            rotated.old_expires_at.as_rfc3339()
        )?;
    }
    Ok(0)
}

/// Parses a `--wait` or `--grace` duration such as `30s`, `10m`, `24h` or `7d`.
fn parse_duration_arg(value: &str, flag: &str) -> Result<Duration, CliError> {
    let text = value.trim();
    let (digits, multiplier) = if let Some(digits) = text.strip_suffix('s') {
        (digits, 1_u64)
    } else if let Some(digits) = text.strip_suffix('m') {
        (digits, 60_u64)
    } else if let Some(digits) = text.strip_suffix('h') {
        (digits, 3_600_u64)
    } else if let Some(digits) = text.strip_suffix('d') {
        (digits, 86_400_u64)
    } else if let Some(digits) = text.strip_suffix('w') {
        (digits, 604_800_u64)
    } else {
        return Err(CliError::Invalid(format!(
            "invalid {flag} {value:?}; use a duration such as 30m, 10m, 24h or 7d"
        )));
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(CliError::Invalid(format!(
            "invalid {flag} {value:?}; use a duration such as 30m, 10m, 24h or 7d"
        )));
    }
    let count: u64 = digits
        .parse()
        .map_err(|_| CliError::Invalid(format!("invalid {flag} {value:?}")))?;
    let seconds = count
        .checked_mul(multiplier)
        .ok_or_else(|| CliError::Invalid(format!("{flag} {value:?} is too long")))?;
    Ok(Duration::from_secs(seconds))
}

/// Formats a duration for a refusal message.
fn format_duration(duration: Duration) -> String {
    if duration.as_secs() % 3_600 == 0 {
        format!("{}h", duration.as_secs() / 3_600)
    } else if duration.as_secs() % 60 == 0 {
        format!("{}m", duration.as_secs() / 60)
    } else {
        format!("{}s", duration.as_secs())
    }
}

/// Returns whether a surface's node is not currently online.
///
/// A missing card is treated as offline: the queued frame still waits for the next
/// authenticated connection, but the owner should hear that delivery may be delayed.
fn node_is_offline(store: &Store, node: &NodeName, now: Timestamp) -> Result<bool, CliError> {
    Ok(store
        .node_card(node, now)?
        .is_none_or(|card| card.presence != NodePresence::Online))
}

/// Returns the exact Mac command for one pending surface. The code is never part of it.
fn agent_pair_command(surface: &SurfaceId, session: &SessionId, app: Option<&str>) -> String {
    let mut command = format!("vowp agent pair --code-file - --surface {surface} --session {session}");
    if let Some(app) = app {
        command.push_str(&format!(" --app {app}"));
    }
    command
}

fn parse_kind(value: &str) -> Result<SurfaceKind, CliError> {
    match value {
        "opencode" => Ok(SurfaceKind::Opencode),
        "chat" => Ok(SurfaceKind::Chat),
        other => Err(CliError::Invalid(format!(
            "invalid surface kind {other:?}; use opencode or chat"
        ))),
    }
}

fn parse_surface_id(value: &str) -> Result<SurfaceId, CliError> {
    if value.len() == 26 && value.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        Ok(SurfaceId(value.to_owned()))
    } else {
        Err(CliError::Invalid(format!(
            "invalid surface id {value:?}; expected a 26-character ULID"
        )))
    }
}

fn require_home(ctx: &Ctx) -> Result<(), CliError> {
    match ctx.role() {
        NodeRole::Home => Ok(()),
        NodeRole::Control => Err(CliError::Guard(
            "surface commands need the home store; run them on the home machine".to_owned(),
        )),
        NodeRole::Uninitialised => Err(CliError::NotFound(
            "vowp home is not initialised; run `vowp setup` or `vowp init`".to_owned(),
        )),
    }
}

fn index_path(ctx: &Ctx) -> PathBuf {
    ctx.paths().root().join(SURFACE_INDEX_FILE)
}

fn read_index(ctx: &Ctx) -> Result<Vec<SurfaceId>, CliError> {
    let path = index_path(ctx);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(CliError::Io(error)),
    };
    serde_json::from_str(&text).map_err(|error| CliError::Protocol(vowp_protocol::ProtocolError::from(error)))
}

fn write_index(ctx: &Ctx, ids: &[SurfaceId]) -> Result<(), CliError> {
    let path = index_path(ctx);
    let bytes = serde_json::to_vec_pretty(ids).map_err(vowp_protocol::ProtocolError::from)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Returns an existing pending surface with the same node, session, kind and app.
///
/// A repeated `surface pair` for one pending surface would create a second code the owner
/// did not ask for; it is a conflict (exit 4). Revoking the pending surface is the documented
/// way to start over.
fn pending_duplicate(
    store: &Store,
    ctx: &Ctx,
    node: &NodeName,
    session: &SessionId,
    kind: SurfaceKind,
    app: Option<&str>,
) -> Result<Option<SurfaceInfo>, CliError> {
    for id in read_index(ctx)? {
        let Some(surface) = store.surface_by_id(&id)? else {
            continue;
        };
        if surface.state != SurfaceState::Pending
            || surface.node != *node
            || surface.kind != kind
            || surface.app.as_deref() != app
        {
            continue;
        }
        if surface.metadata.get("session").and_then(serde_json::Value::as_str) == Some(session.as_str()) {
            return Ok(Some(surface));
        }
    }
    Ok(None)
}

fn append_index(ctx: &Ctx, id: &SurfaceId) -> Result<(), CliError> {
    let mut ids = read_index(ctx)?;
    if !ids.contains(id) {
        ids.push(id.clone());
        write_index(ctx, &ids)?;
    }
    Ok(())
}

fn join_or_dash(scopes: &[SurfaceScope]) -> String {
    if scopes.is_empty() {
        "-".to_owned()
    } else {
        scopes.iter().map(ToString::to_string).collect::<Vec<_>>().join(",")
    }
}

fn wire_name<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "-".to_owned())
}

/// One `surface ls` row: the read model plus its token prefix, expiries and pending rotation.
#[derive(Debug, Serialize)]
struct SurfaceEntry {
    id: SurfaceId,
    node: NodeName,
    kind: SurfaceKind,
    state: SurfaceState,
    scopes: Vec<SurfaceScope>,
    /// Display prefix of the newest active token; never the token.
    prefix: Option<String>,
    /// Expiry of the newest stored (unrevoked) token, even when it has already passed.
    expires_at: Option<Timestamp>,
    /// Latest expiry among older active tokens while a rotation overlaps.
    overlap_until: Option<Timestamp>,
    /// Whether the stored expiry has already passed while the surface stays attached.
    expired: bool,
    /// Whether a rotation frame is still queued for the node.
    pending: bool,
    /// When the queued rotation was written.
    #[serde(skip_serializing_if = "Option::is_none")]
    queued_at: Option<Timestamp>,
    /// When the queued replacement token expires (the delivery deadline).
    #[serde(skip_serializing_if = "Option::is_none")]
    delivery_deadline: Option<Timestamp>,
}

impl SurfaceEntry {
    /// Builds one row from the merged core read model and the surface's stored tokens.
    fn from_row(row: SurfaceListRow, tokens: &[SurfaceToken], now: Timestamp) -> Self {
        let latest_expiry = tokens
            .iter()
            .filter(|token| token.revoked_at.is_none())
            .filter_map(|token| token.expires_at)
            .max();
        let expired = row.prefix.is_none() && latest_expiry.is_some_and(|expiry| expiry <= now);
        Self {
            id: row.surface.id,
            node: row.surface.node,
            kind: row.surface.kind,
            state: row.surface.state,
            scopes: row.scopes,
            prefix: row.prefix,
            expires_at: latest_expiry,
            overlap_until: row.overlap_until,
            expired,
            pending: row.pending,
            queued_at: row.queued_at,
            delivery_deadline: row.delivery_deadline,
        }
    }
}

/// JSON shape of `vowp surface pair`.
#[derive(Debug, Serialize)]
struct SurfacePairOutput {
    surface: SurfaceId,
    node: NodeName,
    session: SessionId,
    kind: SurfaceKind,
    app: Option<String>,
    code: String,
    expires_at: Timestamp,
    command: String,
    /// The surface revoked by `--replace`, when one was named.
    #[serde(skip_serializing_if = "Option::is_none")]
    replaced: Option<SurfaceId>,
}

/// JSON shape of `vowp surface revoke`.
#[derive(Debug, Serialize)]
struct SurfaceRevokeOutput {
    surface: SurfaceId,
    status: String,
    tokens_revoked: u64,
}

/// JSON shape of `vowp surface rotate`.
///
/// The token itself is deliberately absent: only its display prefix and the two expiries
/// are public.
#[derive(Debug, Serialize)]
struct SurfaceRotateOutput {
    surface: SurfaceId,
    node: NodeName,
    prefix: String,
    expires_at: Timestamp,
    old_expires_at: Timestamp,
    /// Whether the replacement frame is still waiting for the node.
    pending: bool,
    /// When the replacement frame was queued.
    queued_at: Option<Timestamp>,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use vowp_core::{AdapterRegistry, CommandRunner, Deps, FixedRng, SystemGit, SystemRunner};
    use vowp_protocol::{
        AgentFrame, MemoryLog, NodeStatus, ProjectName, Session, SessionRole, SessionState, SystemClock, VowpConfig,
    };
    use vowp_store::NodePaths;
    use vowp_tmux::fake::FakeWindows;

    use super::*;
    use crate::BufferIo;

    /// One isolated home with an active node, one session and one attached surface.
    struct RotateEnv {
        home: PathBuf,
        project: PathBuf,
        node: NodeName,
        session: SessionId,
        surface: SurfaceId,
        old_token: String,
        _home_dir: tempfile::TempDir,
        _project_dir: tempfile::TempDir,
    }

    fn rotate_env() -> RotateEnv {
        let home = tempfile::tempdir().expect("temp home");
        let project = tempfile::tempdir().expect("temp project");
        let config = vowp_protocol::VowpConfig::with_defaults_for_tests();
        std::fs::write(
            project.path().join("vowp.toml"),
            toml::to_string_pretty(&config).expect("config toml"),
        )
        .expect("vowp.toml");
        let paths = NodePaths::new(home.path());
        let store = Store::open(&paths).expect("store");
        let now = Timestamp::now();
        let node = NodeName::parse("mac-studio").expect("node");
        store.insert_pending_node(&node, &[], now).expect("node");
        store
            .set_node_status(&node, NodeStatus::Active, "", now.minus(Duration::from_secs(120)))
            .expect("active node");
        let session = SessionId::new();
        store
            .upsert_session(&Session {
                id: session.clone(),
                project: Some(ProjectName::parse("vowp").expect("project")),
                job_id: None,
                role: Some(SessionRole::User),
                title: "surface rotation".to_owned(),
                parent_session_id: None,
                node: node.clone(),
                cwd: "/tmp/vowp".to_owned(),
                state: SessionState::Idle,
                executor: None,
                last_seq: 0,
                last_capsule_id: None,
                log_bytes: 0,
                created_at: now,
                updated_at: now,
            })
            .expect("session");
        let pending = create_pending_surface(&store, &node, &session, SurfaceKind::Opencode, Some("opencode"), now)
            .expect("pending surface");
        let issued = vowp_core::agent::pair::issue_surface_token(
            &store,
            &node,
            &pending.surface.id,
            &session,
            "opencode",
            &pending.code,
            now,
        )
        .expect("issued token");
        RotateEnv {
            home: home.path().to_path_buf(),
            project: project.path().to_path_buf(),
            node,
            session,
            surface: pending.surface.id,
            old_token: issued.token,
            _home_dir: home,
            _project_dir: project,
        }
    }

    /// Builds the injected dependencies for the in-file test call sites.
    fn test_deps(env: &RotateEnv) -> Deps {
        let cmd: Arc<dyn CommandRunner> = Arc::new(SystemRunner);
        Deps {
            paths: NodePaths::new(&env.home),
            config: Arc::new(VowpConfig::with_defaults_for_tests()),
            node: NodeName::parse("home-1").expect("home node"),
            clock: Arc::new(SystemClock),
            log: Arc::new(MemoryLog::new()),
            adapters: Arc::new(AdapterRegistry::new()),
            windows: Arc::new(FakeWindows::new()),
            git: Arc::new(SystemGit::new(Arc::clone(&cmd))),
            cmd,
            rng: Arc::new(FixedRng(0)),
        }
    }

    fn run_rotate(env: &RotateEnv, json: bool) -> (i32, String, String) {
        let mut args = vec!["vowp".to_owned()];
        if json {
            args.push("--json".to_owned());
        }
        args.extend([
            "--home".to_owned(),
            env.home.display().to_string(),
            "--project".to_owned(),
            env.project.display().to_string(),
            "surface".to_owned(),
            "rotate".to_owned(),
            env.surface.0.clone(),
        ]);
        let mut io = BufferIo::new("");
        let code = crate::run(args, &mut io);
        (code, io.stdout(), io.stderr())
    }

    fn queued_surface_issued(env: &RotateEnv) -> (String, Timestamp) {
        let path = env
            .home
            .join("agent-frames")
            .join(env.node.as_str())
            .join(format!("{}.json", env.surface.0));
        let file_mode = fs::metadata(&path).expect("queued frame").permissions().mode() & 0o777;
        assert_eq!(
            file_mode, 0o600,
            "the queued frame contains a token and must be private"
        );
        let dir_mode = fs::metadata(path.parent().expect("frame dir"))
            .expect("frame dir")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "the frame directory must be private");

        let bytes = fs::read(&path).expect("queued frame bytes");
        let envelope: serde_json::Value = serde_json::from_slice(&bytes).expect("queued frame envelope");
        assert!(envelope.get("queued_at").is_some(), "the envelope carries queued_at");
        assert!(
            envelope.get("overlap_until").is_some(),
            "the envelope carries the overlap deadline"
        );
        assert!(envelope.get("frame").is_some(), "the frame is inside the envelope");
        assert!(
            envelope.get("token").is_none(),
            "the token is inside the frame, not at the envelope top level"
        );

        let spool = vowp_core::agent::AgentFrameSpool::new(&env.home);
        let queued = spool
            .read(&env.node, &env.surface.0)
            .expect("read queued frame")
            .expect("queued frame");
        assert!(queued.queued_at.is_some(), "the rotation records its queue time");
        assert!(
            queued.overlap_until.is_some(),
            "the rotation records the old-token deadline"
        );
        let delivery_deadline = queued.delivery_deadline();
        match queued.frame {
            AgentFrame::SurfaceIssued {
                surface_id,
                token,
                scopes,
                expires_at,
            } => {
                assert_eq!(surface_id, env.surface);
                let read_scope = format!("session.read:{}", env.session);
                assert!(scopes.iter().any(|scope| scope.as_str() == read_scope));
                assert!(
                    token.starts_with("vowp_surf_"),
                    "the queued frame carries the new token"
                );
                assert_eq!(
                    delivery_deadline,
                    Some(expires_at),
                    "the replacement expiry is the delivery deadline"
                );
                (token, expires_at)
            }
            other => panic!("expected SurfaceIssued, got {other:?}"),
        }
    }

    #[test]
    fn surface_rotate_queues_for_an_offline_node_and_warns() {
        let env = rotate_env();
        let before = Timestamp::now();
        let (code, stdout, stderr) = run_rotate(&env, false);
        let after = Timestamp::now();
        assert_eq!(code, 0, "an offline node still rotates: {stderr}");
        assert!(stdout.contains("rotated surface"), "{stdout}");
        let warning = format!("warning: node {} is offline; the agent must re-pair before", env.node);
        assert!(stderr.contains(&warning), "the warning is required: {stderr}");

        let (token, new_expiry) = queued_surface_issued(&env);
        assert!(!stdout.contains(&token), "the new token is never printed: {stdout}");
        assert!(!stderr.contains(&token), "the new token is never printed: {stderr}");
        assert!(!stdout.contains("vowp_surf_"), "{stdout}");

        let store = Store::open(&NodePaths::new(&env.home)).expect("store");
        let new_hash = vowp_core::agent::hash_code(&token);
        let new_row = store
            .surface_token_by_hash(&new_hash)
            .expect("hash lookup")
            .expect("new token row");
        assert_eq!(new_row.expires_at, Some(new_expiry));
        assert_eq!(new_row.surface_id, env.surface);

        let tokens = store.list_surface_tokens(&env.surface).expect("tokens");
        let old = tokens
            .iter()
            .find(|row| row.token_hash == vowp_core::agent::hash_code(&env.old_token))
            .expect("old token row");
        let old_expiry = old.expires_at.expect("old token keeps an expiry");
        assert!(
            old_expiry.since(before) <= Duration::from_secs(10 * 60) + Duration::from_secs(2)
                && old_expiry.since(after) >= Duration::from_secs(9 * 60),
            "the old token expires after a ten-minute overlap: {}",
            old_expiry.as_rfc3339()
        );
        assert!(
            stderr.contains(&old_expiry.as_rfc3339()),
            "the warning names the old-token expiry: {stderr}"
        );

        // The frame is still queued: no gateway connection drained it.
        let path = env
            .home
            .join("agent-frames")
            .join(env.node.as_str())
            .join(format!("{}.json", env.surface.0));
        assert!(path.is_file(), "a rotation for an offline node stays queued");
    }

    #[test]
    fn surface_rotate_never_prints_the_token() {
        let env = rotate_env();
        let (code, stdout, stderr) = run_rotate(&env, false);
        assert_eq!(code, 0, "surface rotate failed: {stderr}");
        let (first_token, _) = queued_surface_issued(&env);
        for (label, text) in [("stdout", &stdout), ("stderr", &stderr)] {
            assert!(!text.contains(&first_token), "{label} must not carry the token: {text}");
            assert!(!text.contains("vowp_surf_"), "{label} must not carry a token: {text}");
            assert!(
                !text.contains(&env.old_token),
                "{label} must not carry the old token: {text}"
            );
        }
        assert!(stdout.contains("prefix:"), "{stdout}");
        assert!(stdout.contains("old token expires:"), "{stdout}");

        let (code, stdout, stderr) = run_rotate(&env, true);
        assert_eq!(code, 0, "surface rotate --json failed: {stderr}");
        let (second_token, new_expiry) = queued_surface_issued(&env);
        let value: serde_json::Value = serde_json::from_str(&stdout).expect("rotate JSON");
        assert_eq!(value["surface"], env.surface.0);
        assert_eq!(value["node"], env.node.as_str());
        assert_eq!(value["expires_at"], new_expiry.as_rfc3339());
        assert!(value["prefix"].is_string());
        assert!(value.get("token").is_none(), "JSON has no token field: {stdout}");
        for (label, text) in [("stdout", &stdout), ("stderr", &stderr)] {
            assert!(
                !text.contains(&second_token),
                "{label} must not carry the token: {text}"
            );
            assert!(!text.contains("vowp_surf_"), "{label} must not carry a token: {text}");
        }
    }

    #[test]
    fn surface_rotate_uses_the_existing_exit_codes() {
        let env = rotate_env();
        let store = Store::open(&NodePaths::new(&env.home)).expect("store");
        let now = Timestamp::now();

        let pending = create_pending_surface(
            &store,
            &env.node,
            &env.session,
            SurfaceKind::Opencode,
            Some("opencode"),
            now,
        )
        .expect("pending surface");
        let mut args = vec![
            "vowp".to_owned(),
            "--home".to_owned(),
            env.home.display().to_string(),
            "--project".to_owned(),
            env.project.display().to_string(),
            "surface".to_owned(),
            "rotate".to_owned(),
            pending.surface.id.0.clone(),
        ];
        let mut io = BufferIo::new("");
        assert_eq!(
            crate::run(args.clone(), &mut io),
            4,
            "a pending surface is a conflict: {}",
            io.stderr()
        );
        assert!(io.stderr().contains("pending"), "{}", io.stderr());

        args[7] = "01ARZ3NDEKTSV4RRFFQ69G5FAZ".to_owned();
        let mut io = BufferIo::new("");
        assert_eq!(
            crate::run(args, &mut io),
            3,
            "an unknown surface is not found: {}",
            io.stderr()
        );

        revoke_surface(&store, &test_deps(&env), &env.surface, now).expect("revoke surface");
        let args = vec![
            "vowp".to_owned(),
            "--home".to_owned(),
            env.home.display().to_string(),
            "--project".to_owned(),
            env.project.display().to_string(),
            "surface".to_owned(),
            "rotate".to_owned(),
            env.surface.0.clone(),
        ];
        let mut io = BufferIo::new("");
        assert_eq!(
            crate::run(args, &mut io),
            5,
            "a revoked surface is refused: {}",
            io.stderr()
        );
    }

    #[test]
    fn agent_pair_command_carries_surface_session_and_app_but_never_a_code() {
        let surface = SurfaceId("01ARZ3NDEKTSV4RRFFQ69G5FAW".to_owned());
        let session = SessionId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("session");
        let command = agent_pair_command(&surface, &session, Some("opencode"));
        assert_eq!(
            command,
            "vowp agent pair --code-file - --surface 01ARZ3NDEKTSV4RRFFQ69G5FAW \
             --session 01ARZ3NDEKTSV4RRFFQ69G5FAV --app opencode"
        );
        assert!(!command.contains("code:"), "{command}");
    }

    #[test]
    fn parse_surface_id_rejects_a_path_traversal_value() {
        assert!(parse_surface_id("../../etc/passwd").is_err());
        assert!(parse_surface_id("").is_err());
        assert!(parse_surface_id("01ARZ3NDEKTSV4RRFFQ69G5FAW").is_ok());
    }
}
