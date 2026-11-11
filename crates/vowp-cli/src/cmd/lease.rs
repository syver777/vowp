//! Hidden `vowp session lease` and `vowp session takeover`.
//!
//! These are the reference lease operations used by `vowp chat`, the app hosts and the
//! MCP server. The normal path is a long-lived surface that keeps its [`LeaseGuard`];
//! `--acquire` is a one-shot form that prints the token for a caller that will heartbeat
//! the lease itself. Spec: `docs/design/v0.2/home-and-surfaces.md` §7.3.

use clap::Args;
use serde_json::json;
use vowp_core::surface::{AcquireLease, LeaseGuard};
use vowp_protocol::{LeaseStatus, NodeName, Session, SessionId, Surface, SurfaceId, SurfaceKind};
use vowp_store::{SessionFilter, Store};

use crate::output::print_json;
use crate::{CliError, Ctx, IoStreams};

/// Arguments for the hidden `vowp session lease`.
#[derive(Debug, Clone, Args)]
pub struct LeaseArgs {
    /// Session id or unique prefix.
    pub session: String,
    /// Acquire the lease for this surface kind (`chat`, `opencode`, ...).
    #[arg(long, value_name = "KIND")]
    pub acquire: Option<String>,
    /// Human-visible holder name.
    #[arg(long, value_name = "NAME")]
    pub holder: Option<String>,
    /// Node the surface runs on.
    #[arg(long, value_name = "NAME")]
    pub node: Option<String>,
}

/// Arguments for the hidden `vowp session takeover`.
#[derive(Debug, Clone, Args)]
pub struct TakeoverArgs {
    /// Session id or unique prefix.
    pub session: String,
    /// Surface kind taking over (`chat`, `opencode`, ...).
    #[arg(long, value_name = "KIND")]
    pub kind: String,
    /// Human-visible holder name.
    #[arg(long, value_name = "NAME")]
    pub holder: String,
    /// Node the new surface runs on.
    #[arg(long, value_name = "NAME")]
    pub node: String,
}

/// Runs the hidden `vowp session lease`.
pub fn lease(ctx: &Ctx, io: &mut dyn IoStreams, args: &LeaseArgs) -> Result<i32, CliError> {
    let store = ctx.store()?;
    let session = resolve_session(&store, &args.session)?;
    if let Some(kind) = &args.acquire {
        let kind = parse_kind(kind)?;
        let surface = build_surface(ctx, kind, args.holder.as_deref(), args.node.as_deref())?;
        let guard = LeaseGuard::acquire(
            ctx.deps(),
            &store,
            &AcquireLease {
                session: session.id.clone(),
                surface: surface.clone(),
                takeover: false,
            },
        )?;
        let lease = store
            .lease(&session.id)?
            .ok_or_else(|| CliError::Invalid("lease acquisition left no lease row".to_owned()))?;
        let status = LeaseStatus {
            held: true,
            holder: Some(lease.holder.clone()),
            kind: Some(lease.surface_kind),
            node: Some(lease.node.clone()),
            acquired_at: Some(lease.acquired_at),
            heartbeat_at: Some(lease.heartbeat_at),
            token: Some(guard.token().to_owned()),
        };
        let token = guard.token().to_owned();
        // `--acquire` is the one-shot form: the caller keeps the token and heartbeats the
        // lease itself, so this process must not release the row on exit.
        std::mem::forget(guard);
        if ctx.json() {
            print_json(io, &status)?;
        } else {
            writeln!(io.out(), "{token}")?;
        }
        return Ok(0);
    }

    let status = match store.lease(&session.id)? {
        Some(lease) => LeaseStatus {
            held: true,
            holder: Some(lease.holder),
            kind: Some(lease.surface_kind),
            node: Some(lease.node),
            acquired_at: Some(lease.acquired_at),
            heartbeat_at: Some(lease.heartbeat_at),
            token: None,
        },
        None => LeaseStatus {
            held: false,
            holder: None,
            kind: None,
            node: None,
            acquired_at: None,
            heartbeat_at: None,
            token: None,
        },
    };
    if ctx.json() {
        print_json(io, &status)?;
    } else if status.held {
        writeln!(
            io.out(),
            "held by {} ({}) on {} since {} (heartbeat {})",
            status.holder.as_deref().unwrap_or("unknown"),
            status.kind.map(kind_name).unwrap_or("unknown"),
            status.node.as_ref().map(NodeName::as_str).unwrap_or("unknown"),
            status
                .acquired_at
                .map(|ts| ts.as_rfc3339())
                .unwrap_or_else(|| "unknown".to_owned()),
            status
                .heartbeat_at
                .map(|ts| ts.as_rfc3339())
                .unwrap_or_else(|| "unknown".to_owned()),
        )?;
    } else {
        writeln!(io.out(), "no lease on session {}", session.id)?;
    }
    Ok(0)
}

/// Runs the hidden `vowp session takeover`.
pub fn takeover(ctx: &Ctx, io: &mut dyn IoStreams, args: &TakeoverArgs) -> Result<i32, CliError> {
    let store = ctx.store()?;
    let session = resolve_session(&store, &args.session)?;
    let kind = parse_kind(&args.kind)?;
    let surface = build_surface(ctx, kind, Some(&args.holder), Some(&args.node))?;
    let report = vowp_core::takeover(ctx.deps(), &store, &session.id, &surface, ctx.clock().now())?;
    if ctx.json() {
        print_json(io, &report)?;
    } else {
        match &report.previous {
            Some(previous) => {
                let capsule = report
                    .capsule_id
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "none".to_owned());
                writeln!(
                    io.out(),
                    "took session {} over from {} (drained {}, capsule {capsule})",
                    session.id,
                    previous,
                    report.drained
                )?;
            }
            None => writeln!(io.out(), "attached session {} to {}", session.id, args.holder)?,
        }
    }
    Ok(0)
}

/// Builds the transient surface row for one command.
fn build_surface(ctx: &Ctx, kind: SurfaceKind, holder: Option<&str>, node: Option<&str>) -> Result<Surface, CliError> {
    let node = match node {
        Some(node) => NodeName::parse(node)?,
        None => ctx.deps().node.clone(),
    };
    let holder = holder.map(str::to_owned).unwrap_or_else(|| node.to_string());
    let now = ctx.clock().now();
    Ok(Surface {
        id: SurfaceId::new(),
        kind,
        node,
        holder,
        app: None,
        cwd: None,
        metadata: json!({}),
        created_at: now,
        last_seen_at: now,
    })
}

/// Parses a surface kind from its CLI name.
fn parse_kind(value: &str) -> Result<SurfaceKind, CliError> {
    match value {
        "chat" => Ok(SurfaceKind::Chat),
        "opencode" => Ok(SurfaceKind::Opencode),
        "claude-code" | "claude_code" | "claudecode" => Ok(SurfaceKind::ClaudeCode),
        "mcp" => Ok(SurfaceKind::Mcp),
        "mirror" => Ok(SurfaceKind::Mirror),
        "api" => Ok(SurfaceKind::Api),
        other => Err(CliError::Invalid(format!(
            "unknown surface kind {other:?}; use chat, opencode, claude-code, mcp, mirror or api"
        ))),
    }
}

/// Returns the CLI name of a surface kind.
fn kind_name(kind: SurfaceKind) -> &'static str {
    match kind {
        SurfaceKind::Chat => "chat",
        SurfaceKind::Opencode => "opencode",
        SurfaceKind::ClaudeCode => "claude-code",
        SurfaceKind::Mcp => "mcp",
        SurfaceKind::Mirror => "mirror",
        SurfaceKind::Api => "api",
    }
}

/// Resolves a session id or unique prefix of at least six characters.
fn resolve_session(store: &Store, input: &str) -> Result<Session, CliError> {
    if let Ok(id) = SessionId::parse(input)
        && let Some(session) = store.session(&id)?
    {
        return Ok(session);
    }
    if input.len() < 6 {
        return Err(CliError::Invalid(
            "session id prefix must be at least 6 characters".to_owned(),
        ));
    }
    let prefix = input.to_ascii_uppercase();
    let matches: Vec<Session> = store
        .list_sessions(&SessionFilter::default())?
        .into_iter()
        .filter(|session| session.id.as_str().to_ascii_uppercase().starts_with(&prefix))
        .collect();
    match matches.as_slice() {
        [session] => Ok(session.clone()),
        [] => Err(CliError::NotFound(format!("session {input}"))),
        many => Err(CliError::Invalid(format!(
            "ambiguous session prefix {input}: {}",
            many.iter()
                .map(|session| session.id.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}
