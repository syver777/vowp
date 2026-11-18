//! Home-side v0.4 local-node agent support.
//!
//! Spec: `docs/design/v0.4/local-orchestrator.md` §4.1, §4.2, §4.6, §6.3, §8.2–§8.3 and §12.1.
//! Phase 0 implements enrollment, node-key authentication, heartbeat presence/capacity and the
//! stdio gateway. Phase 1 adds surface pairing: a one-time code becomes a scoped, node-bound
//! surface token. Phase 2 adds the home-side offload dispatcher: ready jobs become durable
//! assignments, and the gateway sends `AssignRun`, applies agent frames and fences expired
//! run leases. Phase 2 also returns a pushed result: the home fetches the `job/<id>` branch,
//! verifies the reported head and base, gates the diff and hands the job to the normal merge
//! queue. Worktree creation on the node stays in the agent crate. Phase 3 adds surface token
//! rotation with a ten-minute overlap and the durable `agent-frames` outbox that delivers a
//! system-initiated `SurfaceIssued` to the surface's live node connection. Phase 4 finishes the
//! rotation UX on the home: the outbox envelope carries the queue time and overlap deadline, the
//! grace is capped, rotate/revoke write one ledger note, `surface ls` reads only the display
//! prefix and expiries, and `surface pair --replace` revokes the old surface before creating its
//! replacement.

pub mod auth;
pub mod code;
pub mod dispatch;
pub mod enroll;
pub mod gateway;
pub mod pair;
pub mod result;
pub mod spool;

pub use code::{ENROLLMENT_CODE_TTL, NewEnrollmentCode, hash_code, new_enrollment_code};
pub use enroll::{EnrollRequest, Enrolled, PendingEnrollment, create_pending, redeem, revoke};
pub use gateway::{GatewayOptions, GatewayOutcome, HANDSHAKE_TIMEOUT, HEARTBEAT_SECS, MAX_FRAME_BYTES, serve};
pub use pair::{
    IssuedSurface, NewSurfaceCode, PendingSurface, RotatedSurface, RotationPolicy, SURFACE_CODE_TTL,
    SURFACE_TOKEN_MAX_GRACE, SURFACE_TOKEN_OVERLAP, SURFACE_TOKEN_TTL, SurfaceListRow, SurfaceRotation,
    VerifiedSurface, create_pending_surface, issue_surface_token, new_surface_code, replace_pending_surface,
    revoke_surface, revoke_surface_token, rotate_surface_token, rotate_surface_with_policy, surface_list_row,
    verify_surface_token,
};
pub use result::{AgentRunResult, Settled, load as load_run_result, result_key, settle_terminal};
pub use spool::{AgentFrameSpool, QueuedAgentFrame};
