# VOWP ↔ ASIF integration (v0.4)

Status: implemented for the v0.4 ASIF slice; ASIF 0.4 remains a review draft. This document is
the cross-cutting contract for the core facade, the CLI and the daemon routes. The build-job split
in §8 is the record of how it was built; the independent verification job (E) has not run at the
time of writing.

- ASIF: `0.4` (review draft dated 5 October 2026), pinned at
  `652d9e23ddbe5656b9ceea311df7e09e93c1cd2f`.
- Adapter: public `vowp-asif` `0.1.1` from crates.io, declared as `vowp-asif = "=0.1.1"` in
  `Cargo.toml` and resolved from `registry+https://github.com/rust-lang/crates.io-index` in
  `Cargo.lock`. The reviewed adapter source is tag `v0.1.1`, commit
  `466ebb81f58646febb80996874b46c11288d5a97`.
- VOWP target: protocol version `1`, capsule version `1`, the v0.2 session-directory
  layout and the v0.3 daemon API.
- Native bundle: `vowp-bundle-v1` remains the default and is not changed.

ASIF 0.4 is a review draft, not a released standard. Nothing in this integration may claim
ASIF conformance, a lossless translation, exact request reconstruction, portable
continuation or native-harness import.

## Implementation status

The core facade (`vowp_core::session::asif`), the CLI (`vowp session export|import --format
asif`), the daemon download/report/import routes and the `/import` page are in the tree. The
command and route surface below is the one those slices implement; `docs/design/cli.md` and
`docs/design/v0.3/http-api.md` hold the command-local and route-local details. The independent
verification job (E) is still open; the adapter is published on crates.io. See §9.

---

## 1. What exists today

| Piece | State |
|---|---|
| Adapter | Complete public CLI and library: `export`, `validate`, `import`, typed `AsifDocument`, `Report`/`Loss`, deterministic `ZIP_STORED` packages, offline schemas. |
| VOWP bundle | `vowp session export <id>` writes `./<id>.vowp-bundle.json`; `vowp session import <file>` creates a child, or the same id with `--same-id`; remote export/import over SSH; `session pull`/`session push`. Unchanged. |
| ASIF core facade | `vowp_core::session::asif::export_asif` / `import_asif`, `MAX_ASIF_ARCHIVE_BYTES`, and the adapter-free `AsifReport`/`AsifImportResponse` types in `vowp-protocol`. The adapter is a runtime dependency of `vowp-core` only; `vowp-cli` names it only as a dev-dependency for fixtures. |
| ASIF CLI | `vowp session export <id> --format asif` writes one deterministic `.asif.zip`; `vowp session import <file> --format asif` creates one new idle session from a package, a standalone `session.json`, a directory or stdin. Redaction, report, strict/allow-loss and the remote stream are implemented. |
| Session directory | `sessions/<id>/log.jsonl`, `blobs/<sha256>`, `capsules/<at_seq>-<ulid>.json`, `adapters.json`, optional `manifest.json`; jobs, gates, leases and project rows live outside it. |
| Daemon web | Read-only `/sessions`, `/sessions/{id}`, `/board`, `/cost`, `/inbox`, `/nodes`, `/jobs`; token-gated write routes for inbox answers, session switch, job actions and ASIF import; token-free ASIF download and loss preview; the `/import` page and PWA shell entry. |
| Missing | ASIF conformance, exact request reconstruction, portable continuation, native-harness import, and the first-slice exclusions in §3.6 (`--asif-output`, `--workspace-root`, `--include-raw`, `--vowp-sidecar`, `--allow-unsupported`, `--same-id`/`--executor` for ASIF, `--report` over `--node`). |

This integration adds one format, one library boundary and two web actions. It does not add a
second ASIF implementation.

---

## 2. Product contract

1. **ASIF is a second format, not a replacement.** `--format bundle` stays the default for
   both export and import. Existing bundle output, JSON shapes, exit codes and remote paths
   are unchanged.
2. **One adapter implementation.** VOWP calls the public `vowp-asif` library in-process. It
   does not copy the mapping code, does not fork ASIF and does not shell out to an installed
   `vowp-asif` binary in the product path.
3. **Honesty is part of the contract.** Every ASIF command reports declared losses and
   unsupported VOWP operations. The default succeeds with losses; it never says "lossless".
4. **Continuation is not claimed.** No `asif.portable-continuation/0.2` object is emitted or
   interpreted. Imported sessions are idle data; VOWP does not promise that another tool, or
   a later VOWP turn, can continue them exactly.
5. **Native harnesses are not imported.** `adapters.json` is evidence of a mapping, not a
   Claude Code, opencode, CLI or ACP store.

---

## 3. CLI contract

### 3.1 `vowp session export`

```text
vowp session export <id> [--format bundle|asif] [--out <file>|-] [--tail <n>]
                     [--redact <literal>]... [--redact-file <file>]...
                     [--keep-host-paths] [--report <file>]
                     [--strict|--allow-loss] [--json]
```

| Flag | `bundle` (default) | `asif` |
|---|---|---|
| `--format` | Current behaviour, unchanged. | Writes one deterministic `.asif.zip` package through the adapter's `ExportFormat::Zip`. |
| `--out` | Default `./<id>.vowp-bundle.json`; `-` writes text to stdout. | Default `./<id>.asif.zip` using the resolved full session id; `-` writes binary ZIP bytes to stdout. Binary stdout is refused on a TTY (exit 2) so a terminal is not filled with ZIP bytes. |
| `--tail` | Current behaviour (default 20, max 500). | Invalid: exit 2. An ASIF tail would be a different capture, not a short export. |
| `--redact` | Invalid: exit 2. | Repeatable exact literal replaced by `<redacted>`; the value is never printed or reported. Convenience only: it appears in shell history and `ps`. |
| `--redact-file` | Invalid: exit 2. | Read exact literals from a local file, one per line (trailing `\r`/`\n` removed; empty lines skipped). The file is not logged or copied. Use this for secret values so they do not appear in argv. |
| `--keep-host-paths` | Invalid: exit 2. | Keep host-path-looking strings in a trusted local export. Default is to scrub them. |
| `--report` | Invalid: exit 2. | Write the full `vowp-asif-report-v1` JSON to this file. `--report -` is refused (exit 2). Without it no report sidecar is left in the project; the summary is printed. |
| `--strict` | Invalid: exit 2. | Refuse losses that are `Blocking` or `Integrity` (see §3.4). No new output is kept; an existing `--out`/`--report` file is left untouched. |
| `--allow-loss` | Invalid: exit 2. | Pass through to the adapter; recoverable integrity/ambiguity problems become declared losses. |
| `--json` | Current shape. | Print one JSON summary; conflicts with `--out -` (exit 2). |

`--strict` and `--allow-loss` conflict with each other (clap `conflicts_with`, exit 2).

The first slice supports only the ZIP package for ASIF. Directory and single-JSON ASIF
output stay behind the adapter's own CLI; VOWP may add `--asif-output zip|dir|json` later
without changing this contract.

Export is read-only: unlike the bundle path it writes no export capsule, takes no session
lock and moves no lease. It reads the current files. A compacted session with no
`log.jsonl` exports its capsule as the captured boundary, not a full-history replay. A torn
or corrupt `log.jsonl` is invalid input (exit 2) and leaves no output; it is never silently
truncated.

Human output is one line on stdout (the output path) and warnings on stderr. With `--out -`,
stdout is the package bytes only; the summary and warnings go to stderr. `--json` prints:

```json
{
  "format": "asif",
  "path": "/home/user/project/01KA...asif.zip",
  "session": "01KA...",
  "bytes": 18342,
  "report_path": null,
  "report": {
    "adapter_version": "0.1.1",
    "asif_version": "0.4",
    "asif_pin": "652d9e23ddbe5656b9ceea311df7e09e93c1cd2f",
    "source_session": "01KA...",
    "target_session": null,
    "counts": {"events_in": 42, "events_out": 42, "resources": 3, "capsules": 1},
    "loss_count": 2,
    "losses": [
      {"kind": "cwd-omitted", "stage": "translation", "scope": "environment", "message": "..."},
      {"kind": "contexts-reconstructed", "stage": "normalization", "scope": "contexts", "message": "..."}
    ],
    "unsupported": ["vowp.jobs", "vowp.gates", "vowp.leases", "vowp.project-db"]
  }
}
```

`loss_count` is the number; `losses` is the summary list (kind, stage, scope, path-free
message). The full `vowp-asif-report-v1` document is written only with `--report`. The
summary never contains source text, secret values or absolute host paths.

### 3.2 `vowp session import`

```text
vowp session import <file>|- [--format bundle|asif] [--node <name>]
                    [--cwd <dir>] [--branch <id>] [--report <file>]
                    [--strict|--allow-loss] [--json]
```

| Flag | `bundle` (default) | `asif` |
|---|---|---|
| Input | Existing bundle file or `-` stdin. | A `.asif.zip` package, a standalone `session.json`, or a directory containing `session.json`; `-` stages stdin in a private temporary file first. |
| `--node` | Current remote import. | Same remote import over SSH (see §3.5). |
| `--cwd` | Current behaviour. | Working directory written to `session.created`; default is the selected project path. The directory must exist; the allow-list is enforced when a turn starts, exactly as for a bundle import. |
| `--branch` | Invalid: exit 2. | Select one ASIF branch when the document has more than one. Without it a multi-branch document is exit 2 and the error names the available branch ids. To list them first, extract the document (`unzip -p <id>.asif.zip session.json > asif-session.json`) and run `jq -r '.branches[].id' asif-session.json`; for a standalone document, run the `jq` command on it directly. |
| `--same-id` | Current behaviour. | Invalid: exit 2. ASIF import always creates a new session. |
| `--executor` | Current behaviour. | Invalid: exit 2. Executor evidence comes from the document; overriding would invent state. |
| `--report` | Invalid: exit 2. | Keep the full adapter report at this path. Without it the report is temporary and removed; the summary is still printed. Not supported with `--node` (exit 2) because the full report stays on the node. |
| `--strict` | Invalid: exit 2. | Refuse `Blocking`/`Integrity` losses before the new session becomes visible; the staging directory is removed. |
| `--allow-loss` | Invalid: exit 2. | Pass through to the adapter. |

ASIF import rules:

1. **New session only.** A fresh VOWP ULID is chosen before the adapter runs. The target must
   be absent as a `sessions` row and as any entry at `sessions/<id>` (a directory, file or
   symlink); otherwise exit 4 and change nothing. There is no overwrite, merge or `--same-id`
   path.
2. **Validate before the session exists.** The adapter validates JSON/schema/package,
   features and resource bytes before it creates anything. Writes go to a private staging
   directory under `<home>/tmp`, never directly into `sessions/`.
3. **The adapter owns the translation.** VOWP does not re-map events. It pre-checks the
   staged log with the same rules the indexer applies (parse, `session.created` first, clean
   tail, no duplicate event ids), then renames the directory to `sessions/<id>/` and runs the
   existing indexer. The indexer must produce exactly one session row. Only the expected
   `project ... left null` / `job ... left null` foreign-key warnings are returned as
   warnings; any other reindex problem removes the new directory and leaves no partial index.
4. **Failure leaves nothing.** A bad package, strict loss, reindex failure or rename collision
   removes the staging directory, any temporary upload and any partial report. No row, log,
   blob, capsule or lease is created.
5. **Idle data.** The imported session is `idle`. Import never starts a turn, writes a new
   capsule, creates a lease or dispatches a job. Restoring capsule-provenance files carried by
   the package (§5.3) is part of the import and does not write a new capsule.
6. **Warnings are not losses.** A source project or job that is absent locally leaves the
   session's `project`/`job_id` null. The `project ... left null` / `job ... left null`
   warnings come from the reindex that runs after the staged directory is renamed, the same
   path a full-directory pull uses; bundle import does not produce them because it resolves
   both keys to `None` before it writes the session. Those warnings go to stderr, not to the
   ASIF loss list. A source project name that is not a valid VOWP project slug is invalid
   input (exit 2), not a partial session. A session imported from another project may
   consequently not appear in the served project's `session ls`; it is still reachable by id.
   Binding it to the local project instead is an owner decision (§9).
7. **Event identity is global.** ASIF import preserves VOWP-origin event IDs. Importing the
   same package twice into one home therefore conflicts on the unique event id, even though
   the target session id is new: exit 4, the first session is untouched, and the second
   staging directory is removed. This is deliberate; VOWP event ids are globally unique. A
   foreign event whose id is not a VOWP ULID is regenerated by the adapter with an
   `event-id-regenerated` loss and does not conflict.

Human output is the new session id on stdout; warnings and a one-line loss summary go to
stderr. `--json` prints:

```json
{
  "session": { "id": "01KA...", "title": "…", "last_seq": 42, "...": "…" },
  "report": {
    "adapter_version": "0.1.1",
    "asif_version": "0.4",
    "asif_pin": "652d9e23ddbe5656b9ceea311df7e09e93c1cd2f",
    "source_session": "01KA-source",
    "target_session": "01KA...",
    "counts": {"events_in": 42, "events_out": 44, "resources": 3, "capsules": 1},
    "loss_count": 3,
    "losses": [
      {"kind": "capture-id-not-preserved", "stage": "translation", "scope": "native", "message": "..."},
      {"kind": "cwd-rebound", "stage": "translation", "scope": "environment", "message": "..."},
      {"kind": "session-id-changed", "stage": "translation", "scope": "native", "message": "..."}
    ],
    "unsupported": []
  },
  "warnings": []
}
```

The bundle import JSON shape is unchanged.

### 3.3 Exit codes

VOWP keeps the `cli.md` §1.1 table. The adapter's exit classes are translated by stable code,
not by passing its process exit through:

| Condition | Exit |
|---|---|
| Success; declared losses may exist and are reported | 0 |
| Internal adapter or write failure; I/O failure | 1 |
| Invalid package, JSON, schema, ASIF version, resource integrity; unsafe file path; torn source log; bad flag combination; an oversized stdin upload | 2 |
| Unknown source session or remote node | 3 |
| Target session id/path already exists, or an imported event id already belongs to another session | 4 |
| Strict refusal; required unsupported ASIF feature; other adapter refusal | 5 |
| Remote transport failure, or a truncated/corrupt ASIF export stream | 7 |

`TARGET_EXISTS` maps to 4, `BRANCH_AMBIGUOUS`/`BRANCH_UNKNOWN` to 2, and every other adapter
`Refused` code to 5. A remote command's exit code is relayed unchanged after its output;
nothing is written locally.

### 3.4 Losses, `--strict` and `--allow-loss`

The adapter declares a loss for every known deviation. A normal VOWP export always has
inherent losses (`cwd-omitted`, `no-causal-edges`, `contexts-reconstructed`,
`configuration-unknown`, `tool-definition-unknown`, `workspace` limits, capsule summary
limits). An import always has `session-id-changed`, `capture-id-not-preserved` and
`cwd-rebound`. The adapter's own `--strict` means "any declared loss is fatal", so exposing
that switch directly would make every real VOWP session fail. VOWP therefore defines strict
mode in terms of loss severity:

| Mode | Export | Import |
|---|---|---|
| Default | Declared omissions and normalizations succeed and are reported. Blocking/integrity problems still refuse. | Same, including the unavoidable import baseline. |
| `--strict` | Refuse if any loss is `Blocking` or `Integrity`; keep no new package/report and leave an existing output untouched; exit 5. Inherent `Omission`/`Normalization` losses stay non-fatal. | Refuse the same classes before the session becomes visible; remove staging; exit 5. |
| `--allow-loss` | Pass to the adapter: recoverable duplicate/orphan/ambiguity problems become declared losses. It does not make unsupported semantics safe. | Same. |

The adapter's `LossClass` is available on the in-memory report even though it is not
serialized. The core integration filters on it; the adapter is always called with
`strict: false`. In `--strict` mode the CLI also passes `allow_loss: true`, so recoverable
integrity/ambiguity problems are declared by the adapter instead of being refused before the
VOWP filter sees them; the filter then refuses any `Blocking`/`Integrity` loss.

`--allow-unsupported` is deliberately not exposed. A required ASIF feature this adapter does
not interpret (`continuation`, `streams`, `external_bindings`, an unknown required extension)
refuses the command (exit 5) and names the feature. Dropping required semantics is not a
normal user action.

Loss evidence:

- `--report <file>` keeps the full `vowp-asif-report-v1` document (schema, pin, mapping,
  losses, unsupported list).
- `--json` keeps the summary (`loss_count`, `losses`, `unsupported`).
- Human output always prints the loss count and the unique kinds to stderr; zero losses is
  stated explicitly.
- An explicit `--report` path is written through a private temporary file and replaced only
  after a successful export/import. A strict failure never deletes or truncates an existing
  report file.
- The web export shows the count in a response header and a preview route (§6).

### 3.5 Remote export and import over SSH

`--node <name>` uses the existing SSH transport and node registry. A node registered as
local falls through to the local command (the existing export/import behaviour) and does not
open an SSH path. No daemon, HTTP or network path is added.

**Export.** The local side runs, on the remote node:

```text
vowp --project <project> --json session export <id> --format asif --out - --asif-stream \
  [--redact <literal>]... [--keep-host-paths] [--strict|--allow-loss]
```

The local side reads `--redact-file` itself and forwards each literal as its own
`--redact <literal>`; the file is never sent. `--keep-host-paths`, `--strict` and
`--allow-loss` are forwarded unchanged, so the remote core calls the adapter with the same
`redact`, `keep_host_paths` and `allow_loss` values (and `strict: false`, as §3.4 specifies)
and applies the same VOWP policy. Because
the adapter's `LossClass` is not serialized, the local side cannot re-apply the
`Blocking`/`Integrity` strict filter to the envelope: that filter runs on the remote against
the in-memory report, after the adapter has produced the package in its private temporary
file and before the remote writes the envelope or any package byte. A strict refusal is exit
5 with no stream and no output kept on either side; the local side relays the exit code and
stderr. `--allow-loss` reaches the remote adapter, so a recoverable integrity problem becomes
a declared loss there.

`--asif-stream` is a hidden internal flag (like `session dir-export`, not help-listed). On
success the remote prints exactly one JSON line, then the exact package bytes:

```json
{"format":"vowp-asif-export-stream-v1","session":"01KA...","bytes":18342,
 "sha256":"<64 lowercase hex>","report":{"adapter_version":"0.1.1",
 "asif_version":"0.4","asif_pin":"652d9e23ddbe5656b9ceea311df7e09e93c1cd2f",
 "source_session":"01KA...","target_session":null,
 "counts":{"events_in":42,"events_out":42,"resources":3,"capsules":1},
 "loss_count":2,
 "losses":[{"kind":"cwd-omitted","stage":"translation","scope":"environment","message":"..."}],
 "unsupported":["vowp.jobs","vowp.gates","vowp.leases","vowp.project-db"]}}
```

The line is bounded by the adapter JSON limit (16 MiB + 4 KiB) and parsed before any local
file is touched. The remote stream mode is quiet: the envelope line and the package bytes are
the only stdout; loss warnings are in the envelope. The local side streams exactly `bytes` to
a private temporary file (or `<out>.tmp`), verifies the byte count and SHA-256, then renames
to `--out` or copies to stdout for `--out -`; it does not re-check the strict decision, which
the remote already made. A failed verification never leaves a partial stdout package.
A missing byte, extra byte, hash mismatch or malformed line is exit 7 and removes the
temporary file. `--report` with `--node` is refused (exit 2); the summary is still returned.
Remote stderr is relayed for errors.

**Import.** The local side streams the file (or stdin) to the remote's stdin and runs:

```text
vowp --project <project> --json session import - --format asif [--branch <id>] [--cwd <dir>] \
  [--strict|--allow-loss]
```

The remote performs the same validate/stage/rename/reindex flow and returns the
`AsifImportResponse` JSON. `--strict` and `--allow-loss` are forwarded so the remote core
applies the same loss policy and exit mapping as a local import (§3.3/§3.4); the local side
does not re-check the returned report. The local side prints that document (or a human
summary) and relays the exit code. `--report` with `--node` is refused (exit 2). A directory
input is local-only; `--node` with a directory is exit 2 (use a package file or stdin). No
local session, temp download, row or index entry is created on failure.

The hidden stream is an internal CLI-to-CLI protocol, not a public API. It is documented in
`docs/design/cli.md` only because `session pull --full` already documents its stream.

### 3.6 Out of scope for the first slice

- `--asif-output dir|json`, `--workspace-root`, `--include-raw`, `--vowp-sidecar`,
  `--allow-unsupported`.
- `--same-id` and `--executor` for ASIF.
- A `vowp session validate` command; the standalone `vowp-asif validate` remains the
  inspection tool.
- Transferring a full adapter report over SSH (`--report` with `--node`).
- A daemon-to-daemon or control-machine ASIF transport.

---

## 4. Dependency choice

### 4.1 Options

| Option | One implementation | Cold offline build | crates.io packaging | Maintenance |
|---|---|---|---|---|
| Git dependency pinned by `rev` | Yes | No (needs the git object) | **No**: crates.io rejects non-registry dependencies | Lowest; follows the public tag |
| crates.io dependency `=0.1.1` | Yes | No cold, then registry cache | Yes | Lowest, once published |
| Vendored workspace crate | Yes, but a source copy | Yes | Yes | Sync commits, licence notices, duplicate review |
| Installed `vowp-asif` binary | Yes | Yes if installed | Yes | Runtime version skew, subprocess/JSON contract, extra install step |

### 4.2 Recommendation

**Primary:** depend on the published crate `vowp-asif = "=0.1.1"` from crates.io. This is the
only option that is both one implementation and compatible with the existing
`scripts/release/check-packages.sh` / `publish-crates.sh` gates.

**Current dependency:** `vowp-asif 0.1.1` is published on crates.io and all three manifests
name it as a registry dependency:

```toml
vowp-asif = "=0.1.1"
```

`Cargo.lock` resolves it from `registry+https://github.com/rust-lang/crates.io-index` and
records no git source. The exact pin keeps an adapter patch update explicit, as §4.4
requires. The version `0.1.1`, ASIF `0.4` and the ASIF pin are recorded in this document and
asserted in the adapter-version test.

**Rejected for now:**

- **Vendoring** copies the adapter into the VOWP tree, adds a sync/update burden and makes
  two review surfaces for the same mapping. It is the fallback only if the adapter cannot be
  published and a crates.io release remains mandatory.
- **Invoking the installed binary** makes ASIF support depend on a separately installed
  tool, couples VOWP to the adapter CLI's JSON/exit contract and cannot be tested in-process.
  It is not a first-class format.

### 4.3 Build, offline, release, licence and CI implications

- **Build:** `vowp-core` is the only runtime crate that names `vowp-asif`; `vowp-cli` and
  `vowp-daemon` consume the core facade, and `vowp-cli` also names it as a dev-dependency for
  fixture construction. The manifest line is `vowp-asif = "=0.1.1"`; a version change edits
  the crate manifest and `Cargo.lock`.
- **Offline:** a cold checkout needs network access to fetch the registry crate. CI and the
  release workflow already have network. `cargo fetch --locked` warms the cache; after that
  `--offline` works. At runtime the adapter is offline: schemas are embedded at compile time
  and export/validate/import make no network call.
- **Release:** with a git source, `cargo publish -p vowp-core` (and therefore
  `vowp-daemon`/`vowp-cli`) could not pass. The build job did not hide this by removing
  `vowp-core` from `check-packages.sh` or by adding `publish = false`. `vowp-asif 0.1.1` is
  now published on crates.io; the manifests use `vowp-asif = "=0.1.1"` and no code changes
  were needed. **Outcome in this tree:** `Cargo.lock` resolves the adapter from
  `registry+https://github.com/rust-lang/crates.io-index` with no git source, and
  `scripts/release/check-packages.sh` packages and dry-run publishes all nine workspace
  crates cleanly.
- **Licence:** the adapter is MIT. It vendors the two ASIF schemas and fixtures under MIT
  with upstream notices. Regenerate `THIRD-PARTY-LICENSES.md` with
  `python3 scripts/release/generate-third-party-licenses.py` and keep the `--check` gate.
  The release bundle contains the adapter licence and ASIF notices from the crates.io
  package.
- **CI:** the adapter's own CI runs fmt, clippy and tests. VOWP adds a `cargo test -p
  vowp-core --test asif` gate and a `cargo tree -p vowp-core -i vowp-asif` evidence line so
  the exact dependency is visible in the build log. The ASIF pin is reviewed in `Cargo.lock`
  and in the doc.

### 4.4 Feature flag and version policy

There is **no `asif` Cargo feature in the first slice**. A default-off feature would create
builds where the command exists but the format does not, which is a worse user contract than
a present command. If offline/minimal builds become a requirement, add a default-on `asif`
feature later; the adapter is a registry dependency and that choice is independent of the
feature.

Versioning:

- Adapter `0.1.x` implements ASIF `0.4` exactly. A new ASIF draft gets a new adapter minor
  and a new VOWP minor; mappings are never silently reinterpreted.
- VOWP protocol/capsule changes require an adapter release and an integration doc update.
- `vowp --version` stays the VOWP version; the ASIF summary reports the adapter version and
  the ASIF pin. There is no separate `vowp-asif` binary in the VOWP installation.

---

## 5. Mapping and honesty

The authoritative mapping is the adapter's `docs/MAPPING.md` and `docs/COMPATIBILITY.md`.
This section records what VOWP exposes and what it must not claim.

### 5.1 Session directory mapping

| VOWP source | ASIF representation | Quality | VOWP exposure |
|---|---|---|---|
| `log.jsonl` events | `events[]` in sequence order; `sequence = seq - 1` | exact IDs for one-to-one events; derived IDs for fan-out | All 17 protocol-1 event types are mapped. |
| `blobs/<sha256>` | `resources[]` with embedded bytes, media type and SHA-256; `resource_change` where provenance exists | exact bytes | Missing or mismatched bytes refuse unless `--allow-loss`, then they are a loss. |
| `capsules/<at_seq>-<ulid>.json` | `session_memory` resource, `resource_change`, summary context, checkpoint | exact capsule bytes; summary content is derived | Capsule bullets are model-written summaries, not observed decisions. |
| `adapters.json` | `vowp-native-adapters` resource and `session.native_ids[]` | exact bytes | Evidence only; never a native transcript or a resume instruction. |
| `manifest.json` | not exported | informational | The adapter says it is not authoritative; a copied directory does not need one. |
| `session.created.cwd` | not emitted | lossy | `cwd-omitted`; import rebinds to the local `--cwd`/project path (`cwd-rebound`). |
| `parent_session_id` | `vowp.session/1` additional property | lossy | No ASIF lineage: the parent capture boundary is unknown (`parent-lineage-unresolved`). |
| jobs, gates, leases, project rows | not in the session directory | unsupported | `vowp-operations-not-captured`; only a caller-supplied sidecar (not exposed) could embed them opaquely. |

### 5.2 Event mapping

| VOWP event | ASIF target | Quality |
|---|---|---|
| `session.created` | `extension` `vowp.session.created/1` + session metadata | exact except cwd |
| `session.executor` | `extension` `vowp.session.executor/1` | exact; native id is evidence |
| `turn.started` / `turn.ended` | `execution_transition` with `vowp.turn/1` | exact, including stop reason |
| `user.message` | `message` role `user` | exact text; user attachments become BlobRefs |
| `assistant.message` | `message` role `assistant` | exact text |
| `assistant.reasoning` | `note` with an opaque resource | exact bytes; non-replayable |
| `tool.call` | `tool_call` | exact arguments; tool schema unknown |
| `tool.result` | `tool_result` terminal | exact output or blob + exit code |
| `file.change` | `resource_change` + diff/unavailable resource | derived; no full-file snapshot |
| `plan` | `task_update` fan-out | derived; textual task identity |
| `switch` | `extension` `vowp.switch/1` | exact |
| `capsule` | `resource_change` + `session_memory` + checkpoint/context | exact bytes; derived summary |
| `error` | `extension` `vowp.error/1` | exact |
| `note` | `note` | exact |
| `limit` | `extension` `vowp.limit/1` + checkpoint requirement | exact; destination must re-check |
| `cost` | `extension` `vowp.cost/1` + `usage[]` | exact event; derived usage |
| unknown event type | refused by default; `vowp.unknown/1` with `--allow-loss` | lossy/opaque |

Import reverses these mappings through the adapter. ASIF objects with no VOWP
representation (contexts, configurations, decisions, foreign task history, foreign
checkpoints) become non-replayable notes or `raw` values with declared losses. VOWP does not
enforce ASIF instructions, approvals or policies.

### 5.3 Identities, blobs, capsules and source ids

- **Export source id:** `session.id` is the VOWP ULID; ASIF `session.native_ids[]` also
  records it with provider `vowp`, so a later import can name the origin.
- **Import target id:** always a new VOWP ULID. The ASIF source id is kept in the report, in
  an import note and in the original event `raw`; `session-id-changed` is a declared loss.
  `--same-id` does not exist for ASIF. Event ids are preserved for VOWP-origin events, and
  VOWP's event index is globally unique, so the same package can be imported once per home;
  a second import is a conflict (exit 4), not an overwriting or ID-rewriting import.
- **Blob bytes:** identical bytes share one resource; import writes exact bytes to
  `blobs/<sha256>` and verifies the hash before the session becomes visible. Unavailable
  resources are not invented; they are notes plus `resource-unavailable`.
- **Capsules:** export preserves exact capsule JSON, except the ASIF resource identity; import
  restores only capsule-provenance resources, rewrites `session_id` to the new session,
  clamps `at_seq` into the imported range when needed and reports
  `capsule-id-regenerated` / `capsule-at-seq-normalized`. Restoring a capsule-provenance file
  is not writing a new capsule; the imported file keeps the package's capsule content. A
  capsule is a summary, not an approval or a replayable instruction.
- **Native ids:** known harness providers (`claude-code`, `opencode`, `cli`, `acp`) are
  written to `adapters.json` on import; unknown providers are omitted with a loss. `vowp` is
  the source session identity, not a harness mapping.
- **Executor metadata:** an imported `session.executor` attach is a record of the source
  executor. Import never starts a process, takes a lease or resumes that executor.

### 5.4 Redaction and secrets

- `--redact <literal>` replaces exact literal occurrences in text fields. The matched value
  is never echoed in the report, error or output. Because a secret in argv is visible in
  shell history and `ps`, secrets should use `--redact-file <file>` (one literal per line),
  which VOWP reads locally and passes to the adapter in-process. With `--node` the literals
  are forwarded to the remote argv (see §3.5), so a secret read from `--redact-file` is
  visible in the node's process list for the duration of the export; use a local export when
  that matters. Both forms are best-effort, not secret discovery.
- Host-path-looking strings are replaced by `<host-path>` by default (`host-path-scrubbed`).
  `--keep-host-paths` is for a trusted local export only. Absolute cwd is never emitted in
  either mode.
- Binary resource bytes are not scanned (`host-path-not-inspected`); redaction never claims
  that arbitrary content contains no secret.
- The web download uses the safe defaults (host paths scrubbed, no redaction list). A user
  who needs exact host paths must use the CLI with `--keep-host-paths`.
- Reports contain basenames and logical identifiers, not source content, secret values or
  absolute host paths. The VOWP wrapper keeps that guarantee; it must not add paths to error
  messages.

### 5.5 What must not be claimed

- No ASIF conformance. ASIF 0.4 is a review draft; the adapter validates a documented subset.
- No lossless round trip. "Round trip" means semantic interchange with declared losses.
- No exact request reconstruction: system prompt, provider encoding and full tool schemas are
  not recorded.
- No continuation: no portable-continuation object, no `ready` assessment, no automatic
  resume.
- No workspace snapshot: `file.change` diffs are evidence, not a tree; no Git index, commit,
  bundle, submodule or LFS content is captured by default.
- No native-harness import: `adapters.json` is evidence only.
- No jobs, gates, leases, approvals or project rows in the file.
- No secret guarantee: redaction and host-path scrubbing are transformations, not a scan.

---

## 6. Web download and upload

The daemon remains loopback-only, one project per process. The ASIF additions keep the sessions
list and detail page read-only for ASIF: export is a token-free `GET` and import is a separate,
token-gated action and page.

### 6.1 Download as `.asif.zip`

Route: `GET /api/v1/sessions/{id}/export/asif`.

- **Read, token-free, loopback.** It matches the existing token-free `GET` contract. The
  session must exist in the served home. The daemon's ownership boundary is the local home,
  not a multi-user project ACL: `GET /api/v1/sessions/{id}` and the event routes already
  address any session id in the home, while the list pages stay project-filtered. A future
  multi-user daemon needs a real authorization model; this slice does not pretend the
  project filter is one.
- **Response:** `200` with `Content-Type: application/zip`,
  `Content-Disposition: attachment; filename="<id>.asif.zip"`, `Cache-Control: no-store`,
  `X-Content-Type-Options: nosniff`, `Content-Length` when known,
  `X-Vowp-Asif-Losses: <n>` and `X-Vowp-Asif-Unsupported: <n>`.
- **Body:** streamed from a private temporary file under `<home>/tmp/asif-export-<ulid>/`.
  The adapter writes the deterministic package and report there; the handler streams 64 KiB
  chunks and removes the directory when the response ends or the client disconnects. The
  handler does not buffer the package whole, but the adapter's own export step may hold it
  in memory up to its 272 MiB archive limit.
- **Loss preview:** `GET /api/v1/sessions/{id}/export/asif?report=1` runs the same export,
  discards the package and returns the `AsifReport` JSON summary (counts, the loss list,
  unsupported list, adapter/ASIF versions). `report` accepts only `1`/`true`; anything else
  is `400`. The preview recomputes the export; a later optimization may cache it against the
  session's last sequence and log hash.
- **Errors:** unknown session/directory `404`; a stored session that cannot be exported
  (torn log, corrupt capsule) `409` with a path-free message and stable code; an export over
  the adapter's archive/resource limits `413`; an internal I/O failure `500`. No error
  includes the home path, source text, blob content or a secret value.
- **UI:** the session detail page gets an "Export" card with a plain
  `<a download>` link to the route and a "Preview declared losses" button that fetches
  `?report=1` and shows `N losses: kind, kind…` or "No declared losses." The card says host
  paths are scrubbed but exact secrets are not detected, and points to the CLI
  `--redact-file` for a specific redaction list. The page never claims the export is
  lossless.

### 6.2 Import action and page

Route: `POST /api/v1/sessions/import/asif?branch=<id>`.

- **Write, token-gated.** `Authorization: Bearer <VOWP_API_TOKEN>` is checked before the body
  is read. No token: `503`; missing/wrong token: `401` with `WWW-Authenticate: Bearer`. The
  token is never put in a URL, echo or log.
- **Body:** raw `.asif.zip` bytes with `Content-Type: application/zip`,
  `application/x-zip-compressed` or `application/octet-stream`. Other content types `415`.
  No multipart parser is added.
- **Size:** streamed to a private `0600` file under `<home>/tmp/asif-upload-<ulid>/` with a
  hard 272 MiB cap (`MAX_ARCHIVE_BYTES`). Exceeding it removes the file and returns `413`.
  The route also sets a body limit of `cap + 1` so a chunked upload cannot grow unbounded.
- **Validation and write:** the temp file is handed to the same core import as the CLI. The
  adapter validates the package/schema/features/resources before creating a staging session;
  the core verifies the staged log and reindexes before the final rename. A new VOWP ULID is
  chosen up front; an existing row or any entry at the target path is `409`. Nothing is
  overwritten or merged.
- **Ownership and cwd:** the new session is created in the served project's context; `cwd` is
  the registered project path (same allow-list rules as a bundle import). The ASIF source
  project is kept in the session metadata when it exists locally; otherwise the session gets
  a null `project` and the `project ... left null` reindex warning, the same path a
  full-directory pull uses (bundle import resolves the key to `None` before writing and
  produces no such warning). No other project's data is read or written.
- **No auto-run:** the imported session is idle. The handler never starts a turn, writes a new
  capsule, creates a lease or dispatches a job. Capsule-provenance files carried by the
  package are restored by the import (§5.3); no new capsule is written.
- **Response:** `201 Created`, `Location: /api/v1/sessions/{id}`, body
  `AsifImportResponse { session, report, warnings }` (same shared type as the CLI). `session`
  is the normal registered `Session` row, so it carries the fields every session API returns,
  including the project `cwd` and the imported `title`. The report is the summary, not the
  full 16 MiB report; `X-Vowp-Asif-Losses` repeats the count.
- **Errors:** invalid package/schema/version/branch `400` with a stable code and a safe
  message; too large `413`; existing target, duplicate event id or a missing registered
  project path `409`; internal failure `500`. The response never echoes uploaded bytes, the
  uploaded filename, a temporary upload/staging path or source text; the only paths in it are
  the ordinary `Session` fields described above.

Page: `GET /import` serves a static shell with:

- the same token unlock form as the sessions page (`sessionStorage`, never the URL);
- a file input accepting `.asif.zip,application/zip`;
- an optional branch text field (shown always; the API error says a branch is required when
  the package has more than one, and the field lets the user name it);
- a submit button that checks the client-side size, posts the raw file with an explicit
  `Content-Type: application/zip` and shows the new session link, event/loss counts and a
  "not continued automatically" note;
- `401` clears the token, `413` names the limit, `400` shows the API hint. No external
  scripts, fonts or images; no filename is sent to the server.

The sessions page nav gains an "Import" link; the import page has a link back. The detail page
keeps only the download action. This is the "read-only download plus a separate import
action" choice: a GET never creates state, and every upload is an explicit token-gated POST.

### 6.3 Authentication and ownership summary

| Action | Method | Token | Project scope |
|---|---|---|---|
| List/show sessions | GET | none | served project |
| Download ASIF / preview report | GET | none | session exists in the served home |
| Import ASIF | POST | required | new session in the served home; the source project is kept when it exists locally |
| Switch/inbox/jobs | POST | required | existing behaviour |

There are no user accounts or per-user quotas. "Ownership" means the local home; the
project filter scopes lists and pages, not read authorization. A future multi-user daemon
would need a real authorization model; this slice does not invent one.

### 6.4 Temp files, limits and secret exposure

- Private directories use a fresh ULID, mode `0700`; uploaded files and packages use `0600`.
- Temp directories are removed on success, error, a five-minute upload/export timeout and
  client disconnect. A crash may leave an `asif-*` directory under `<home>/tmp`; it contains
  only the caller's upload or export, never a token or a log line.
- Limits: 16 MiB session/report JSON, 64 MiB single resource, 256 MiB total resources,
  272 MiB archive, 1,024 members, 4,096-character portable paths. Exceeding a limit is a
  typed error, never a truncation.
- No uploaded bytes, source text, secrets, tokens, uploaded filenames or temporary
  upload/staging paths are logged or returned. Errors use stable codes and path-free
  messages. The report's loss explanations are the adapter's path-free text. The success body
  is the exception defined in §6.2: the registered `Session` row, whose `cwd` is the project
  path and whose `title` is the imported session title.
- The web does not offer `--redact` or `--keep-host-paths`; it uses the safe defaults. A user
  who needs a specific redaction list uses the CLI.

### 6.5 No deployment in the build jobs

The web job ships code, tests and the static pages only. It does not start the daemon, open a
port, deploy, publish, change repository visibility or create a release. Browser testing is
against a temporary home and a loopback listener in the test process.

---

## 7. Tests

Tests are hermetic: temporary `NodePaths`, temporary project directories, a real
`127.0.0.1:0` listener only for daemon tests, fake SSH only for remote tests, no real home,
no real tmux server, no real harness binary, no external network. ASIF fixtures are built in
the test with `vowp_asif::pack` from a minimal `session.json` value, not copied from a
developer's home. For the multi-branch and invalid cases, start from one valid export of a
synthetic session and mutate the JSON (add a branch, truncate a member, change a version
byte); do not hand-maintain a second full document.

### 7.1 Core (`crates/vowp-core/tests/asif.rs`)

- `asif_export_writes_a_deterministic_package_and_summary` — same source twice produces the
  same bytes; summary counts match the log.
- `asif_export_redacts_exact_literal_and_never_echoes_it` — the literal is absent from the
  package, report and error strings.
- `asif_export_strict_refuses_blocking_loss_and_removes_output` — no package/report remains.
- `asif_export_keeps_inherent_losses_non_fatal` — a normal session with omissions succeeds
  and reports them.
- `asif_import_creates_one_indexed_session_and_no_turn` — log parses, `session.created` is
  first, state is idle, `session_events` rows exist, no new capsule or lease is created;
  capsule-provenance files carried by the fixture are restored, not rewritten.
- `asif_import_round_trip_preserves_text_blobs_and_event_order`.
- `asif_import_invalid_package_leaves_no_directory_or_row`.
- `asif_import_existing_id_is_a_conflict_and_changes_nothing`.
- `asif_import_same_package_twice_conflicts_on_event_ids_and_keeps_the_first_session`.
- `asif_import_duplicate_event_id_conflicts_without_a_partial_index`.
- `asif_import_strict_removes_staging_and_reports_the_loss`.
- `asif_import_allow_loss_accepts_a_recoverable_integrity_fixture`.
- `asif_import_multiple_branches_requires_a_selection` and
  `asif_import_selected_branch_maps_only_that_branch`.
- `asif_adapter_version_and_pin_match_the_documented_contract`.

### 7.2 CLI (`crates/vowp-cli/tests/asif.rs` and the existing `session.rs` remote helpers)

- `session_export_asif_uses_default_asif_zip_name`.
- `session_export_asif_stdout_is_binary_and_refuses_json`.
- `session_export_asif_refuses_binary_stdout_on_a_tty`.
- `session_export_asif_rejects_tail_and_bundle_only_flags`.
- `session_export_asif_human_and_json_shapes`.
- `session_export_asif_then_import_round_trip` — one test owns the full temp-home flow:
  create a synthetic session, export, import, inspect the new log/blobs, export again and
  compare the stable fields.
- `session_export_asif_redact_file_is_read_locally_and_never_echoed`.
- `session_export_asif_keep_host_paths_is_opt_in`.
- `session_export_asif_reports_losses_and_writes_report_only_when_asked`.
- `session_export_asif_strict_exits_five_and_writes_nothing`.
- `session_import_asif_creates_a_new_session_and_prints_its_id`.
- `session_import_asif_json_shape_and_full_report`.
- `session_import_asif_requires_an_existing_cwd`.
- `session_import_asif_stdin_stages_and_imports`.
- `session_import_asif_invalid_package_exits_two_and_writes_nothing`.
- `session_import_asif_same_package_twice_exits_four_and_keeps_the_first_session`.
- `session_import_asif_multiple_branches_requires_branch`.
- `session_import_asif_same_id_and_executor_are_refused`.
- `session_import_asif_strict_and_allow_loss_map_to_the_documented_exit_codes`.
- `session_export_asif_remote_stream_verifies_bytes_and_hash` (fake ssh).
- `session_export_asif_remote_failure_writes_no_local_file`.
- `session_export_asif_remote_report_is_refused_with_exit_two`.
- `session_import_asif_remote_streams_and_relays_the_response`.
- `session_import_asif_remote_failure_creates_no_local_session`.
- `session_import_asif_remote_report_is_refused_with_exit_two`.
- `session_import_asif_remote_directory_is_refused`.

### 7.3 Web (`crates/vowp-daemon/tests/asif_export_api.rs`, `asif_import_api.rs`, `session_page.rs`, `pwa.rs`)

- `asif_download_returns_zip_attachment_headers_and_exact_bytes`.
- `asif_download_report_query_returns_counts_and_losses`.
- `asif_download_unknown_session_is_404_and_a_home_session_is_allowed`.
- `asif_download_corrupt_session_is_409_and_leaves_no_temp`.
- `asif_download_over_limit_is_413`.
- `asif_import_without_token_is_503_and_with_wrong_token_is_401`.
- `asif_import_rejects_other_content_types_with_415`.
- `asif_import_over_limit_is_413_and_removes_the_upload`.
- `asif_import_invalid_package_is_400_and_creates_no_session`.
- `asif_import_duplicate_event_ids_is_409_and_keeps_the_existing_session`.
- `asif_import_creates_one_idle_session_with_201_and_location`.
- `asif_import_never_echoes_uploaded_bytes_or_a_filename`.
- `asif_import_branch_is_required_for_a_multi_branch_package`.
- `sessions_page_detail_has_the_asif_download_link`.
- `import_page_posts_raw_zip_with_bearer_and_clears_on_401`.
- `service_worker_shell_includes_the_import_page_and_a_bumped_cache_name`.
- Every returned JSON document validates against its exported schema (`AsifReport`,
  `AsifImportResponse`).

### 7.4 Documentation and verification

- `docs/design/cli.md` commands have matching `--help` text; no invented flag.
- `docs/design/v0.3/http-api.md` routes match the router; no placeholder route.
- The integration doc names only commands that exist after the build.
- The verification job runs the end-to-end flow below and records the output.

---

## 8. Build-job split

Each job has one owner per path. The dependency order is A → B/C → D → E. `Cargo.lock` is
the shared exception: let cargo update it, never edit it by hand.

**Build status.** A (core), B (CLI) and C (web) are implemented in the tree; D updates this
document and the user documentation; E is the independent verification and remains open. The
per-job owns lists and gates below are the build record, not a claim that every later v0.4
slice has shipped.

**Verification gates.** Every job runs the public gates below and records the exact output in
its report. The gates are `cargo fmt --all -- --check`,
`cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`,
`bash docs/tests/run.sh` and `python3 docs/reference/check-reference.py`. A job passes this
part of its gate only when every applicable command exits 0 and the output contains no finding
in a path the job changed. A new finding in the job's own diff fails the job.

### Job A — `v4-asif-core` (adapter dependency, shared report types, core facade)

**Owns**

- `Cargo.lock`
- `crates/vowp-core/Cargo.toml`
- `crates/vowp-core/src/error.rs`
- `crates/vowp-core/src/lib.rs`
- `crates/vowp-core/src/session/mod.rs`
- `crates/vowp-core/src/session/asif.rs` (new) and `crates/vowp-core/src/session/asif/**` (new)
- `crates/vowp-core/tests/asif.rs` (new)
- `crates/vowp-protocol/src/asif.rs` (new)
- `crates/vowp-protocol/src/lib.rs`
- `crates/vowp-protocol/src/schema.rs`
- `crates/vowp-protocol/tests/api.rs`
- `docs/design/types.md`
- `docs/design/crates/vowp-core.md`
- `docs/reference/schema/` (regenerate the new ASIF report pages)
- `THIRD-PARTY-LICENSES.md`

**Deliverable**

1. Add the pinned `vowp-asif` dependency to `vowp-core` only.
2. Define the shared, adapter-free report types in `vowp_protocol::asif`:
   ```rust
   pub struct AsifCounts { pub events_in: u64, pub events_out: u64,
                           pub resources: u64, pub capsules: u64 }
   pub struct AsifLoss { pub kind: String, pub stage: String,
                         pub scope: Option<String>, pub message: String }
   pub struct AsifReport {
       pub adapter_version: String, pub asif_version: String, pub asif_pin: String,
       pub source_session: Option<String>, pub target_session: Option<String>,
       pub counts: AsifCounts, pub loss_count: u64, pub losses: Vec<AsifLoss>,
       pub unsupported: Vec<String>,
   }
   pub struct AsifImportResponse { pub session: Session, pub report: AsifReport,
                                  pub warnings: Vec<String> }
   ```
   The CLI's `AsifExportOutput` adds `path`, `format`, `bytes` and `report_path` locally.
   No absolute paths, no source text.
3. `vowp_core::session::asif` exposes:
   ```rust
   pub const MAX_ASIF_ARCHIVE_BYTES: u64;

   pub struct AsifExportOptions {
       pub redact: Vec<String>,
       pub keep_host_paths: bool,
       pub strict: bool,
       pub allow_loss: bool,
       pub report_path: Option<PathBuf>,
   }
   pub struct AsifExport { pub output: PathBuf, pub bytes: u64, pub report: AsifReport }
   pub fn export_asif(paths: &NodePaths, session: &SessionId, out: &Path,
                      opts: &AsifExportOptions) -> Result<AsifExport, CoreError>;

   pub struct AsifImportOptions {
       pub cwd: PathBuf,
       pub branch: Option<String>,
       pub strict: bool,
       pub allow_loss: bool,
       pub session_id: Option<SessionId>,
       pub report_path: Option<PathBuf>,
   }
   pub struct AsifImport { pub session: Session, pub report: AsifReport,
                           pub warnings: Vec<String> }
   pub fn import_asif(paths: &NodePaths, store: &Store, input: &Path,
                      opts: &AsifImportOptions) -> Result<AsifImport, CoreError>;
   ```
4. `CoreError` gains an ASIF variant whose `kind()` maps the stable adapter code to
   `InvalidInput`, `Conflict`, `Refused` or `Internal` as described in §3.3. Messages stay
   path-free.
5. Export writes to a private temp file, checks the report's source session id, applies the
   VOWP strict filter, then atomically renames to the requested output; an existing output is
   untouched until that final rename. Import writes to staging, checks the target is absent
   and that the report's target id matches, pre-checks the staged log, renames, reindexes and
   cleans up on every failure.
6. Add the ASIF report documents to `schema::all_schemas()` and the protocol API tests.
7. Update `docs/design/types.md` with the new report types and
   `docs/design/crates/vowp-core.md` with the module/API, in the same change.
8. Regenerate `THIRD-PARTY-LICENSES.md` and keep the generator check green.

**Gates**

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test -p vowp-core --test asif
cargo test -p vowp-protocol --test api
cargo tree -p vowp-core -i vowp-asif
cargo build -p vowp-cli --bin vowp
python3 docs/reference/generate.py --check
python3 docs/reference/check-reference.py
python3 scripts/release/generate-third-party-licenses.py --check
```

### Job B — `v4-asif-cli` (CLI contract and remote paths)

**Owns**

- `Cargo.toml` (root: the `vowp-cli` package manifest; add the `[[test]] name = "asif"` entry and any dev-dependency)
- `crates/vowp-cli/src/cli.rs`
- `crates/vowp-cli/src/cmd/mod.rs`
- `crates/vowp-cli/src/cmd/session.rs`
- `crates/vowp-cli/src/cmd/session_asif.rs` (new) and `crates/vowp-cli/src/cmd/session_asif/**` (new)
- `crates/vowp-cli/tests/asif.rs` (new)
- `crates/vowp-cli/tests/session.rs` (remote helper additions)
- `docs/design/cli.md`
- `docs/design/crates/vowp-cli.md`
- `docs/reference/cli.md` (regenerate the help tree)

**Depends on:** A.

**Deliverable:** the `--format bundle|asif` flags, validation of incompatible flags,
default naming, stdout/stdin (including the TTY refusal), `--redact`/`--redact-file`,
report/strict/allow-loss, remote stream/import, exit mapping, and the named CLI tests. Keep
`export`/`import` dispatch in `session.rs`; put the ASIF work in `session_asif.rs`. Do not
change bundle output shapes. Update `docs/design/cli.md` and
`docs/design/crates/vowp-cli.md` with the command and module contract in the same change.

**Gates**

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test -p vowp-cli --test asif
cargo test -p vowp-cli --test session
cargo build -p vowp-cli --bin vowp
python3 docs/reference/generate.py --check
python3 docs/reference/check-reference.py
```

### Job C — `v4-asif-web` (daemon download/upload)

**Owns**

- `crates/vowp-daemon/Cargo.toml`
- `crates/vowp-daemon/src/app.rs`
- `crates/vowp-daemon/src/asif.rs` (new) and `crates/vowp-daemon/src/asif/**` (new helper modules)
- `crates/vowp-daemon/src/lib.rs`
- `crates/vowp-daemon/web/sessions.html`
- `crates/vowp-daemon/web/import.html` (new)
- `crates/vowp-daemon/web/sw.js` (add `/import` to the shell, bump the cache name)
- `crates/vowp-daemon/tests/asif_export_api.rs` (new)
- `crates/vowp-daemon/tests/asif_import_api.rs` (new)
- `crates/vowp-daemon/tests/session_page.rs`
- `crates/vowp-daemon/tests/pwa.rs`
- `crates/vowp-daemon/README.md`
- `docs/design/v0.3/http-api.md`
- `docs/design/v0.3/web-window.md`

**Depends on:** A.

**Deliverable:** the two routes, streaming temp files, token gate, ownership rules, limits,
static pages, schema-validated responses and the named tests. Update `http-api.md`,
`web-window.md` and the daemon README in the same change. No deploy, no port left open after
tests, no real home.

**Gates**

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test -p vowp-daemon --test asif_export_api
cargo test -p vowp-daemon --test asif_import_api
cargo test -p vowp-daemon --test session_page
cargo test -p vowp-daemon --test pwa
```

### Job D — `v4-asif-docs` (user and design documentation)

**Owns**

- `docs/design/v0.4/asif-integration.md` (this file; mark implemented only after B/C pass)
- `docs/design/sessions.md` (the session-level export/import narrative)
- `docs/design/README.md`
- `README.md`
- `CHANGELOG.md`

**Depends on:** B and C.

**Deliverable:** the cross-cutting integration doc, the session-level narrative and the
top-level user/changelog updates. Review the A/B/C spec edits for one consistent command and
route surface. Every command shown must run. Mark ASIF as a review draft; do not add
conformance, lossless, continuation or native-import claims.

**Gates**

```sh
cargo test --workspace
cargo build -p vowp-cli --bin vowp
python3 docs/reference/check-reference.py
```

### Job E — `v4-asif-verify` (independent verification, no code ownership)

**Owns:** nothing. Writes only its report.

**Depends on:** A, B, C, D.

**Gates and evidence**

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build -p vowp-cli --bin vowp
python3 docs/reference/generate.py --check
python3 docs/reference/check-reference.py
```

Then run the end-to-end test that owns the temporary home and the fake project:

```sh
cargo test -p vowp-cli --test asif session_export_asif_then_import_round_trip -- --nocapture
```

That test creates a synthetic session, exports it, imports it, inspects the new log and
blobs, exports the imported session again and compares the stable fields. The verifier must
also read the test and confirm:

- text, event count/order and blob hashes match where the adapter guarantees them, with
  every expected difference listed;
- the report's loss list and unsupported list match this doc;
- no `jobs`, `gates`, `leases`, native transcript or continuation object is in the package.

Finally run `scripts/release/check-packages.sh` and record the exact result. With
`vowp-asif = "=0.1.1"` resolved from crates.io it must pass for every workspace crate,
including `vowp-core`/`vowp-daemon`/`vowp-cli`.

---

## 9. Risks and owner decisions

These were the open decisions before the build. The shipped slice resolves 2–8 and 10–11 as
recommended: one new idle session per successful import, the source project kept or left
null, token-free download, strict as `Blocking`/`Integrity` only, a summary-only remote
stream with `--report` refused over `--node`, the adapter's 272 MiB web cap, safe web
defaults, the document staying under `docs/design/v0.4/`, and event-id conflict on a repeat
import. 9 is enforced in the CLI, the web copy and this document. 1 is resolved:
`vowp-asif 0.1.1` is published on crates.io and the workspace uses `vowp-asif = "=0.1.1"`.
12 remains a versioning constraint; no `asif` Cargo feature was added.

1. **Public git dependency vs release pin.** A git `rev` dependency would block crates.io
   publishing of `vowp-core`, `vowp-daemon` and `vowp-cli`. Resolved: `vowp-asif 0.1.1` is
   published on crates.io and every manifest uses `vowp-asif = "=0.1.1"`; `Cargo.lock`
   records only the registry source, and `scripts/release/check-packages.sh` passes for all
   nine workspace crates. The vendoring fallback is no longer needed. Do not silently drop
   the publish gate.
2. **Does upload create a session or only validate it?** Recommended: create one new idle
   session per successful upload, with no auto-run, because that matches `session import`
   and gives the user something to open. Validate-only is safer but makes the page a preview
   tool, not an import.
3. **Project binding of an imported session.** Recommended for the first slice: keep the
   source project when it exists locally; otherwise write a null `project` and let the
   post-rename reindex emit its `project ... left null` warning (the full-directory pull
   path, not bundle import, which resolves the key to `None` before writing). The download
   route addresses any session in the home, so the imported session
   is still downloadable even when it is not in the served project's list. If the owner wants
   every web import to appear under the served project, the core must rewrite
   `session.created.project` and record a `project-rebound` warning; that is extra work and a
   provenance decision.
4. **Download authentication.** Recommended: token-free like every other read, because the
   daemon is loopback-only and the event API already exposes the content. If session
   content is considered more sensitive than the timeline, make the download token-gated and
   use a fetch/blob UI instead of a plain link.
5. **VOWP strict semantics.** Recommended: strict means `Blocking`/`Integrity` only, not the
   adapter's "any loss" mode, because a normal session always has inherent omissions. Owner
   confirmation needed before the docs promise `--strict`.
6. **Remote full report.** Recommended: the hidden export stream carries only the summary;
   `--report` with `--node` is refused. If users need the full remote report, add a second
   stream member or a remote report file transfer later.
7. **Web size cap.** Recommended: the adapter's 272 MiB archive cap. A 1 GiB session
   directory can still fail ASIF export because resources exceed 256 MiB; the error is a
   path-free `413`. A lower web cap (for example 64 MiB) is a product choice.
8. **Redaction on download.** Recommended: safe defaults only; no browser-provided redaction
   list. Secrets should not be typed into a web form, and the adapter's redaction is
   exact-literal, not discovery.
9. **ASIF wording.** Recommended: always "ASIF 0.4 (review draft)" and "semantic
   interchange with declared losses"; never "ASIF-compatible" without the caveat, never
   "lossless", "continue on another tool" or "native import".
10. **Version 0.4 placement.** This plan lives under `docs/design/v0.4/`; the implementing
    jobs may ship in the next v0.4 slice. If the owner prefers a separate version directory,
    move the doc before dispatch; the job split does not depend on it.
11. **Re-import behaviour.** Recommended: keep event identity and conflict (exit 4) when the
    same package is imported twice into one home. Regenerating event ids would make the
    import repeatable but silently break the "event ids are preserved" round-trip guarantee
    and add another loss class. The user can import into another home or remove the first
    session. Owner confirmation needed if repeatable imports are a product requirement.
12. **Feature flag and versioning.** Recommended: no `asif` Cargo feature in the first slice;
    the command is either fully present or absent, and the dependency is pinned to adapter
    `0.1.x` / ASIF `0.4`. A new ASIF draft or adapter minor is a new VOWP minor, never a
    silent reinterpretation. Revisit a default-on feature only if offline/minimal builds
    become a requirement; the adapter's registry dependency is independent of that choice.

---

## 10. Done criteria for the build

The plan is specific enough to create the jobs when:

- each build job has one owner per path and no two jobs write the same file;
- the CLI contract has exact flags, defaults, output shapes and exit codes;
- the dependency pin and its release consequence are explicit;
- the mapping and honesty limits are copied from the adapter, not invented;
- the web routes have exact methods, auth, limits, temp-file and response rules;
- every test listed can fail when the behaviour is wrong and is hermetic;
- the owner decisions above are answered or explicitly defaulted.

For this slice, A, B, C and D meet those criteria and the gates recorded in §8. The
independent verification job (E) is the open item; the crates.io release block in §4.3 is
resolved.
