//! The ASIF download and import routes.
//!
//! The download route is a token-free read like every other `GET`. It exports the
//! session through the `vowp-core` ASIF facade into a private `<home>/tmp/asif-export-*`
//! directory and streams the package in 64 KiB chunks. The import route is a
//! token-gated write: it checks the Bearer token and Content-Type before reading the
//! body, streams the raw archive into a private `0600` file under
//! `<home>/tmp/asif-upload-*`, enforces the adapter's archive cap and then hands the
//! file to the same core import the CLI uses.
//!
//! Temporary directories are removed on success, error, a five-minute timeout and when
//! the response body is dropped (including a client disconnect). Error messages are
//! path-free: they carry the adapter's stable code and explanation, never the home,
//! uploaded filename, source text, bytes or a token.
//!
//! Spec: `docs/design/v0.4/asif-integration.md` §6 and
//! `docs/design/v0.3/http-api.md` §First slice.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path as AxumPath, RawQuery, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::{Stream, StreamExt as _};
use tokio::io::{AsyncRead, AsyncWriteExt as _, ReadBuf};
use vowp_core::CoreError;
use vowp_core::session::asif::{AsifExportOptions, AsifImport, AsifImportOptions, export_asif, import_asif};
use vowp_protocol::{AsifImportResponse, AsifReport, ErrorKind, SessionId};
use vowp_store::{NodePaths, StoreError};

use crate::app::{ApiFailure, ApiState, authorize, blocking, store_failure};

/// Longest accepted upload or export run.
const ASIF_TIMEOUT: Duration = Duration::from_secs(300);

/// Chunk size used to stream an exported package.
const STREAM_CHUNK: usize = 64 * 1024;

/// `GET /api/v1/sessions/{id}/export/asif`.
///
/// `?report=1` or `?report=true` returns the `AsifReport` summary instead of the
/// package; any other `report` value is `400`. The route is token-free and addresses
/// any session in the served home, not only the served project's list.
pub(crate) async fn export_session(
    State(state): State<ApiState>,
    AxumPath(id): AxumPath<String>,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiFailure> {
    let id = SessionId::parse(&id).map_err(|_| ApiFailure::invalid("invalid session id"))?;
    let preview = report_query(query.as_deref())?;

    let checked = state.clone();
    let checked_id = id.clone();
    blocking(move || {
        let store = checked.open()?;
        if store.session(&checked_id).map_err(|_| store_failure())?.is_none() {
            return Err(ApiFailure::not_found(format!("session {checked_id} not found")));
        }
        Ok(())
    })
    .await?;

    let paths = state.paths().clone();
    let export_id = id.clone();
    let prepared = match tokio::time::timeout(
        ASIF_TIMEOUT,
        tokio::task::spawn_blocking(move || prepare_export(&paths, &export_id, preview)),
    )
    .await
    {
        Ok(Ok(result)) => result?,
        Ok(Err(_)) => return Err(internal("the ASIF export failed")),
        Err(_) => {
            return Err(ApiFailure::new(
                StatusCode::REQUEST_TIMEOUT,
                ErrorKind::Timeout,
                "the ASIF export timed out",
            ));
        }
    };

    match prepared {
        PreparedExport::Report(report) => Ok(Json(report).into_response()),
        PreparedExport::Package {
            report,
            bytes,
            file,
            directory,
        } => Ok(package_response(&id, report, bytes, file, directory)),
    }
}

/// `POST /api/v1/sessions/import/asif?branch=<id>`.
///
/// The token is checked before the body is read. The archive is streamed to a private
/// file, then imported as one new idle session in the served project's registered
/// directory. Success is `201 Created` with `AsifImportResponse`.
pub(crate) async fn import_session(
    State(state): State<ApiState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiFailure> {
    authorize(&state, &headers)?;
    if !is_zip_content_type(&headers) {
        return Err(ApiFailure::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ErrorKind::InvalidInput,
            "content type must be application/zip, application/x-zip-compressed or application/octet-stream",
        ));
    }
    let branch = branch_query(query.as_deref())?;

    let upload = TempWorkDir::new(state.paths(), "asif-upload")?;
    let path = upload.path().join("session.asif.zip");
    let limit = state.upload_limit();
    match tokio::time::timeout(ASIF_TIMEOUT, receive_upload(body, &path, limit)).await {
        Ok(Ok(_bytes)) => {}
        Ok(Err(error)) => return Err(error),
        Err(_) => {
            return Err(ApiFailure::new(
                StatusCode::REQUEST_TIMEOUT,
                ErrorKind::Timeout,
                "the ASIF upload timed out",
            ));
        }
    }

    let import_state = state.clone();
    let input = path.clone();
    let imported = match tokio::time::timeout(
        ASIF_TIMEOUT,
        tokio::task::spawn_blocking(move || import_upload(&import_state, &input, branch)),
    )
    .await
    {
        Ok(Ok(result)) => result?,
        Ok(Err(_)) => return Err(internal("the ASIF import failed")),
        Err(_) => {
            return Err(ApiFailure::new(
                StatusCode::REQUEST_TIMEOUT,
                ErrorKind::Timeout,
                "the ASIF import timed out",
            ));
        }
    };

    // The core import has finished reading the archive; dropping the guard removes the
    // upload directory before the response is sent.
    drop(upload);
    Ok(import_response(imported))
}

/// Reports the loss counts from the session detail page's preview button.
fn report_query(raw: Option<&str>) -> Result<bool, ApiFailure> {
    let pairs = query_pairs(raw);
    match value(&pairs, "report") {
        None => Ok(false),
        Some("1") | Some("true") => Ok(true),
        Some(other) => Err(ApiFailure::invalid(format!("report must be 1 or true, not {other:?}"))),
    }
}

/// Returns the optional branch name from `?branch=`.
fn branch_query(raw: Option<&str>) -> Result<Option<String>, ApiFailure> {
    let pairs = query_pairs(raw);
    match value(&pairs, "branch").map(str::trim) {
        None | Some("") => Ok(None),
        Some(branch) => Ok(Some(branch.to_owned())),
    }
}

/// One prepared export: either the package stream or the report preview.
enum PreparedExport {
    /// The `AsifReport` summary.
    Report(AsifReport),
    /// The package file and the private directory that owns it.
    Package {
        /// Adapter-free summary used for the loss headers.
        report: AsifReport,
        /// Exact package length.
        bytes: u64,
        /// Open package file moved into the response body.
        file: tokio::fs::File,
        /// Directory removed when the response body is dropped.
        directory: TempWorkDir,
    },
}

/// Exports one session inside a blocking task and opens the package for streaming.
fn prepare_export(paths: &NodePaths, id: &SessionId, preview: bool) -> Result<PreparedExport, ApiFailure> {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = TempWorkDir::new(paths, "asif-export")?;
    let package = directory.path().join("session.asif.zip");
    let exported = export_asif(paths, id, &package, &AsifExportOptions::default()).map_err(export_failure)?;
    std::fs::set_permissions(&package, std::fs::Permissions::from_mode(0o600))
        .map_err(|_| internal("the ASIF export could not be secured"))?;
    if preview {
        return Ok(PreparedExport::Report(exported.report));
    }
    let file = std::fs::File::open(&package).map_err(|_| internal("the ASIF export could not be read"))?;
    Ok(PreparedExport::Package {
        report: exported.report,
        bytes: exported.bytes,
        file: tokio::fs::File::from_std(file),
        directory,
    })
}

/// Builds the streaming download response with the documented headers.
fn package_response(
    id: &SessionId,
    report: AsifReport,
    bytes: u64,
    file: tokio::fs::File,
    directory: TempWorkDir,
) -> Response {
    let body = Body::from_stream(PackageStream {
        file,
        _directory: directory,
        buffer: vec![0; STREAM_CHUNK],
    });
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/zip"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{id}.asif.zip\"")) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    if let Ok(value) = HeaderValue::from_str(&bytes.to_string()) {
        headers.insert(header::CONTENT_LENGTH, value);
    }
    if let Ok(value) = HeaderValue::from_str(&report.loss_count.to_string()) {
        headers.insert(HeaderName::from_static("x-vowp-asif-losses"), value);
    }
    if let Ok(value) = HeaderValue::from_str(&report.unsupported.len().to_string()) {
        headers.insert(HeaderName::from_static("x-vowp-asif-unsupported"), value);
    }
    response
}

/// Streams one exported package in bounded chunks and removes its directory on drop.
struct PackageStream {
    file: tokio::fs::File,
    _directory: TempWorkDir,
    buffer: Vec<u8>,
}

impl Stream for PackageStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let Self { file, buffer, .. } = self.get_mut();
        let mut read = ReadBuf::new(buffer);
        match Pin::new(file).poll_read(cx, &mut read) {
            Poll::Ready(Ok(())) => {
                let filled = read.filled();
                if filled.is_empty() {
                    Poll::Ready(None)
                } else {
                    Poll::Ready(Some(Ok(Bytes::copy_from_slice(filled))))
                }
            }
            Poll::Ready(Err(error)) => Poll::Ready(Some(Err(error))),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Streams the raw request body into a private file, refusing more than `limit` bytes.
async fn receive_upload(body: Body, path: &Path, limit: u64) -> Result<u64, ApiFailure> {
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .await
        .map_err(|_| internal("the ASIF upload could not be stored"))?;
    let mut stream = body.into_data_stream();
    let mut total = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| ApiFailure::invalid("the ASIF upload could not be read"))?;
        total = total.checked_add(chunk.len() as u64).ok_or_else(|| too_large(limit))?;
        if total > limit {
            return Err(too_large(limit));
        }
        file.write_all(&chunk)
            .await
            .map_err(|_| internal("the ASIF upload could not be stored"))?;
    }
    file.flush()
        .await
        .map_err(|_| internal("the ASIF upload could not be stored"))?;
    file.sync_all()
        .await
        .map_err(|_| internal("the ASIF upload could not be stored"))?;
    drop(file);
    Ok(total)
}

/// Runs the core import against the served project's registered path.
fn import_upload(state: &ApiState, input: &Path, branch: Option<String>) -> Result<AsifImport, ApiFailure> {
    let store = state.open_writable()?;
    let project = store
        .project(&state.project_name())
        .map_err(|_| store_failure())?
        .ok_or_else(|| ApiFailure::conflict("the served project is not registered"))?;
    if project.path.trim().is_empty() {
        return Err(ApiFailure::conflict(
            "the served project has no registered working directory",
        ));
    }
    let options = AsifImportOptions {
        branch,
        ..AsifImportOptions::new(PathBuf::from(project.path))
    };
    let paths = state.paths().clone();
    import_asif(&paths, &store, input, &options).map_err(import_failure)
}

/// Builds the `201 Created` response with the session location and loss header.
fn import_response(imported: AsifImport) -> Response {
    let location = format!("/api/v1/sessions/{}", imported.session.id);
    let losses = imported.report.loss_count.to_string();
    let body = Json(AsifImportResponse {
        session: imported.session,
        report: imported.report,
        warnings: imported.warnings,
    });
    let mut response = (StatusCode::CREATED, body).into_response();
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&location) {
        headers.insert(header::LOCATION, value);
    }
    if let Ok(value) = HeaderValue::from_str(&losses) {
        headers.insert(HeaderName::from_static("x-vowp-asif-losses"), value);
    }
    response
}

/// A private directory under `<home>/tmp` removed when this value is dropped.
struct TempWorkDir {
    path: PathBuf,
}

impl TempWorkDir {
    /// Creates `<home>/tmp/<prefix>-<ulid>` with mode `0700`.
    fn new(paths: &NodePaths, prefix: &str) -> Result<Self, ApiFailure> {
        use std::os::unix::fs::DirBuilderExt as _;

        let tmp = paths.tmp_dir();
        std::fs::create_dir_all(&tmp).map_err(|_| internal("the ASIF temporary directory could not be created"))?;
        let path = tmp.join(format!("{prefix}-{}", SessionId::new()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|_| internal("the ASIF temporary directory could not be created"))?;
        Ok(Self { path })
    }

    /// Returns the private directory.
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempWorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Maps a failed export onto a path-free status.
///
/// A stored session that cannot be exported (torn log, corrupt capsule) is a `409`,
/// as the integration design requires, not a caller-input `400`. Adapter limits are
/// `413`, an unknown directory is `404` and every internal failure is a generic `500`.
fn export_failure(error: CoreError) -> ApiFailure {
    match error {
        CoreError::Asif { code, message, class } => {
            if limit_code(&code) {
                return too_large_message(&code, &message);
            }
            match class {
                ErrorKind::NotFound => ApiFailure::not_found(format!("asif {code}: {message}")),
                ErrorKind::Conflict | ErrorKind::InvalidInput | ErrorKind::Refused => {
                    ApiFailure::conflict(format!("asif {code}: {message}"))
                }
                _ => internal("the ASIF export failed"),
            }
        }
        CoreError::Store(StoreError::NotFound { .. }) => ApiFailure::not_found("session not found"),
        other => match other.kind() {
            ErrorKind::NotFound => ApiFailure::not_found("session not found"),
            ErrorKind::Conflict => ApiFailure::conflict("the ASIF export conflicted with the stored session"),
            _ => internal("the ASIF export failed"),
        },
    }
}

/// Maps a failed import onto a path-free status.
fn import_failure(error: CoreError) -> ApiFailure {
    match error {
        CoreError::Asif { code, message, class } => {
            if limit_code(&code) {
                return too_large_message(&code, &message);
            }
            match class {
                ErrorKind::Conflict => ApiFailure::conflict(format!("asif {code}: {message}")),
                ErrorKind::NotFound => ApiFailure::not_found(format!("asif {code}: {message}")),
                ErrorKind::InvalidInput | ErrorKind::Refused => ApiFailure::invalid(format!("asif {code}: {message}")),
                _ => internal("the ASIF import failed"),
            }
        }
        CoreError::Store(StoreError::Conflict(_)) => {
            ApiFailure::conflict("the ASIF import conflicted with an existing session")
        }
        other => match other.kind() {
            ErrorKind::Conflict => ApiFailure::conflict("the ASIF import conflicted with an existing session"),
            ErrorKind::InvalidInput => ApiFailure::invalid("the ASIF package is invalid"),
            _ => internal("the ASIF import failed"),
        },
    }
}

/// True for the adapter's archive, member, collection and JSON size limits.
fn limit_code(code: &str) -> bool {
    code.ends_with("_TOO_LARGE")
        || code == "PACKAGE_MEMBER_LIMIT"
        || code == "ASIF_COLLECTION_LIMIT"
        || code == "ASIF_JSON_TOO_LARGE"
}

/// A `413` that names the archive limit without echoing a path.
fn too_large(limit: u64) -> ApiFailure {
    ApiFailure::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        ErrorKind::InvalidInput,
        format!("the ASIF upload exceeds the {limit}-byte archive limit"),
    )
}

/// A `413` that carries the adapter's stable code and path-free limit message.
fn too_large_message(code: &str, message: &str) -> ApiFailure {
    ApiFailure::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        ErrorKind::InvalidInput,
        format!("asif {code}: {message}"),
    )
}

/// A generic path-free `500`.
fn internal(message: &'static str) -> ApiFailure {
    ApiFailure::new(StatusCode::INTERNAL_SERVER_ERROR, ErrorKind::Internal, message)
}

/// Accepts exactly the three documented zip content types, with optional parameters.
fn is_zip_content_type(headers: &HeaderMap) -> bool {
    let Some(value) = headers.get(header::CONTENT_TYPE).and_then(|value| value.to_str().ok()) else {
        return false;
    };
    let essence = value.split(';').next().unwrap_or_default().trim();
    matches!(
        essence.to_ascii_lowercase().as_str(),
        "application/zip" | "application/x-zip-compressed" | "application/octet-stream"
    )
}

/// Splits a raw query string into decoded key/value pairs.
fn query_pairs(raw: Option<&str>) -> Vec<(String, String)> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    raw.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((key, value)) => (decode(key), decode(value)),
            None => (decode(pair), String::new()),
        })
        .collect()
}

/// Returns the last value for `name`, matching the other query parsers.
fn value<'a>(pairs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    pairs
        .iter()
        .rev()
        .find(|(key, _)| key.as_str() == name)
        .map(|(_, value)| value.as_str())
}

/// Decodes one `application/x-www-form-urlencoded` component.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => match (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                (Some(high), Some(low)) => {
                    out.push(high * 16 + low);
                    index += 3;
                }
                _ => {
                    out.push(b'%');
                    index += 1;
                }
            },
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Returns the value of one hexadecimal digit.
fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
