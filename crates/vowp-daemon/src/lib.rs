//! The VOWP daemon: a loopback HTTP API over one existing VOWP home.
//!
//! The v0.3 slice serves the static board page at `GET /` and `GET /board`, the static
//! session list and detail pages at `GET /sessions` and `GET /sessions/{id}`, the static
//! capsule page at `GET /sessions/{id}/capsules/{capsule_id}`, the static cost, inbox and nodes
//! pages at `GET /cost`, `GET /inbox` and `GET /nodes`, the static ASIF import page at
//! `GET /import`, and the API endpoints
//! `GET /api/v1/health`, `/api/v1/board`, `/api/v1/board/stream`, `/api/v1/jobs`,
//! `/api/v1/sessions`, `/api/v1/sessions/{id}`, `/api/v1/sessions/{id}/capsules/{capsuleId}`,
//! `/api/v1/sessions/{id}/events`, `/api/v1/sessions/{id}/events/stream`,
//! `/api/v1/sessions/{id}/export/asif`, `/api/v1/sessions/{id}/switch`, `/api/v1/inbox`,
//! `/api/v1/nodes` and `/api/v1/cost` with the same `vowp-protocol` objects the CLI prints.
//! The session detail
//! contains the exact `Session`, its latest capsule and at most 32 parent links, nearest
//! parent first; the capsule route answers only for a capsule belonging to that session.
//! The per-session stream sends stored `EventEnvelope` frames with periodic heartbeats and a
//! bounded per-client batch. The board stream sends the exact running `Board` document when
//! its content changes, plus heartbeats, and has no cursor or resume.
//!
//! The board page is also the phone board: its card layout is usable at a 360 px viewport.
//! It loads `/api/v1/board`, follows `/api/v1/board/stream`, and keeps a visible reconnect
//! state with bounded-backoff retry and a five-second polling fallback until the stream
//! opens. The session list page fetches `/api/v1/sessions`; the detail page loads its
//! metadata and a first event page, then follows the per-session SSE stream, with a visible
//! reconnect state, bounded-backoff retry and a five-second polling fallback. The capsule page
//! fetches one capsule from `/api/v1/sessions/{id}/capsules/{capsuleId}` and renders its
//! metadata and structured fields read-only.
//! The session detail page also carries the token-gated executor switch form. The cost page
//! renders grouped ledger rows and totals with a `by` selector. The inbox page renders open,
//! answered and dismissed items with a state selector, and its answer form is one of the two
//! browser writes. The nodes page fetches the stored nodes from `/api/v1/nodes` on load and
//! every five seconds and renders them read-only, with no add, remove or probe path.
//!
//! ASIF download and import are the v0.4 slice. The session detail page links to the
//! token-free `GET /api/v1/sessions/{id}/export/asif`, which streams a deterministic
//! `.asif.zip` from a private temporary file and removes it when the response ends; the same
//! route with `?report=1` returns the `AsifReport` summary and the page's preview button
//! calls it. `POST /api/v1/sessions/import/asif?branch=<id>` requires the write token, streams
//! the raw zip to a private `0600` upload file under `<home>/tmp`, imports one new idle
//! session through the core ASIF facade and answers `201 Created` with
//! `AsifImportResponse`. The static `/import` page performs the token unlock, the browser-side
//! size check and the upload; it never puts the token or a filename in a URL.
//!
//! Reads are token-free. `POST /api/v1/inbox/{id}/answer`,
//! `POST /api/v1/sessions/{id}/switch` and `POST /api/v1/sessions/import/asif` require
//! `Authorization: Bearer <VOWP_API_TOKEN>` and answer `503` when the daemon was started
//! without a token. The answer handler opens the
//! home writable with [`vowp_store::Store::open_writable`], writes the same item update and
//! `answer_inbox` request row as `vowp inbox answer`, and leaves job state to the supervisor.
//! The switch handler runs the same `SessionService` switch as `vowp session switch` over a
//! writable store, with the CLI's running/switch/closed/limit guards and a five-second drain.
//! Neither write calls `Store::open`, so neither creates or migrates a home. Every other
//! request uses [`vowp_store::Store::open_read_only`]. The listener binds `127.0.0.1` only,
//! and the daemon never prints a home path, token or secret.
//!
//! Spec: `docs/design/v0.3/http-api.md`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod app;
mod asif;
mod auth;
mod config;
mod error;
mod filters;
mod stream;

pub use app::{ApiState, build_deps, router, serve, serve_with_shutdown};
pub use auth::ApiToken;
pub use config::{Invocation, Options, ProjectSelection, USAGE, parse_invocation, select_project};
pub use error::DaemonError;
