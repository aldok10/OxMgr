use std::path::Path;
use std::sync::OnceLock;

use axum::extract::{Path as AxumPath, State};
use axum::http::header::{ACCEPT_ENCODING, CONTENT_TYPE, ETAG, HeaderName, IF_NONE_MATCH};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};
use tokio::fs;

use crate::daemon::http::compress::{CompressedAsset, accepts_gzip, etag_matches, gzip_bytes};
use crate::daemon::http::{
    AppState, CSP_HEADER, CSS_CONTENT_TYPE, DASHBOARD_CSS, DASHBOARD_JS_ASSETS, DASHBOARD_THEME_JS,
    HTML_CONTENT_TYPE, ICO_CONTENT_TYPE, JS_CONTENT_TYPE, JSON_CONTENT_TYPE,
    OCTET_STREAM_CONTENT_TYPE, PNG_CONTENT_TYPE, SVG_CONTENT_TYPE, TEXT_PLAIN_CONTENT_TYPE,
    WEBP_CONTENT_TYPE, WOFF2_CONTENT_TYPE, favicon_svg, render_dashboard_html,
};

/// Compressed/etagged embedded assets, computed once per build.
static DASHBOARD_CSS_ASSET: CompressedAsset = CompressedAsset::new(DASHBOARD_CSS.as_bytes());
static DASHBOARD_THEME_ASSET: CompressedAsset = CompressedAsset::new(DASHBOARD_THEME_JS.as_bytes());

/// The dashboard HTML is built at runtime (template tokens resolved against
/// constants), so it cannot be a `const` — one lazy instance, created once.
static DASHBOARD_HTML_ASSET: OnceLock<CompressedAsset> = OnceLock::new();
fn dashboard_html_asset() -> &'static CompressedAsset {
    DASHBOARD_HTML_ASSET.get_or_init(|| CompressedAsset::new(render_dashboard_html().as_bytes()))
}

/// GET / — serves the single-page dashboard HTML document, with compression
/// and ETag handling.
pub(crate) async fn get_dashboard(headers: HeaderMap) -> Response {
    let accept_encoding = headers.get(ACCEPT_ENCODING).and_then(|v| v.to_str().ok());
    let if_none_match = headers.get(IF_NONE_MATCH).and_then(|v| v.to_str().ok());

    let mut resp = dashboard_html_asset().serve(accept_encoding, if_none_match, HTML_CONTENT_TYPE);
    resp.headers_mut().insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static(CSP_HEADER),
    );
    resp
}

/// GET /favicon.svg | GET /favicon.ico — serves the favicon from the static
/// web directory when static mode is active, otherwise the bundled SVG.
pub(crate) async fn get_favicon(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(ref web_dir) = state.static_web_dir {
        let accept_encoding = headers.get(ACCEPT_ENCODING).and_then(|v| v.to_str().ok());
        let if_none_match = headers.get(IF_NONE_MATCH).and_then(|v| v.to_str().ok());
        return serve_file_from_dir(
            web_dir,
            Path::new("favicon.svg"),
            SVG_CONTENT_TYPE,
            accept_encoding,
            if_none_match,
        )
        .await;
    }
    (
        StatusCode::OK,
        [(CONTENT_TYPE, SVG_CONTENT_TYPE)],
        favicon_svg(),
    )
        .into_response()
}

/// GET /logs/:name — standalone full-page log view. Serves the same document as
/// `/`; the client boots into log-page mode from the path, so the page needs no
/// second asset and the template-token guard still covers it.
pub(crate) async fn get_log_viewer(
    headers: HeaderMap,
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> Response {
    if name.is_empty() {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    if state.snapshot.get_process(&name).await.is_none() {
        return (StatusCode::NOT_FOUND, "service not found").into_response();
    }

    let accept_encoding = headers.get(ACCEPT_ENCODING).and_then(|v| v.to_str().ok());
    let if_none_match = headers.get(IF_NONE_MATCH).and_then(|v| v.to_str().ok());

    let mut resp = dashboard_html_asset().serve(accept_encoding, if_none_match, HTML_CONTENT_TYPE);
    resp.headers_mut().insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static(CSP_HEADER),
    );
    resp
}

/// GET /dashboard.css | GET /dashboard.js — serves a static asset. Registered
/// unconditionally: the static index.html references these URLs in both modes.
/// With `OXMGR_WEB_DIR` set, the bytes come from disk (edits land without a
/// restart); otherwise the compiled-in copies are the fallback, so a missing
/// file in a configured (but dry) web dir 404s — that is an operator error
/// (unmounted volume), not a router misconfiguration.
///
/// `dashboard.js` is special: after the modular split, the on-disk
/// `dashboard.js` is an entry point that references symbols defined in the
/// `web/js/` modules. Serving that single file would ship a broken bundle, so
/// static mode assembles the modules from disk in the same order as the
/// embedded concatenation. The result is byte-identical to the embedded bundle
/// for the same build, and edits still land without a restart.
pub(crate) async fn get_asset(
    method: Method,
    headers: HeaderMap,
    State(state): State<AppState>,
    uri: Uri,
) -> Response {
    // Reached as the router's FALLBACK, so it receives every method and every
    // otherwise-unmatched path. An asset is only ever readable, and answering 404
    // rather than 405 keeps an unknown path indistinguishable whatever the verb —
    // which is what `POST /api/processes/x/explode` must still see.
    if !matches!(method, Method::GET | Method::HEAD) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }

    let file_name = uri.path().trim_start_matches('/');
    // Every extension is servable now: an unknown one becomes opaque bytes rather
    // than a 415 that would disclose the path exists. See `guess_content_type`.
    let content_type = guess_content_type(file_name);

    if let Some(ref web_dir) = state.static_web_dir {
        let accept_encoding = headers.get(ACCEPT_ENCODING).and_then(|v| v.to_str().ok());
        let if_none_match = headers.get(IF_NONE_MATCH).and_then(|v| v.to_str().ok());
        return serve_file_from_dir(
            web_dir,
            Path::new(file_name),
            content_type,
            accept_encoding,
            if_none_match,
        )
        .await;
    }

    let accept_encoding = headers.get(ACCEPT_ENCODING).and_then(|v| v.to_str().ok());
    let if_none_match = headers.get(IF_NONE_MATCH).and_then(|v| v.to_str().ok());

    // Embedded fallback — serve individual files from the precomputed map.
    if let Some(bytes) = DASHBOARD_JS_ASSETS
        .iter()
        .find_map(|(p, b)| if *p == file_name { Some(*b) } else { None })
    {
        return CompressedAsset::new(bytes).serve(accept_encoding, if_none_match, JS_CONTENT_TYPE);
    }

    match file_name {
        "dashboard.css" => {
            DASHBOARD_CSS_ASSET.serve(accept_encoding, if_none_match, CSS_CONTENT_TYPE)
        }
        "theme.js" => DASHBOARD_THEME_ASSET.serve(accept_encoding, if_none_match, JS_CONTENT_TYPE),
        _ => (StatusCode::NOT_FOUND, "asset not found").into_response(),
    }
}

/// Reads every module of the dashboard script from the web dir in bundle order
/// and concatenates them, so static serving equals the embedded bundle.
/// Per-request read keeps the edit-without-restart property. Returns 404 when
/// any module is missing — same contract as a missing single asset file.
/// Resolves `file_path` inside `web_dir` and refuses anything that escapes it.
///
/// Containment is decided on the CANONICAL path, not on the request text. Text
/// inspection cannot cover the cases that matter: a traversal can arrive
/// percent-encoded, as an absolute path, with alternate separators, or as a
/// symbolic link whose own path contains no `..` at all. Canonicalising resolves
/// every one of those to a real location, which is then required to sit under the
/// canonical web directory.
///
/// Returns `None` when the target does not exist or lies outside the directory.
/// Those two are deliberately indistinguishable to the caller: answering
/// "forbidden" for an escaping path confirms to a prober that something is there,
/// while a uniform not-found answers nothing.
///
/// This runs BEFORE any read, so a refused path is never opened.
async fn resolve_contained(web_dir: &Path, file_path: &Path) -> Option<std::path::PathBuf> {
    // An absolute or prefixed component would make `join` discard `web_dir`
    // entirely, so those are refused before resolution rather than after.
    if file_path.components().any(|c| {
        matches!(
            c,
            std::path::Component::RootDir
                | std::path::Component::Prefix(_)
                | std::path::Component::ParentDir
        )
    }) {
        return None;
    }

    let base = fs::canonicalize(web_dir).await.ok()?;
    let target = fs::canonicalize(base.join(file_path)).await.ok()?;

    // Compare resolved paths. `starts_with` on `Path` matches whole components,
    // so a sibling directory sharing a name prefix cannot pass.
    if !target.starts_with(&base) {
        return None;
    }
    if !target.is_file() {
        return None;
    }
    Some(target)
}

/// Strong validator derived from the file's CURRENT state: size and mtime,
/// hashed. Metadata is enough to answer a conditional request without opening
/// the file, which is the point — an unchanged asset costs one `stat`, not a
/// read. A content hash would also vary with edits but requires reading every
/// byte before anything can be decided.
///
/// Size is folded in deliberately: two edits inside one filesystem timestamp
/// tick that also keep the length identical are the only false-negative case,
/// and for dashboard assets an edit that preserves byte count exactly is not a
/// realistic editing pattern. The hash makes the tag opaque and stable in form
/// with the embedded assets' tags.
fn on_disk_etag(metadata: &std::fs::Metadata) -> String {
    let nanos = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    etag_from_fingerprint(format!("{}:{nanos}", metadata.len()).as_bytes())
}

/// Fallback validator when no metadata is available: derived from the bytes we
/// are about to serve anyway, so it still varies when the file changes.
fn content_etag(bytes: &[u8]) -> String {
    etag_from_fingerprint(bytes)
}

fn etag_from_fingerprint(fingerprint: &[u8]) -> String {
    let digest = Sha256::digest(fingerprint);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("\"{hex}\"")
}

/// Serves a file from the web directory with a per-file validator and optional
/// gzip, so conditional requests work in static mode exactly as they do against
/// the embedded fallback (dashboard-per-file-assets §6).
///
/// The validator is computed from file metadata BEFORE any body is read: a
/// matching `If-None-Match` is answered 304 with no body and no read at all.
/// Otherwise the bytes are read once; compression happens per request because
/// the content can change under the daemon's feet between requests.
async fn serve_file_from_dir(
    web_dir: &Path,
    file_path: &Path,
    content_type: &str,
    accept_encoding: Option<&str>,
    if_none_match: Option<&str>,
) -> Response {
    let Some(path) = resolve_contained(web_dir, file_path).await else {
        return (StatusCode::NOT_FOUND, "asset not found").into_response();
    };

    // Validator first, from state on disk right now — never cached across
    // requests, or an edit would keep serving a stale validator.
    let meta = fs::metadata(&path).await.ok().map(|m| on_disk_etag(&m));
    if meta
        .as_deref()
        .is_some_and(|etag| etag_matches(if_none_match, etag))
    {
        return (
            StatusCode::NOT_MODIFIED,
            [(ETAG, meta.as_deref().unwrap_or_default())],
        )
            .into_response();
    }

    // Per-request read: edits on disk are picked up without a daemon restart.
    match fs::read(&path).await {
        Ok(bytes) => {
            let etag = meta.unwrap_or_else(|| content_etag(&bytes));
            if accepts_gzip(accept_encoding) {
                let compressed = gzip_bytes(&bytes);
                (
                    StatusCode::OK,
                    [
                        (CONTENT_TYPE, content_type),
                        (ETAG, etag.as_str()),
                        (HeaderName::from_static("content-encoding"), "gzip"),
                    ],
                    compressed,
                )
                    .into_response()
            } else {
                (
                    StatusCode::OK,
                    [(CONTENT_TYPE, content_type), (ETAG, etag.as_str())],
                    bytes,
                )
                    .into_response()
            }
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to read asset: {err}"),
        )
            .into_response(),
    }
}

/// Maps a file extension to its content type.
///
/// An unknown extension is NOT an error. The previous version returned `None`
/// here and the caller turned that into 415, which was wrong twice under a
/// wildcard route: a font the dashboard legitimately needs would be refused, and
/// a 415 confirms to a prober that the path exists where a 404 would not. So the
/// status code answers "does this file exist" only, and an unrecognised suffix is
/// served as opaque bytes.
fn guess_content_type(filename: &str) -> &'static str {
    let ext = filename.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    match ext {
        "js" | "mjs" => JS_CONTENT_TYPE,
        "css" => CSS_CONTENT_TYPE,
        "svg" => SVG_CONTENT_TYPE,
        "html" => HTML_CONTENT_TYPE,
        "json" | "map" => JSON_CONTENT_TYPE,
        "woff2" => WOFF2_CONTENT_TYPE,
        "png" => PNG_CONTENT_TYPE,
        "webp" => WEBP_CONTENT_TYPE,
        "ico" => ICO_CONTENT_TYPE,
        "txt" => TEXT_PLAIN_CONTENT_TYPE,
        _ => OCTET_STREAM_CONTENT_TYPE,
    }
}
