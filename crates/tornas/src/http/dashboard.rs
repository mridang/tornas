//! The single-page dashboard at `/`, served from a compiled-in HTML file with a
//! strict Content-Security-Policy.

use axum::{http::header, response::IntoResponse};

/// The dashboard: a single self-contained page that talks to the JSON API. No
/// external scripts, fonts or styles, so it works on a box with no internet. The
/// CSP limits it to this origin plus TMDB poster images.
pub(super) async fn index() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'self'; img-src 'self' https://image.tmdb.org data:; \
                 style-src 'self' 'unsafe-inline'; script-src 'self' 'unsafe-inline'; \
                 connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
            ),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        include_str!("ui.html"),
    )
}
