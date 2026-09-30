//! Glue between the product and the self-contained protocol servers.
//!
//! [`dlna`](self::dlna) and (later) the Stremio handlers live here because they are
//! the only code that knows both worlds: they read the catalog and the engine, and
//! speak the vocabulary each server defines. Keeping them here is what lets
//! `crate::dlna` and `crate::mdns` stay free of any reference to this crate.

pub mod dlna;
pub mod stremio;

use std::sync::OnceLock;

use opentelemetry::{KeyValue, metrics::Counter};

use crate::metrics::meter;

/// Stream metrics, recorded by both video routes (Stremio and DLNA), so they live
/// here rather than in either adapter. Built from the shared `tornas` meter.
struct StreamInstruments {
    streams: Counter<u64>,
    stream_bytes: Counter<u64>,
}

fn stream_instruments() -> &'static StreamInstruments {
    static I: OnceLock<StreamInstruments> = OnceLock::new();
    I.get_or_init(|| {
        let m = meter();
        StreamInstruments {
            streams: m
                .u64_counter("tornas_streams_total")
                .with_description("Video stream requests, by kind (range or full)")
                .build(),
            stream_bytes: m
                .u64_counter("tornas_stream_bytes_total")
                .with_description("Bytes requested by video stream clients")
                .build(),
        }
    })
}

/// Seed the labelled stream counters at zero so the first scrape carries them.
pub fn install() {
    let i = stream_instruments();
    for k in ["range", "full"] {
        i.streams.add(0, &[KeyValue::new("kind", k)]);
    }
    i.stream_bytes.add(0, &[]);
}

/// Count one served stream, by kind (`range` or `full`), and the bytes served.
pub fn stream(kind: &'static str, bytes: u64) {
    let i = stream_instruments();
    i.streams.add(1, &[KeyValue::new("kind", kind)]);
    i.stream_bytes.add(bytes, &[]);
}
