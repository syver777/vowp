//! L8 watchdog helpers: the heartbeat file, the systemd unit and the cron check script.
//!
//! Spec: `docs/design/loops.md` §10. L1 writes `<node>/heartbeat` at the end of every tick;
//! when it is older than 180 seconds the cron check restarts the supervisor. The constants
//! here are the text `vowp init` installs.

use std::path::Path;
use std::time::Duration;

use vowp_protocol::Timestamp;
use vowp_store::NodePaths;

use crate::CoreError;
use crate::deps::Deps;

/// Seconds after which the heartbeat counts as stale.
pub const STALE_AFTER_SECS: u64 = 180;

/// The `vowp.service` systemd user unit text.
pub const SYSTEMD_UNIT: &str = "[Unit]\n\
Description=VOWP supervisor\n\
After=network.target\n\
\n\
[Service]\n\
Type=simple\n\
ExecStart=%h/.local/bin/vowp up --foreground\n\
Restart=always\n\
RestartSec=5\n\
\n\
[Install]\n\
WantedBy=default.target\n";

/// The cron check text (`scripts/vowp-watchdog.sh`).
pub const CRON_CHECK_SH: &str = "#!/bin/sh\n\
# VOWP watchdog: restart the supervisor when the heartbeat is older than 180 seconds.\n\
set -eu\n\
home=${VOWP_HOME:-$HOME/.vowp}\n\
heartbeat=\"$home/heartbeat\"\n\
log=\"$home/logs/watchdog.log\"\n\
mkdir -p \"$home/logs\"\n\
now=$(date +%s)\n\
if [ ! -f \"$heartbeat\" ]; then\n\
  stale=1\n\
else\n\
  value=$(cat \"$heartbeat\" 2>/dev/null || echo 0)\n\
  case \"$value\" in\n\
    ''|*[!0-9]*) stale=1 ;;\n\
    *) age=$((now - value)); if [ \"$age\" -gt 180 ]; then stale=1; else stale=0; fi ;;\n\
  esac\n\
fi\n\
if [ \"${stale:-0}\" -eq 1 ]; then\n\
  if command -v systemctl >/dev/null 2>&1 && systemctl --user restart vowp.service 2>/dev/null; then\n\
    echo \"$(date -u +%Y-%m-%dT%H:%M:%SZ) restarted vowp.service (heartbeat stale)\" >> \"$log\"\n\
  else\n\
    command -v vowp >/dev/null 2>&1 && vowp up --detach >> \"$log\" 2>&1 || true\n\
    echo \"$(date -u +%Y-%m-%dT%H:%M:%SZ) started vowp up --detach (heartbeat stale)\" >> \"$log\"\n\
  fi\n\
fi\n";

/// Writes `<node>/heartbeat` atomically (temporary file then rename).
pub fn write_heartbeat(deps: &Deps) -> Result<(), CoreError> {
    let root = deps.paths.root();
    std::fs::create_dir_all(root).map_err(|source| CoreError::Io {
        path: root.display().to_string(),
        source,
    })?;
    let now = deps.clock.now().unix_secs().to_string();
    let tmp = root.join(".heartbeat.tmp");
    let path = root.join("heartbeat");
    std::fs::write(&tmp, now).map_err(|source| CoreError::Io {
        path: tmp.display().to_string(),
        source,
    })?;
    std::fs::rename(&tmp, &path).map_err(|source| CoreError::Io {
        path: path.display().to_string(),
        source,
    })?;
    Ok(())
}

/// Returns how old the heartbeat is at `now`, or `None` when it is missing or unreadable.
pub fn heartbeat_age(paths: &NodePaths, now: Timestamp) -> Option<Duration> {
    let path = paths.root().join("heartbeat");
    let text = std::fs::read_to_string(Path::new(&path)).ok()?;
    let secs: i64 = text.trim().parse().ok()?;
    let age = now.unix_secs().saturating_sub(secs);
    Some(Duration::from_secs(u64::try_from(age).unwrap_or(0)))
}
