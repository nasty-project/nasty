//! Read-only file portal for standard users.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, LazyLock};
use std::task::{Context, Poll};

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use nasty_sharing::smb::{SmbShare, share_allows_principal};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::auth::EndpointAccess;
use crate::file_boundary::{self, BoundaryNode};
use crate::{AppState, validate_bearer};

const FILES_ROOT: &str = "/fs";
const MAX_PORTAL_ROOTS: usize = 256;
const MAX_DIRECTORY_ENTRIES: usize = 1_000;
const MAX_PATH_BYTES: usize = 4_096;
const MAX_COMPONENT_BYTES: usize = 255;
const MAX_CONCURRENT_CONTROL_OPERATIONS: usize = 32;
const MAX_CONCURRENT_DOWNLOADS: usize = 8;

static PORTAL_CONTROL_OPERATIONS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_CONCURRENT_CONTROL_OPERATIONS)));
static PORTAL_DOWNLOADS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_CONCURRENT_DOWNLOADS)));

#[derive(Serialize)]
struct PortalRoot {
    id: String,
    name: String,
}

#[derive(Serialize)]
struct PortalEntry {
    name: String,
    is_dir: bool,
    size: u64,
    modified: u64,
}

#[derive(Serialize)]
struct BrowseResult {
    path: String,
    entries: Vec<PortalEntry>,
}

#[derive(Deserialize)]
pub(crate) struct PortalPathQuery {
    share: String,
    #[serde(default)]
    path: String,
}

pub(crate) async fn roots_handler(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Response {
    let authenticated = match validate_bearer(
        &headers,
        &state.auth,
        EndpointAccess::PortalFiles,
        "user.files.roots",
    )
    .await
    {
        Ok(authenticated) => authenticated,
        Err(error) => return error.into_response(),
    };
    let _permit = match control_permit() {
        Ok(permit) => permit,
        Err(()) => return unavailable(StatusCode::TOO_MANY_REQUESTS),
    };
    let shares = match authorized_shares(&state, &authenticated.session).await {
        Ok(shares) => shares,
        Err(()) => return unavailable(StatusCode::SERVICE_UNAVAILABLE),
    };
    let roots: Vec<PortalRoot> = shares
        .into_iter()
        .map(|share| PortalRoot {
            id: share.id,
            name: share.name,
        })
        .collect();
    Json(roots).into_response()
}

pub(crate) async fn browse_handler(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    Query(query): Query<PortalPathQuery>,
) -> Response {
    let authenticated = match validate_bearer(
        &headers,
        &state.auth,
        EndpointAccess::PortalFiles,
        "user.files.browse",
    )
    .await
    {
        Ok(authenticated) => authenticated,
        Err(error) => return error.into_response(),
    };
    let _permit = match control_permit() {
        Ok(permit) => permit,
        Err(()) => return unavailable(StatusCode::TOO_MANY_REQUESTS),
    };
    let relative = match portal_relative_path(&query.path, true) {
        Ok(path) => path,
        Err(()) => return unavailable(StatusCode::NOT_FOUND),
    };
    let share = match authorized_share(&state, &authenticated.session, &query.share).await {
        Ok(share) => share,
        Err(()) => return unavailable(StatusCode::NOT_FOUND),
    };
    let display_path = query.path.clone();
    let result = tokio::task::spawn_blocking(move || browse_share(&share, &relative, display_path))
        .await
        .ok()
        .and_then(Result::ok);
    match result {
        Some(result) => Json(result).into_response(),
        None => unavailable(StatusCode::NOT_FOUND),
    }
}

pub(crate) async fn content_handler(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    Query(query): Query<PortalPathQuery>,
) -> Response {
    let authenticated = match validate_bearer(
        &headers,
        &state.auth,
        EndpointAccess::PortalFiles,
        "user.files.content",
    )
    .await
    {
        Ok(authenticated) => authenticated,
        Err(error) => return error.into_response(),
    };
    let permit = match download_permit() {
        Ok(permit) => permit,
        Err(()) => return unavailable(StatusCode::TOO_MANY_REQUESTS),
    };
    let relative = match portal_relative_path(&query.path, false) {
        Ok(path) => path,
        Err(()) => return unavailable(StatusCode::NOT_FOUND),
    };
    let disposition = match content_disposition(&relative) {
        Some(disposition) => disposition,
        None => return unavailable(StatusCode::NOT_FOUND),
    };
    let share = match authorized_share(&state, &authenticated.session, &query.share).await {
        Ok(share) => share,
        Err(()) => return unavailable(StatusCode::NOT_FOUND),
    };
    let share_id = share.id.clone();
    let audit_path = query.path.clone();
    let opened = tokio::task::spawn_blocking(move || {
        let root = file_boundary::open_root_beneath(Path::new(FILES_ROOT), Path::new(&share.path))?;
        file_boundary::open_regular_from_root(root, &relative)
    })
    .await
    .ok()
    .and_then(Result::ok);
    let file = match opened {
        Some(file) => file,
        None => return unavailable(StatusCode::NOT_FOUND),
    };
    let length = match file.metadata() {
        Ok(metadata) => metadata.len(),
        Err(_) => return unavailable(StatusCode::NOT_FOUND),
    };

    crate::auth::audit(
        "user.files.content",
        &authenticated.session.username,
        &authenticated.client_ip,
        &format!("share={share_id} path={audit_path}"),
    );

    let reader = PortalReader {
        file: tokio::fs::File::from_std(file),
        _permit: permit,
    };
    let body = axum::body::Body::from_stream(tokio_util::io::ReaderStream::new(reader));
    let mut response = body.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/octet-stream"),
    );
    response
        .headers_mut()
        .insert(header::CONTENT_DISPOSITION, disposition);
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("private, no-store"),
    );
    if let Ok(value) = axum::http::HeaderValue::from_str(&length.to_string()) {
        response.headers_mut().insert(header::CONTENT_LENGTH, value);
    }
    response
}

pub(crate) async fn content_head_handler(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    Query(query): Query<PortalPathQuery>,
) -> Response {
    let authenticated = match validate_bearer(
        &headers,
        &state.auth,
        EndpointAccess::PortalFiles,
        "user.files.content.head",
    )
    .await
    {
        Ok(authenticated) => authenticated,
        Err(error) => return error.into_response(),
    };
    let _permit = match control_permit() {
        Ok(permit) => permit,
        Err(()) => return unavailable(StatusCode::TOO_MANY_REQUESTS),
    };
    let relative = match portal_relative_path(&query.path, false) {
        Ok(path) => path,
        Err(()) => return unavailable(StatusCode::NOT_FOUND),
    };
    let disposition = match content_disposition(&relative) {
        Some(disposition) => disposition,
        None => return unavailable(StatusCode::NOT_FOUND),
    };
    let share = match authorized_share(&state, &authenticated.session, &query.share).await {
        Ok(share) => share,
        Err(()) => return unavailable(StatusCode::NOT_FOUND),
    };
    let opened = tokio::task::spawn_blocking(move || {
        let root = file_boundary::open_root_beneath(Path::new(FILES_ROOT), Path::new(&share.path))?;
        file_boundary::open_regular_from_root(root, &relative)
    })
    .await
    .ok()
    .and_then(Result::ok);
    let length = match opened.and_then(|file| file.metadata().ok().map(|metadata| metadata.len())) {
        Some(length) => length,
        None => return unavailable(StatusCode::NOT_FOUND),
    };

    let mut response = StatusCode::OK.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/octet-stream"),
    );
    response
        .headers_mut()
        .insert(header::CONTENT_DISPOSITION, disposition);
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("private, no-store"),
    );
    if let Ok(value) = axum::http::HeaderValue::from_str(&length.to_string()) {
        response.headers_mut().insert(header::CONTENT_LENGTH, value);
    }
    response
}

fn control_permit() -> Result<OwnedSemaphorePermit, ()> {
    PORTAL_CONTROL_OPERATIONS
        .clone()
        .try_acquire_owned()
        .map_err(|_| ())
}

fn download_permit() -> Result<OwnedSemaphorePermit, ()> {
    PORTAL_DOWNLOADS.clone().try_acquire_owned().map_err(|_| ())
}

async fn authorized_shares(
    state: &AppState,
    session: &crate::auth::Session,
) -> Result<Vec<SmbShare>, ()> {
    let principal = session.file_principal.as_deref().ok_or(())?;
    let authorization = state
        .smb
        .principal_authorization(principal)
        .await
        .map_err(|error| {
            tracing::warn!(
                "Portal principal lookup failed for '{}': {error}",
                session.username
            );
        })?;
    let shares = state.smb.list().await.map_err(|error| {
        tracing::warn!("Portal share lookup failed: {error}");
    })?;
    if shares.len() > MAX_PORTAL_ROOTS {
        tracing::warn!("Portal share count exceeds {MAX_PORTAL_ROOTS}");
        return Err(());
    }
    Ok(shares
        .into_iter()
        .filter(|share| {
            share_allows_principal(share, &authorization.principal, &authorization.groups)
        })
        .collect())
}

async fn authorized_share(
    state: &AppState,
    session: &crate::auth::Session,
    share_id: &str,
) -> Result<SmbShare, ()> {
    if share_id.is_empty() || share_id.len() > 128 || share_id.chars().any(char::is_control) {
        return Err(());
    }
    authorized_shares(state, session)
        .await?
        .into_iter()
        .find(|share| share.id == share_id)
        .ok_or(())
}

fn browse_share(
    share: &SmbShare,
    relative: &Path,
    display_path: String,
) -> std::io::Result<BrowseResult> {
    let root = file_boundary::open_root_beneath(Path::new(FILES_ROOT), Path::new(&share.path))?;
    let directory = file_boundary::open_directory_from_root(root, relative)?;
    let names = directory.entry_names(MAX_DIRECTORY_ENTRIES)?;
    let mut entries = Vec::with_capacity(names.len());
    for name in names {
        let Some(name_string) = supported_name(&name) else {
            continue;
        };
        let Ok(node) = directory.open_child(&name) else {
            continue;
        };
        let metadata = node.metadata()?;
        let is_dir = matches!(node, BoundaryNode::Directory(_));
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        entries.push(PortalEntry {
            name: name_string.to_string(),
            is_dir,
            size: if is_dir { 0 } else { metadata.len() },
            modified,
        });
    }
    entries.sort_by(|left, right| {
        right.is_dir.cmp(&left.is_dir).then_with(|| {
            left.name
                .to_ascii_lowercase()
                .cmp(&right.name.to_ascii_lowercase())
        })
    });
    Ok(BrowseResult {
        path: display_path,
        entries,
    })
}

fn portal_relative_path(raw: &str, allow_empty: bool) -> Result<PathBuf, ()> {
    if raw.len() > MAX_PATH_BYTES {
        return Err(());
    }
    if raw.is_empty() {
        return allow_empty.then(PathBuf::new).ok_or(());
    }
    if raw.starts_with('/') || raw.ends_with('/') || raw.contains('\\') {
        return Err(());
    }
    let mut path = PathBuf::new();
    for component in raw.split('/') {
        if component.is_empty()
            || component == "."
            || component == ".."
            || component.len() > MAX_COMPONENT_BYTES
            || component.chars().any(char::is_control)
        {
            return Err(());
        }
        path.push(component);
    }
    Ok(path)
}

fn supported_name(name: &OsStr) -> Option<&str> {
    let name = name.to_str()?;
    (!name.is_empty()
        && name.len() <= MAX_COMPONENT_BYTES
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_control))
    .then_some(name)
}

fn content_disposition(relative: &Path) -> Option<axum::http::HeaderValue> {
    let name = relative.file_name().and_then(supported_name)?;
    let mut encoded = String::with_capacity(name.len());
    for byte in name.as_bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'!' | b'#' | b'$' | b'&' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
            )
        {
            encoded.push(*byte as char);
        } else {
            use std::fmt::Write;
            write!(&mut encoded, "%{byte:02X}").ok()?;
        }
    }
    axum::http::HeaderValue::from_str(&format!("attachment; filename*=UTF-8''{encoded}")).ok()
}

fn unavailable(status: StatusCode) -> Response {
    (
        status,
        Json(serde_json::json!({"error": "Resource unavailable"})),
    )
        .into_response()
}

struct PortalReader {
    file: tokio::fs::File,
    _permit: OwnedSemaphorePermit,
}

impl AsyncRead for PortalReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.file).poll_read(cx, buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portal_path_policy_accepts_only_normal_relative_components() {
        assert_eq!(portal_relative_path("", true).unwrap(), PathBuf::new());
        assert_eq!(
            portal_relative_path("reports/2026.txt", false).unwrap(),
            PathBuf::from("reports/2026.txt")
        );
        for path in [
            "",
            "/etc/passwd",
            "../secret",
            "reports/../secret",
            "reports//secret",
            "reports/",
            "reports\\secret",
            "reports/\nsecret",
        ] {
            assert!(
                portal_relative_path(path, false).is_err(),
                "accepted {path:?}"
            );
        }
    }

    #[test]
    fn portal_path_policy_bounds_path_and_component_bytes() {
        assert!(portal_relative_path(&"a".repeat(MAX_COMPONENT_BYTES + 1), false).is_err());
        let oversized = vec!["abcd"; MAX_PATH_BYTES / 4 + 1].join("/");
        assert!(portal_relative_path(&oversized, false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn unsupported_directory_names_are_not_exposed() {
        use std::os::unix::ffi::OsStrExt;

        assert_eq!(supported_name(OsStr::new("report.txt")), Some("report.txt"));
        assert!(supported_name(OsStr::from_bytes(b"bad\nname")).is_none());
        assert!(supported_name(OsStr::from_bytes(b"bad\xffname")).is_none());
    }

    #[test]
    fn content_disposition_uses_rfc5987_encoding_for_the_final_component() {
        assert_eq!(
            content_disposition(Path::new("reports/Q3 report \"final\".txt"))
                .unwrap()
                .to_str()
                .unwrap(),
            "attachment; filename*=UTF-8''Q3%20report%20%22final%22.txt"
        );
        assert_eq!(content_disposition(Path::new("reports/caf\n.txt")), None);
    }
}
