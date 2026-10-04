//! Parsing the `{extra}` path segment.
//!
//! Stremio puts catalogue arguments in the *path*, shaped like a query string:
//!
//! ```text
//! /catalog/movie/local/search=blade%20runner&skip=100.json
//! ```
//!
//! The subtlety, which the JavaScript SDK also works around: the segment must be
//! split on `&` and `=` **before** percent-decoding. Decoding first turns an
//! encoded `%26` inside a value into a real `&` and splits one argument into two —
//! so a search for "Tom & Jerry" would arrive mangled.

/// The extras that came with a request, in the order they were sent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Extra(Vec<(String, String)>);

impl Extra {
    /// Parse a raw, still-encoded path segment. Pass it straight from the router
    /// without decoding.
    pub fn parse(raw: &str) -> Self {
        let raw = raw.strip_suffix(".json").unwrap_or(raw);
        let mut out = Vec::new();
        for pair in raw.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            out.push((decode(k), decode(v)));
        }
        Self(out)
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// What the user typed into the search box.
    pub fn search(&self) -> Option<&str> {
        self.get("search").filter(|s| !s.is_empty())
    }

    /// The genre filter chosen from the manifest's declared options.
    pub fn genre(&self) -> Option<&str> {
        self.get("genre").filter(|s| !s.is_empty())
    }

    /// How many entries to skip. Stremio pages in hundreds, and treats a page
    /// shorter than it asked for as the end of the catalogue.
    pub fn skip(&self) -> Option<usize> {
        self.get("skip")?.parse().ok()
    }
}

/// Percent-decoding, applied per key and per value once they are already split.
/// `+` is left alone: this is a path segment, not a form body. Also used directly by
/// the router to decode non-extra path segments.
pub(crate) fn decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_documented_extras() {
        let e = Extra::parse("search=blade%20runner&skip=100.json");
        assert_eq!(e.search(), Some("blade runner"));
        assert_eq!(e.skip(), Some(100));
        assert_eq!(e.genre(), None);
    }

    #[test]
    fn an_encoded_ampersand_stays_inside_its_value() {
        // Decoding before splitting would turn this into two arguments and lose
        // half the search term.
        let e = Extra::parse("search=Tom%20%26%20Jerry&skip=0");
        assert_eq!(e.search(), Some("Tom & Jerry"));
        assert_eq!(e.skip(), Some(0));
        assert_eq!(e.iter().count(), 2);
    }

    #[test]
    fn an_encoded_equals_stays_inside_its_value() {
        let e = Extra::parse("search=a%3Db");
        assert_eq!(e.search(), Some("a=b"));
    }

    #[test]
    fn handles_empty_and_malformed_segments() {
        assert!(Extra::parse("").is_empty());
        assert!(Extra::parse(".json").is_empty());
        assert_eq!(Extra::parse("genre").get("genre"), Some(""));
        assert_eq!(Extra::parse("&&genre=Drama&&").genre(), Some("Drama"));
        // A stray percent is data, not an error.
        assert_eq!(Extra::parse("search=100%").search(), Some("100%"));
    }

    #[test]
    fn keys_are_decoded_too() {
        assert_eq!(Extra::parse("sear%63h=abc").search(), Some("abc"));
    }

    #[test]
    fn utf8_survives_decoding() {
        assert_eq!(Extra::parse("search=Am%C3%A9lie").search(), Some("Amélie"));
    }
}
