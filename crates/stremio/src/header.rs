//! A typed `X-Forwarded-Proto` header, so the scheme behind a reverse proxy is read
//! through the type system (`TypedHeader<ForwardedProto>`) rather than a stringly
//! `headers.get("x-forwarded-proto")` lookup.

use std::sync::OnceLock;

use axum::http::{HeaderName, HeaderValue};
use axum_extra::headers::{Error, Header};

/// The scheme the original client used, as reported by a reverse proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Http,
    Https,
}

impl Scheme {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }
}

/// The `X-Forwarded-Proto` request header. Absent means "no proxy told us"; the
/// caller decides the default (tornas assumes `http` on a LAN).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForwardedProto(pub Scheme);

impl ForwardedProto {
    pub fn scheme(self) -> Scheme {
        self.0
    }

    pub fn is_https(self) -> bool {
        self.0 == Scheme::Https
    }
}

impl Header for ForwardedProto {
    fn name() -> &'static HeaderName {
        static NAME: OnceLock<HeaderName> = OnceLock::new();
        NAME.get_or_init(|| HeaderName::from_static("x-forwarded-proto"))
    }

    fn decode<'i, I: Iterator<Item = &'i HeaderValue>>(values: &mut I) -> Result<Self, Error> {
        let value = values.next().ok_or_else(Error::invalid)?;
        let text = value.to_str().map_err(|_| Error::invalid())?;
        // A proxy chain can list several, comma-separated; the first is the scheme
        // the original client used.
        let first = text.split(',').next().unwrap_or(text).trim();
        let scheme = if first.eq_ignore_ascii_case("https") {
            Scheme::Https
        } else if first.eq_ignore_ascii_case("http") {
            Scheme::Http
        } else {
            return Err(Error::invalid());
        };
        Ok(ForwardedProto(scheme))
    }

    fn encode<E: Extend<HeaderValue>>(&self, values: &mut E) {
        values.extend(std::iter::once(HeaderValue::from_static(self.0.as_str())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(raw: &str) -> Option<ForwardedProto> {
        let value = HeaderValue::from_str(raw).unwrap();
        ForwardedProto::decode(&mut std::iter::once(&value)).ok()
    }

    #[test]
    fn reads_the_scheme_case_insensitively() {
        assert_eq!(decode("https"), Some(ForwardedProto(Scheme::Https)));
        assert_eq!(decode("HTTP"), Some(ForwardedProto(Scheme::Http)));
    }

    #[test]
    fn takes_the_first_hop_of_a_chain() {
        assert_eq!(decode("https, http"), Some(ForwardedProto(Scheme::Https)));
    }

    #[test]
    fn rejects_anything_else() {
        assert_eq!(decode("ftp"), None);
    }
}
