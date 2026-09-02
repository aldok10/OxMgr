use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256, Sha512};

use crate::daemon::http::{ENV_DASHBOARD_PASS, ENV_DASHBOARD_USER};

/// Reads dashboard credentials from the environment. Auth is enabled only when
/// the user provides both a username and a password, e.g.:
///   OXMGR_DASHBOARD_USER=admin OXMGR_DASHBOARD_PASS=s3cret
///
/// Called once at daemon startup, never per-request — the credential set must
/// be stable for the daemon's lifetime, and reading env late would make tests
/// racing on process-wide env vars flaky.
pub(crate) fn dashboard_auth_from_env() -> Option<(String, String)> {
    let user = std::env::var(ENV_DASHBOARD_USER).ok()?;
    let pass = std::env::var(ENV_DASHBOARD_PASS).ok()?;
    if user.trim().is_empty() || pass.is_empty() {
        return None;
    }
    Some((user, pass))
}

/// Verifies a password against the configured credential.
///
/// Supports multiple formats (supervisord-compatible):
/// - Plain text: `s3cret`
/// - SHA256:     `{SHA256}base64hash`
/// - SHA512:     `{SHA512}base64hash`
///
/// Generate hashes with:
/// ```sh
/// echo -n 'password' | openssl dgst -sha256 -binary | base64    # {SHA256}
/// echo -n 'password' | openssl dgst -sha512 -binary | base64    # {SHA512}
/// ```
fn verify_password(input: &str, configured: &str) -> bool {
    /// Helper to compute hash and compare with expected base64.
    fn check_hash<D: Digest>(input: &str, expected_b64: &str) -> bool {
        let mut hasher = D::new();
        hasher.update(input.as_bytes());
        let digest = hasher.finalize();
        STANDARD.encode(digest) == expected_b64
    }

    if let Some(hash) = configured.strip_prefix("{SHA256}") {
        check_hash::<Sha256>(input, hash)
    } else if let Some(hash) = configured.strip_prefix("{SHA512}") {
        check_hash::<Sha512>(input, hash)
    } else {
        // Plain text comparison
        input == configured
    }
}

/// Verifies the `Authorization: Basic <base64(user:pass)>` header. Returns
/// `true` when auth is disabled, or when the provided credentials match.
pub(super) fn auth_ok(
    headers: &std::collections::HashMap<String, String>,
    creds: &Option<(String, String)>,
) -> bool {
    let Some((expected_user, expected_pass)) = creds else {
        return true; // Auth disabled
    };

    headers
        .get("authorization")
        .and_then(|auth| {
            auth.strip_prefix("Basic ")
                .or_else(|| auth.strip_prefix("basic "))
        })
        .and_then(|encoded| STANDARD.decode(encoded.trim()).ok())
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|decoded| {
            decoded
                .split_once(':')
                .map(|(u, p)| (u.to_owned(), p.to_owned()))
        })
        .is_some_and(|(user, pass)| user == *expected_user && verify_password(&pass, expected_pass))
}

/// A `401 Unauthorized` response with a `WWW-Authenticate` challenge, used to
/// protect the dashboard and REST API when credentials are configured.
pub(super) fn unauthorized_response() -> Response {
    let mut headers = HeaderMap::new();
    // SAFETY: both literals are visible ASCII only — `from_static` is a const-time
    // construction that cannot fail for valid ASCII input (no fallible path exists).
    headers.insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"oxmgr dashboard\""),
    );
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    (StatusCode::UNAUTHORIZED, headers, "authentication required").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unauthorized_response_has_401_and_www_authenticate() {
        let resp = unauthorized_response();
        let status = resp.status();
        assert_eq!(status, StatusCode::UNAUTHORIZED, "must return 401");

        let headers = resp.headers();
        let challenge = headers
            .get(header::WWW_AUTHENTICATE)
            .expect("WWW-Authenticate header must be set");
        assert_eq!(
            challenge.to_str().unwrap(),
            "Basic realm=\"oxmgr dashboard\"",
            "challenge string must match expected value"
        );

        let content_type = headers
            .get(header::CONTENT_TYPE)
            .expect("Content-Type header must be set");
        assert_eq!(
            content_type.to_str().unwrap(),
            "text/plain; charset=utf-8",
            "must return plain text"
        );

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            body.as_ref(),
            b"authentication required",
            "body must contain the auth required message"
        );
    }
}
