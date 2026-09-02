use std::io::Write;
use std::sync::OnceLock;

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};

/// `If-None-Match` comparison shared by every asset path. A header may carry a
/// comma-separated list, and entries are compared after trimming optional
/// whitespace; a match means the client's copy is current.
pub(super) fn etag_matches(if_none_match: Option<&str>, etag: &str) -> bool {
    if_none_match.is_some_and(|value| value.split(',').any(|candidate| candidate.trim() == etag))
}

/// Whether the request advertises gzip via `Accept-Encoding`.
pub(super) fn accepts_gzip(accept_encoding: Option<&str>) -> bool {
    accept_encoding.is_some_and(|value| value.split(',').any(|e| e.trim() == "gzip"))
}

/// Gzips bytes at best compression. Infallible: writes go to a heap buffer.
pub(super) fn gzip_bytes(raw: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
    // SAFETY: Vec<u8> Write impl is infallible — heap buffer only,
    // no syscall or I/O path. GzEncoder::finish flushes to the same buffer.
    #[expect(
        clippy::unwrap_used,
        reason = "Vec<u8> impl of io::Write is infallible"
    )]
    {
        encoder.write_all(raw).unwrap();
        encoder.finish().unwrap()
    }
}

/// A static asset with a build-derived strong ETag and a lazily-gzip-compressed
/// twin, so every request reuses bytes computed once per build rather than
/// compressing per request (dashboard-frontend-modularization §11.3).
///
/// The ETag is a strong tag over the exact bytes served, so a rebuild that
/// changes any embedded asset changes the validator (§11.7) and a conditional
/// request with a matching `If-None-Match` is answered with 304 and no body
/// (§11.6).
pub(super) struct CompressedAsset {
    raw: &'static [u8],
    compressed: OnceLock<Vec<u8>>,
    etag: OnceLock<String>,
}

impl CompressedAsset {
    pub(super) const fn new(raw: &'static [u8]) -> Self {
        Self {
            raw,
            compressed: OnceLock::new(),
            etag: OnceLock::new(),
        }
    }

    pub(super) fn etag(&self) -> &str {
        self.etag.get_or_init(|| {
            let hash = Sha256::digest(self.raw);
            let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
            format!("\"{hex}\"")
        })
    }

    /// Serves the asset, honouring `Accept-Encoding` and `If-None-Match`.
    ///
    /// `If-None-Match` matching the current ETag wins over compression: a 304 has
    /// no body, so encoding is irrelevant. Otherwise the client gets gzip bytes
    /// when it advertises the encoding, and the raw bytes when it does not (§11.5).
    pub(super) fn serve(
        &self,
        accept_encoding: Option<&str>,
        if_none_match: Option<&str>,
        content_type: &'static str,
    ) -> Response {
        let etag = self.etag();

        if etag_matches(if_none_match, etag) {
            return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
        }

        let builder = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, content_type)
            .header(header::ETAG, etag);

        if accepts_gzip(accept_encoding) {
            let compressed = self.compressed.get_or_init(|| gzip_bytes(self.raw));
            // SAFETY: Response::builder with ASCII-only static header values
            // (CONTENT_TYPE, ETAG, CONTENT_ENCODING) cannot produce an http::Error.
            #[expect(
                clippy::unwrap_used,
                reason = "builder has ASCII-only static headers, infallible"
            )]
            {
                builder
                    .header(header::CONTENT_ENCODING, "gzip")
                    .body(compressed.clone().into())
                    .unwrap()
            }
        } else {
            // SAFETY: same as gzip branch — all header values are ASCII-only statics.
            #[expect(
                clippy::unwrap_used,
                reason = "builder has ASCII-only static headers, infallible"
            )]
            {
                builder.body(self.raw.to_vec().into()).unwrap()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn compressed_asset_serves_raw_when_no_accept_encoding() {
        let asset = CompressedAsset::new(b"hello, world");
        let resp = asset.serve(None, None, "text/plain");
        assert_eq!(resp.status(), StatusCode::OK);
        let headers = resp.headers();
        assert_eq!(
            headers.get(header::ETAG).and_then(|v| v.to_str().ok()),
            Some(asset.etag())
        );
        assert!(
            headers.get(header::CONTENT_ENCODING).is_none(),
            "no gzip without Accept-Encoding"
        );
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), b"hello, world");
    }

    #[tokio::test]
    async fn compressed_asset_gzip_when_accept_encoding_gzip() {
        use flate2::read::GzDecoder;
        use std::io::Read;
        let content = "repeat me ".repeat(100);
        let asset = CompressedAsset::new(Box::leak(
            content.clone().into_boxed_str().into_boxed_bytes(),
        ));
        let resp = asset.serve(Some("gzip"), None, "text/plain");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok()),
            Some("gzip")
        );
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let mut decoder = GzDecoder::new(&body[..]);
        let mut decompressed = String::new();
        decoder.read_to_string(&mut decompressed).unwrap();
        assert_eq!(decompressed, content);
    }

    #[tokio::test]
    async fn compressed_asset_304_when_etag_matches() {
        let asset = CompressedAsset::new(b"cached content");
        let etag = asset.etag().to_string();
        let resp = asset.serve(Some("gzip"), Some(&etag), "text/plain");
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(
            resp.headers()
                .get(header::ETAG)
                .and_then(|v| v.to_str().ok()),
            Some(etag.as_str())
        );
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body.is_empty(), "304 response must have empty body");
    }

    #[test]
    fn compressed_asset_etag_varies_by_content() {
        let a = CompressedAsset::new(b"aaa");
        let b = CompressedAsset::new(b"bbb");
        assert_ne!(
            a.etag(),
            b.etag(),
            "different content must produce different ETags"
        );
    }
}
