//! Swagger UI docs page, served at `/api/docs`.
//!
//! Loads `swagger-ui-bundle.js` + `swagger-ui.css` (vendored at
//! `vendor/swagger-ui/`, pinned to swagger-ui-dist v5.33.3) and points it
//! at the engine's own `/api/openapi.json`. Assets are embedded into the
//! binary via `include_dir!` so the page works on air-gapped boxes —
//! no CDN, no runtime file dependency.
//!
//! Route layout:
//!   GET /api/docs                  → 200 with the HTML host page
//!   GET /api/docs/                 → 301 redirect to /api/docs
//!   GET /api/docs/static/{file}    → asset bytes (CSS/JS) with appropriate
//!                                    content-type and a long cache header.

use axum::{
    Router,
    extract::Path,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
    routing::get,
};
use include_dir::{Dir, include_dir};

static SWAGGER_UI_DIST: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/../../vendor/swagger-ui");

/// Minimal docs host page. Pinned `SwaggerUIBundle` config matches what
/// every Swagger UI deployment uses: deep linking on, persisted auth across
/// reloads (so the `nasty_session` cookie stays usable), and the spec URL
/// pointed at this engine's own endpoint.
const DOCS_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <title>NASty API</title>
    <link rel="stylesheet" href="/api/docs/static/swagger-ui.css">
    <style>body { margin: 0; background: #fafbfc; }</style>
</head>
<body>
    <div id="swagger-ui"></div>
    <script src="/api/docs/static/swagger-ui-bundle.js"></script>
    <script>
        window.onload = () => {
            window.ui = SwaggerUIBundle({
                url: '/api/openapi.json',
                dom_id: '#swagger-ui',
                deepLinking: true,
                persistAuthorization: true
            });
        };
    </script>
</body>
</html>
"#;

pub fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/api/docs", get(docs_handler))
        // Trailing-slash variant — common typo, just redirect rather than 404.
        .route(
            "/api/docs/",
            get(|| async { Redirect::permanent("/api/docs") }),
        )
        .route("/api/docs/static/{file}", get(static_handler))
}

async fn docs_handler() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )],
        DOCS_HTML,
    )
}

async fn static_handler(Path(file): Path<String>) -> Response {
    // Reject any path component that tries to escape the vendor dir.
    // include_dir doesn't follow `..`, but the file param could otherwise
    // contain slashes and produce surprising lookups.
    if file.contains('/') || file.contains("..") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(asset) = SWAGGER_UI_DIST.get_file(&file) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let content_type = content_type_for(&file);
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
            // Long cache — assets are version-pinned at compile time and the
            // filename doesn't change across rebuilds of the same engine
            // version, but a fresh build embeds new bytes so a cache bust
            // on engine upgrade is acceptable.
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=3600"),
            ),
        ],
        asset.contents(),
    )
        .into_response()
}

fn content_type_for(name: &str) -> &'static str {
    if name.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if name.ends_with(".js") {
        "application/javascript; charset=utf-8"
    } else if name.ends_with(".png") {
        "image/png"
    } else if name.ends_with(".html") {
        "text/html; charset=utf-8"
    } else {
        "application/octet-stream"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendored_assets_are_embedded() {
        // Smoke test: confirm the include_dir! macro picked up the vendored
        // bundle. Catches a rebase that accidentally deletes vendor/swagger-ui/.
        assert!(SWAGGER_UI_DIST.get_file("swagger-ui.css").is_some());
        assert!(SWAGGER_UI_DIST.get_file("swagger-ui-bundle.js").is_some());
    }

    #[test]
    fn content_type_dispatch() {
        assert_eq!(
            content_type_for("swagger-ui.css"),
            "text/css; charset=utf-8"
        );
        assert_eq!(
            content_type_for("swagger-ui-bundle.js"),
            "application/javascript; charset=utf-8"
        );
        assert_eq!(content_type_for("favicon-32x32.png"), "image/png");
        assert_eq!(content_type_for("unknown.bin"), "application/octet-stream");
    }
}
