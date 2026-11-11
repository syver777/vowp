//! Tool catalogue and JSON-RPC result envelopes for `vowp mcp`.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §7.7. A tool failure is not a JSON-RPC
//! error: `tools/call` returns `{"isError":true,"content":[{"type":"text",...}]}` so the
//! model sees the refusal text, including the takeover notice.

use serde_json::{Value, json};

use crate::CliError;
use crate::output::kind_name;

use super::backend::Backend;

/// A tool-level failure with the stable error kind and the human text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    /// Stable kind name (`invalid_input`, `not_found`, `refused`, ...).
    pub kind: String,
    /// Human-readable message.
    pub message: String,
}

impl ToolError {
    /// Builds one tool error.
    pub fn new(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            message: message.into(),
        }
    }

    /// Builds an `invalid_input` error.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new("invalid_input", message)
    }

    /// Builds a `refused` error.
    pub fn refused(message: impl Into<String>) -> Self {
        Self::new("refused", message)
    }
}

impl From<CliError> for ToolError {
    fn from(error: CliError) -> Self {
        Self::new(kind_name(error.kind()), error.to_string())
    }
}

impl From<vowp_core::CoreError> for ToolError {
    fn from(error: vowp_core::CoreError) -> Self {
        Self::new(kind_name(error.kind()), error.to_string())
    }
}

impl From<vowp_store::StoreError> for ToolError {
    fn from(error: vowp_store::StoreError) -> Self {
        Self::new(kind_name(error.kind()), error.to_string())
    }
}

impl From<std::io::Error> for ToolError {
    fn from(error: std::io::Error) -> Self {
        Self::new("io", format!("io: {error}"))
    }
}

impl From<serde_json::Error> for ToolError {
    fn from(error: serde_json::Error) -> Self {
        Self::new("invalid_input", format!("json: {error}"))
    }
}

impl From<vowp_protocol::ProtocolError> for ToolError {
    fn from(error: vowp_protocol::ProtocolError) -> Self {
        Self::new(kind_name(error.kind()), error.to_string())
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.kind, self.message)
    }
}

/// Returns the MCP `tools/list` result.
pub fn definitions() -> Value {
    json!({
        "tools": [
            {
                "name": "fleet_status",
                "description": "Read the node snapshot: supervisor, projects, capacity, inbox and sessions.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "project": {"type": "string", "description": "Restrict the snapshot to one project."}
                    }
                }
            },
            {
                "name": "fleet_queue",
                "description": "List queued and waiting jobs across the fleet.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "project": {"type": "string", "description": "Restrict to one project."},
                        "state": {"type": "string", "description": "Restrict to one job state."}
                    }
                }
            },
            {
                "name": "inbox_list",
                "description": "List open inbox questions and approvals.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "project": {"type": "string", "description": "Restrict to one project."},
                        "all": {"type": "boolean", "description": "Include answered and dismissed items."}
                    }
                }
            },
            {
                "name": "capacity_report",
                "description": "Read the measured capacity report, target, brakes, estimates and waiting jobs.",
                "inputSchema": {"type": "object", "properties": {}}
            },
            {
                "name": "session_tail",
                "description": "Read recent events of a session.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session": {"type": "string", "description": "Session id or unique prefix; defaults to this server's session."},
                        "after_seq": {"type": "integer", "minimum": 0, "description": "Return events after this sequence number."},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "description": "Maximum events (default 50)."}
                    }
                }
            },
            {
                "name": "inbox_answer",
                "description": "Answer an open inbox item and resolve its parked job.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "Inbox id or unique prefix."},
                        "answer": {"type": "string", "description": "The owner's answer."}
                    },
                    "required": ["id", "answer"]
                }
            },
            {
                "name": "job_add",
                "description": "Add a job from a NewJob document and write its brief.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "project": {"type": "string", "description": "Project name; defaults to the session's project."},
                        "id": {"type": "string"},
                        "title": {"type": "string"},
                        "kind": {"type": "string", "enum": ["plan", "spec", "build", "test", "verify", "fix", "merge", "docs", "research", "promote", "release", "decision"]},
                        "role": {"type": "string", "enum": ["planner", "keeper", "worker", "verifier"]},
                        "profile": {"type": "string"},
                        "needs": {"type": "array", "items": {"type": "string"}},
                        "owns": {"type": "array", "items": {"type": "string"}},
                        "reads": {"type": "array", "items": {"type": "string"}},
                        "gates": {"type": "array", "items": {"type": "object"}},
                        "priority": {"type": "integer"},
                        "loop": {"type": "object"},
                        "brief_text": {"type": "string"}
                    },
                    "required": ["id", "title", "kind", "role"]
                }
            },
            {
                "name": "job_pause",
                "description": "Park a non-terminal job.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "job": {"type": "string"},
                        "project": {"type": "string", "description": "Project name; inferred from the session or the job id."},
                        "reason": {"type": "string"}
                    },
                    "required": ["job"]
                }
            },
            {
                "name": "job_retry",
                "description": "Retry a failed, stalled or parked job.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "job": {"type": "string"},
                        "project": {"type": "string", "description": "Project name; inferred from the session or the job id."}
                    },
                    "required": ["job"]
                }
            },
            {
                "name": "capacity_set",
                "description": "Set the worker target, recording the MCP surface as the actor.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "target": {"type": "integer", "minimum": 0},
                        "mode": {"type": "string", "enum": ["auto", "fixed"]},
                        "override": {"type": "boolean", "description": "Accept a target above the advice."},
                        "reason": {"type": "string"}
                    }
                }
            },
            {
                "name": "tunnel_open",
                "description": "Open a managed SSH tunnel. Only the control machine can do this.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "remote_port": {"type": "integer", "minimum": 1, "maximum": 65535},
                        "local_port": {"type": "integer", "minimum": 1, "maximum": 65535},
                        "host": {"type": "string", "description": "Remote host (default 127.0.0.1)."}
                    },
                    "required": ["name", "remote_port"]
                }
            },
            {
                "name": "tunnel_close",
                "description": "Close one managed SSH tunnel. Only the control machine can do this.",
                "inputSchema": {
                    "type": "object",
                    "properties": {"name": {"type": "string"}},
                    "required": ["name"]
                }
            },
            {
                "name": "secret_request",
                "description": "Start a one-time paste page for a secret and return its URL and expiry. The value is never returned.",
                "inputSchema": {
                    "type": "object",
                    "properties": {"name": {"type": "string"}},
                    "required": ["name"]
                }
            },
            {
                "name": "session_handoff",
                "description": "Write a capsule for a session so another app or model can continue it.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session": {"type": "string", "description": "Defaults to this server's session."},
                        "reason": {"type": "string"}
                    }
                }
            },
            {
                "name": "session_resume",
                "description": "Return the session, latest capsule, tail events and lease for continuing in another app.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session": {"type": "string", "description": "Defaults to this server's session."},
                        "surface": {"type": "string", "description": "Surface id to acquire on takeover."},
                        "takeover": {"type": "boolean", "description": "Take the lease from a live surface."},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 200, "description": "Tail events (default 20)."}
                    }
                }
            }
        ]
    })
}

/// Handles one `tools/call` request.
pub fn call(backend: &Backend<'_>, name: &str, arguments: &Value) -> Value {
    match backend.call(name, arguments) {
        Ok(value) => tool_success(&value),
        Err(error) => tool_failure(&error),
    }
}

/// Wraps a successful value in the MCP content envelope.
fn tool_success(value: &Value) -> Value {
    let text = serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned());
    json!({
        "content": [{"type": "text", "text": text}],
        "isError": false,
    })
}

/// Wraps a tool error in the MCP content envelope.
fn tool_failure(error: &ToolError) -> Value {
    json!({
        "content": [{"type": "text", "text": error.to_string()}],
        "isError": true,
    })
}
