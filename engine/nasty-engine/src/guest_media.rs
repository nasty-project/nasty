//! Prototype media access: only unlimited shares, one retained descriptor per
//! stream, and the same token/password/path boundary as ordinary downloads.
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use std::{collections::HashMap, sync::Arc};
use tokio::io::AsyncReadExt;

pub(crate) fn media_type(name: &str) -> Option<&'static str> {
    match name.rsplit('.').next()?.to_ascii_lowercase().as_str() {
        "mp4" | "m4v" => Some("video/mp4"),
        "mov" => Some("video/quicktime"),
        "webm" => Some("video/webm"),
        "mkv" => Some("video/x-matroska"),
        "mp3" => Some("audio/mpeg"),
        "m4a" => Some("audio/mp4"),
        "wav" => Some("audio/wav"),
        "ogg" | "oga" => Some("audio/ogg"),
        "flac" => Some("audio/flac"),
        "aac" => Some("audio/aac"),
        _ => None,
    }
}

/// Only single byte ranges are supported. Reject malformed/multipart or empty
/// ranges rather than accidentally sending a full large file.
pub(crate) fn byte_range(value: &str, size: u64) -> Option<(u64, u64)> {
    let (start, end) = value.strip_prefix("bytes=")?.split_once('-')?;
    if size == 0 || value.contains(',') {
        return None;
    }
    let number = |s: &str| {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            None
        } else {
            s.parse::<u64>().ok()
        }
    };
    if start.is_empty() {
        let suffix = number(end)?;
        return (suffix > 0).then_some((size.saturating_sub(suffix), size - 1));
    }
    let start = number(start)?;
    let end = if end.is_empty() {
        size - 1
    } else {
        number(end)?.min(size - 1)
    };
    (start < size && start <= end).then_some((start, end))
}

pub async fn media_handler(
    Path(token): Path<String>,
    headers: HeaderMap,
    method: Method,
    State(state): State<Arc<crate::AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let now = crate::now_unix_i64();
    let Some(share) = state.guest_shares.lookup_active(&token, now).await else {
        return crate::share_not_available();
    };
    if !crate::share_is_unlocked(&state, &share, &headers, now) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    // Range reads cannot be charged as individual downloads. Until preview
    // sessions have defined accounting, do not bypass any capped share.
    if share.max_downloads.is_some() {
        return crate::share_not_available();
    }
    let root = match params.get("root").and_then(|s| s.parse::<usize>().ok()) {
        Some(root) => root,
        None => return crate::share_not_available(),
    };
    let rel = params.get("path").map(String::as_str).unwrap_or("");
    let Some(opened) = state.guest_shares.open_media(&share, root, rel).await else {
        return crate::share_not_available();
    };
    let (mut file, name, size) = opened.into_parts();
    let Some(mime) = media_type(&name) else {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    };
    let range = match headers.get(header::RANGE) {
        Some(value) => match value.to_str().ok().and_then(|s| byte_range(s, size)) {
            Some(range) => Some(range),
            None => {
                return (
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    [(header::CONTENT_RANGE, format!("bytes */{size}"))],
                )
                    .into_response();
            }
        },
        None => None,
    };
    let (start, length) = range.map_or((0, size), |(start, end)| (start, end - start + 1));
    if file.seek_to(start).await.is_err() {
        return crate::share_not_available();
    }
    // Recheck after opening/seek, including expiry and revocation. Password
    // grants are also revalidated on each separate range request.
    let Some(current) = state
        .guest_shares
        .lookup_active(&token, crate::now_unix_i64())
        .await
    else {
        return crate::share_not_available();
    };
    if current.max_downloads.is_some()
        || current.paths != share.paths
        || !crate::share_is_unlocked(&state, &current, &headers, crate::now_unix_i64())
    {
        return crate::share_not_available();
    }
    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        Body::from_stream(tokio_util::io::ReaderStream::new(file.take(length)))
    };
    let mut response = body.into_response();
    *response.status_mut() = if range.is_some() {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let out = response.headers_mut();
    out.insert(header::CONTENT_TYPE, mime.parse().unwrap());
    out.insert(header::CONTENT_LENGTH, length.to_string().parse().unwrap());
    out.insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
    out.insert(header::CONTENT_DISPOSITION, "attachment".parse().unwrap());
    out.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    out.insert(
        header::CONTENT_SECURITY_POLICY,
        "sandbox; default-src 'none'".parse().unwrap(),
    );
    if let Some((start, end)) = range {
        out.insert(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{size}").parse().unwrap(),
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn seeks_open_suffix_and_clipped_ranges() {
        assert_eq!(byte_range("bytes=5-9", 20), Some((5, 9)));
        assert_eq!(byte_range("bytes=5-", 20), Some((5, 19)));
        assert_eq!(byte_range("bytes=-4", 20), Some((16, 19)));
        assert_eq!(byte_range("bytes=0-99", 20), Some((0, 19)));
        assert_eq!(byte_range("bytes=-99", 20), Some((0, 19)));
        for value in [
            "bytes=20-",
            "bytes=9-5",
            "bytes=-0",
            "bytes=0-1,4-5",
            "bytes=+1-2",
            "bytes=-",
            "items=0-1",
        ] {
            assert_eq!(byte_range(value, 20), None, "{value}");
        }
        assert_eq!(byte_range("bytes=0-", 0), None);
    }
    #[test]
    fn never_serve_documents_or_playlists_as_media() {
        for name in ["attack.html", "attack.svg", "playlist.m3u8", "notes.txt"] {
            assert_eq!(media_type(name), None);
        }
        assert_eq!(media_type("movie.MP4"), Some("video/mp4"));
    }
}
