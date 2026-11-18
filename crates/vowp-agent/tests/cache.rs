//! The agent-side read cache: keying, bounds, TTL, permissions and surface isolation.
//!
//! Every test uses a temporary state directory and an injected timestamp. No real
//! `~/.vowp-agent` directory, socket or network is touched.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use vowp_agent::{
    AgentError, AgentStatus, CACHE_DIR, CACHE_MAX_BYTES, CACHE_MAX_ENTRIES, CACHE_TTL, Cache, CacheStats,
};
use vowp_protocol::{SurfaceId, Timestamp};

const SURFACE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";
const OTHER_SURFACE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAX";

fn surface(id: &str) -> SurfaceId {
    SurfaceId(id.to_owned())
}

fn at(secs: i64) -> Timestamp {
    Timestamp::from_unix_secs(secs)
}

fn envelope(text: &str) -> Value {
    json!({
        "content": [{"type": "text", "text": text}],
        "isError": false,
    })
}

/// Returns every file below `dir`, including hidden temporary files.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = match fs::read_dir(&current) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => panic!("read {}: {error}", current.display()),
        };
        for entry in entries {
            let entry = entry.expect("cache entry");
            let file_type = entry.file_type().expect("cache file type");
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if file_type.is_file() {
                files.push(entry.path());
            }
        }
    }
    files
}

fn cache_files(cache: &Cache) -> Vec<PathBuf> {
    files_under(cache.root())
}

#[test]
fn cache_never_stores_a_write_tool_or_a_secret() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = Cache::new(dir.path());
    let now = at(1_700_000_000);
    let secret = "cache-argument-value-that-must-never-be-written";
    let arguments = json!({
        "token": secret,
        "path": "/Users/me/private/notes.md",
        "project": "vowp",
    });

    // A write tool is a typed refusal for both sides of the cache.
    assert!(
        matches!(
            cache.store("inbox_answer", &surface(SURFACE), &arguments, &envelope("x"), now),
            Err(AgentError::Config(_))
        ),
        "a write tool must be refused"
    );
    assert!(
        matches!(
            cache.lookup("inbox_answer", &surface(SURFACE), &arguments, now),
            Err(AgentError::Config(_))
        ),
        "a write tool must never read the cache"
    );
    assert!(
        matches!(
            cache.store("session.show", &surface(SURFACE), &arguments, &envelope("x"), now),
            Err(AgentError::Config(_))
        ),
        "an unknown tool must be refused, not silently missed"
    );
    assert!(cache_files(&cache).is_empty(), "a refused store writes no file");

    // A read tool stores only the result and an opaque argument hash.
    cache
        .store("fleet_status", &surface(SURFACE), &arguments, &envelope("ok"), now)
        .expect("store a read");
    let files = cache_files(&cache);
    assert_eq!(files.len(), 1, "one entry is one file");
    let name = files[0]
        .file_name()
        .and_then(|name| name.to_str())
        .expect("cache file name");
    let text = fs::read_to_string(&files[0]).expect("cache file");
    for needle in [secret, "/Users/me/private/notes.md", "project", "token"] {
        assert!(!name.contains(needle), "the file name contains {needle}: {name}");
        assert!(!text.contains(needle), "the cache file contains {needle}: {text}");
    }
    assert!(text.contains("arguments_sha256"), "the key keeps only the hash");
    assert!(text.contains("\"stored_at\""), "the entry records its age");
}

#[test]
fn cache_is_bounded_and_prunes_old_entries() {
    let base = at(1_700_000_000);

    // Entry count: storing 300 entries keeps the newest 256, oldest first out.
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = Cache::new(dir.path());
    for index in 0..(CACHE_MAX_ENTRIES + 44) {
        cache
            .store(
                "fleet_status",
                &surface(SURFACE),
                &json!({"n": index}),
                &envelope(&format!("entry-{index}")),
                base.plus(Duration::from_secs(index as u64)),
            )
            .expect("store");
    }
    let stats = cache.stats();
    assert_eq!(stats.entries, CACHE_MAX_ENTRIES as u64);
    assert!(stats.bytes <= CACHE_MAX_BYTES);
    assert_eq!(
        stats.oldest_stored_at,
        Some(base.plus(Duration::from_secs(44))),
        "the oldest entries are evicted first"
    );
    assert!(
        cache
            .lookup(
                "fleet_status",
                &surface(SURFACE),
                &json!({"n": 0}),
                base.plus(Duration::from_secs(1_000))
            )
            .expect("lookup")
            .is_none(),
        "the oldest entry was evicted"
    );
    assert!(
        cache
            .lookup(
                "fleet_status",
                &surface(SURFACE),
                &json!({"n": CACHE_MAX_ENTRIES + 43}),
                base.plus(Duration::from_secs(1_000))
            )
            .expect("lookup")
            .is_some(),
        "the newest entry survives"
    );

    // Byte bound: 9 MiB of fresh entries is evicted back under 8 MiB.
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = Cache::new(dir.path());
    let big = "x".repeat(1024 * 1024);
    for index in 0..9 {
        cache
            .store(
                "capacity_report",
                &surface(SURFACE),
                &json!({"n": index}),
                &envelope(&big),
                base.plus(Duration::from_secs(index as u64)),
            )
            .expect("store");
    }
    let stats = cache.stats();
    assert!(stats.entries <= 8, "kept {} entries", stats.entries);
    assert!(stats.bytes <= CACHE_MAX_BYTES, "kept {} bytes", stats.bytes);
    assert!(
        cache
            .lookup(
                "capacity_report",
                &surface(SURFACE),
                &json!({"n": 0}),
                base.plus(Duration::from_secs(100))
            )
            .expect("lookup")
            .is_none(),
        "the oldest large entry was evicted"
    );
    assert!(
        cache
            .lookup(
                "capacity_report",
                &surface(SURFACE),
                &json!({"n": 8}),
                base.plus(Duration::from_secs(100))
            )
            .expect("lookup")
            .is_some(),
        "the newest large entry survives"
    );

    // TTL: exactly 24 hours is still fresh; one second more is not used and is removed.
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = Cache::new(dir.path());
    let stored = at(1_700_000_000);
    cache
        .store(
            "session_tail",
            &surface(SURFACE),
            &json!({"session": "01ARZ3NDEKTSV4RRFFQ69G5FAV"}),
            &envelope("tail"),
            stored,
        )
        .expect("store");
    assert!(
        cache
            .lookup(
                "session_tail",
                &surface(SURFACE),
                &json!({"session": "01ARZ3NDEKTSV4RRFFQ69G5FAV"}),
                stored.plus(CACHE_TTL)
            )
            .expect("lookup")
            .is_some(),
        "an entry at exactly the TTL is still fresh"
    );
    assert!(
        cache
            .lookup(
                "session_tail",
                &surface(SURFACE),
                &json!({"session": "01ARZ3NDEKTSV4RRFFQ69G5FAV"}),
                stored.plus(CACHE_TTL + Duration::from_secs(1))
            )
            .expect("lookup")
            .is_none(),
        "an entry older than the TTL is not used"
    );
    assert_eq!(cache.stats().entries, 0, "the expired entry was removed");

    // Prune removes expired entries without a lookup.
    cache
        .store(
            "session_tail",
            &surface(SURFACE),
            &json!({"session": "01ARZ3NDEKTSV4RRFFQ69G5FAV"}),
            &envelope("tail"),
            stored,
        )
        .expect("store");
    let stats = cache.prune(stored.plus(CACHE_TTL + Duration::from_secs(1)));
    assert_eq!(stats, CacheStats::default(), "prune leaves no entry behind");

    // A corrupt entry is skipped, never fatal, and prune removes it.
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = Cache::new(dir.path());
    fs::create_dir_all(cache.root()).expect("cache dir");
    fs::write(cache.root().join("broken.json"), b"{not json").expect("corrupt entry");
    let stats = cache.stats();
    assert_eq!(stats.entries, 1);
    assert_eq!(stats.oldest_stored_at, None);
    assert!(
        cache
            .lookup("fleet_status", &surface(SURFACE), &json!({}), stored)
            .expect("lookup")
            .is_none(),
        "a corrupt entry is a miss"
    );
    cache.prune(stored);
    assert_eq!(cache.stats().entries, 0, "prune removes the corrupt entry");
}

#[test]
fn cache_file_is_0600_and_its_directory_is_0700() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = Cache::new(dir.path());
    cache
        .store(
            "fleet_queue",
            &surface(SURFACE),
            &json!({"project": "vowp"}),
            &envelope("queued"),
            at(1_700_000_000),
        )
        .expect("store");

    assert_eq!(cache.root(), dir.path().join(CACHE_DIR));
    let root_mode = fs::metadata(cache.root())
        .expect("cache directory")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(root_mode, 0o700, "the cache directory must be 0700");
    let files = cache_files(&cache);
    assert_eq!(files.len(), 1);
    let mode = fs::metadata(&files[0]).expect("cache file").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "a cache entry must be 0600");
    assert!(
        files.iter().all(|path| !path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with('.'))),
        "no temporary file is left behind"
    );
}

#[test]
fn cache_key_and_file_do_not_contain_arguments() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = Cache::new(dir.path());
    let now = at(1_700_000_000);
    let arguments = json!({
        "project": "vowp",
        "filter": {"state": "running", "ids": [1, 2]},
    });
    cache
        .store("fleet_queue", &surface(SURFACE), &arguments, &envelope("queued"), now)
        .expect("store");

    let files = cache_files(&cache);
    assert_eq!(files.len(), 1);
    let name = files[0]
        .file_name()
        .and_then(|name| name.to_str())
        .expect("cache file name");
    let text = fs::read_to_string(&files[0]).expect("cache file");
    for needle in ["project", "vowp", "running", "filter", "ids"] {
        assert!(!name.contains(needle), "the key contains {needle}: {name}");
        assert!(!text.contains(needle), "the entry contains {needle}: {text}");
    }

    // Object key order does not change the hash, so the same read is one entry.
    let reordered = json!({
        "filter": {"ids": [1, 2], "state": "running"},
        "project": "vowp",
    });
    assert!(
        cache
            .lookup("fleet_queue", &surface(SURFACE), &reordered, now)
            .expect("lookup")
            .is_some(),
        "canonical arguments hit the stored entry"
    );
    assert_eq!(cache.stats().entries, 1, "canonical arguments reuse one entry");
}

#[test]
fn cache_is_cleared_when_the_surface_is_revoked() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = Cache::new(dir.path());
    let now = at(1_700_000_000);
    let arguments = json!({"project": "vowp"});
    cache
        .store("fleet_status", &surface(SURFACE), &arguments, &envelope("one"), now)
        .expect("store first surface");
    cache
        .store(
            "fleet_status",
            &surface(OTHER_SURFACE),
            &arguments,
            &envelope("two"),
            now,
        )
        .expect("store second surface");
    // A crashed write leaves a hidden temporary file behind; revocation must remove it
    // with the surface's entries so the old payload does not linger.
    let stale_surface = cache.root().join(format!(".{SURFACE}.fleet_status.abc.json.1.0.tmp"));
    let stale_other = cache
        .root()
        .join(format!(".{OTHER_SURFACE}.fleet_status.abc.json.1.0.tmp"));
    fs::write(&stale_surface, b"stale payload").expect("stale temp");
    fs::write(&stale_other, b"stale payload").expect("stale temp");

    // This is the call the client makes when the home sends `SurfaceRevoked`.
    let removed = cache.clear_surface(&surface(SURFACE)).expect("clear");
    assert_eq!(removed, 2, "the entry and its temporary file are removed");
    assert!(!stale_surface.exists(), "the revoked surface keeps no temporary file");
    assert!(stale_other.exists(), "another surface's temporary file is untouched");
    assert!(
        cache
            .lookup("fleet_status", &surface(SURFACE), &arguments, now)
            .expect("lookup")
            .is_none(),
        "the revoked surface cannot read its old entry"
    );
    assert!(
        cache
            .lookup("fleet_status", &surface(OTHER_SURFACE), &arguments, now)
            .expect("lookup")
            .is_some(),
        "another surface keeps its own entry"
    );

    // A fresh pairing for the same surface id replaces the old entries too.
    cache
        .store("fleet_status", &surface(SURFACE), &arguments, &envelope("three"), now)
        .expect("store after pairing");
    cache.clear_surface(&surface(SURFACE)).expect("clear on pairing");
    assert!(
        cache
            .lookup("fleet_status", &surface(SURFACE), &arguments, now)
            .expect("lookup")
            .is_none()
    );
    assert_eq!(cache.stats().entries, 1, "only the other surface remains");
    assert_eq!(cache.clear_surface(&surface(SURFACE)).expect("clear is idempotent"), 0);
    cache.prune(now);
    assert!(
        !stale_other.exists(),
        "prune removes stale temporary files of another surface"
    );
}

#[test]
fn an_older_status_json_still_loads() {
    let dir = tempfile::tempdir().expect("temp dir");
    let old = json!({
        "connection": "connected",
        "node": "mac-studio",
        "home": "home-1",
        "protocol": "vowp-agent/1",
        "last_heartbeat": null,
        "last_error": null,
        "pending_spool_bytes": 0,
        "key_fingerprint": null,
        "surface_token_expires_at": null
    });
    fs::write(
        AgentStatus::file_path(dir.path()),
        serde_json::to_vec(&old).expect("json"),
    )
    .expect("status.json");

    let status = AgentStatus::load(dir.path()).expect("load").expect("status present");
    assert_eq!(status.cache_entries, 0);
    assert_eq!(status.cache_bytes, 0);
    assert_eq!(status.cache_oldest_at, None);
}
