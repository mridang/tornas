//! The HTTP metrics layer: count requests and time them by route *template* (never
//! the raw path, which would be unbounded).
//!
//! Its two OpenTelemetry instruments live here, not in the central `metrics` module,
//! because this layer is the only thing that records them. They are built from the
//! same global `tornas` meter, so they register into the provider `/metrics` scrapes
//! exactly as if they had been declared centrally.

use std::{sync::OnceLock, time::Instant};

use opentelemetry::{
    KeyValue,
    metrics::{Counter, Histogram},
};

struct HttpInstruments {
    requests: Counter<u64>,
    duration: Histogram<f64>,
}

fn instruments() -> &'static HttpInstruments {
    static I: OnceLock<HttpInstruments> = OnceLock::new();
    I.get_or_init(|| {
        let m = opentelemetry::global::meter("tornas");
        HttpInstruments {
            requests: m
                .u64_counter("tornas_http_requests_total")
                .with_description("HTTP requests served, by route template, method and status")
                .build(),
            duration: m
                .f64_histogram("tornas_http_request_duration_seconds")
                .with_description(
                    "Time until response headers, by route template. For video this is time to \
                     first byte, not the whole stream.",
                )
                .build(),
        }
    })
}

pub(in crate::http) async fn track_http(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let route = req
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|m| m.as_str().to_owned())
        .unwrap_or_else(|| "unmatched".to_owned());
    let method: &'static str = match req.method().as_str() {
        "GET" => "GET",
        "HEAD" => "HEAD",
        "POST" => "POST",
        "PUT" => "PUT",
        "PATCH" => "PATCH",
        "DELETE" => "DELETE",
        "OPTIONS" => "OPTIONS",
        _ => "OTHER",
    };
    let start = Instant::now();
    let resp = next.run(req).await;
    let status = resp.status().as_u16().to_string();
    let i = instruments();
    i.requests.add(
        1,
        &[
            KeyValue::new("route", route.clone()),
            KeyValue::new("method", method),
            KeyValue::new("status", status),
        ],
    );
    i.duration.record(
        start.elapsed().as_secs_f64(),
        &[KeyValue::new("route", route)],
    );
    resp
}
