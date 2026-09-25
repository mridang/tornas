//! The wire types of the Stremio addon protocol.
//!
//! Field names and shapes follow the addon SDK's documented responses; anything
//! optional is skipped when absent, because Stremio treats a present `null`
//! differently from a missing key in a few places.

use serde::{Deserialize, Serialize};

/// The content types an addon can serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContentType {
    Movie,
    Series,
    Channel,
    Tv,
}

impl ContentType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Movie => "movie",
            Self::Series => "series",
            Self::Channel => "channel",
            Self::Tv => "tv",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "movie" => Some(Self::Movie),
            "series" => Some(Self::Series),
            "channel" => Some(Self::Channel),
            "tv" => Some(Self::Tv),
            _ => None,
        }
    }
}

/// A resource an addon can answer for. These are the path prefixes Stremio calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resource {
    Catalog,
    Meta,
    Stream,
    Subtitles,
    AddonCatalog,
}

impl Resource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Catalog => "catalog",
            Self::Meta => "meta",
            Self::Stream => "stream",
            Self::Subtitles => "subtitles",
            Self::AddonCatalog => "addon_catalog",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "catalog" => Some(Self::Catalog),
            "meta" => Some(Self::Meta),
            "stream" => Some(Self::Stream),
            "subtitles" => Some(Self::Subtitles),
            "addon_catalog" => Some(Self::AddonCatalog),
            _ => None,
        }
    }
}

// ---- manifest --------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub description: String,
    pub version: String,
    pub resources: Vec<ResourceEntry>,
    pub types: Vec<ContentType>,
    pub catalogs: Vec<CatalogDef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub id_prefixes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addon_catalogs: Vec<CatalogDef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config: Vec<ConfigField>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contact_email: Option<String>,
    #[serde(skip_serializing_if = "BehaviorHints::is_empty")]
    pub behavior_hints: BehaviorHints,
}

/// A manifest `resources` entry: either the bare name, or the long form that
/// narrows which types and id prefixes that resource answers for.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResourceEntry {
    Name(Resource),
    Detailed {
        name: Resource,
        types: Vec<ContentType>,
        #[serde(rename = "idPrefixes", default, skip_serializing_if = "Vec::is_empty")]
        id_prefixes: Vec<String>,
    },
}

impl ResourceEntry {
    pub fn name(&self) -> Resource {
        match self {
            Self::Name(r) => *r,
            Self::Detailed { name, .. } => *name,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogDef {
    #[serde(rename = "type")]
    pub content_type: ContentType,
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra: Vec<ExtraDef>,
}

impl CatalogDef {
    pub fn new(content_type: ContentType, id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            content_type,
            id: id.into(),
            name: name.into(),
            extra: Vec::new(),
        }
    }

    /// Declare an extra this catalog accepts. Stremio only ever sends extras that
    /// the manifest declares.
    pub fn extra(mut self, e: ExtraDef) -> Self {
        self.extra.push(e);
        self
    }
}

/// One accepted extra, e.g. `search` or `skip`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtraDef {
    pub name: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_required: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options_limit: Option<u32>,
}

impl ExtraDef {
    pub fn optional(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            is_required: false,
            options: Vec::new(),
            options_limit: None,
        }
    }

    /// A required extra makes the catalog search-only: Stremio never requests it
    /// for a feed, only when the user types something.
    pub fn required(name: impl Into<String>) -> Self {
        Self {
            is_required: true,
            ..Self::optional(name)
        }
    }

    pub fn options(mut self, options: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.options = options.into_iter().map(Into::into).collect();
        self
    }
}

/// A user-data field, rendered by Stremio on the addon's configure page.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigField {
    pub key: String,
    #[serde(rename = "type")]
    pub field_type: ConfigFieldType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigFieldType {
    Text,
    Number,
    Password,
    Checkbox,
    Select,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BehaviorHints {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub adult: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub p2p: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub configurable: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub configuration_required: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub epg_provider: bool,
}

impl BehaviorHints {
    pub fn is_empty(&self) -> bool {
        !self.adult
            && !self.p2p
            && !self.configurable
            && !self.configuration_required
            && !self.epg_provider
    }
}

// ---- catalogue and metadata ------------------------------------------------

/// A catalogue entry. `poster` is required by the protocol even though a missing
/// one only costs you a blank tile.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetaPreview {
    pub id: String,
    #[serde(rename = "type")]
    pub content_type: Option<ContentType>,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poster: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poster_shape: Option<PosterShape>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imdb_rating: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_info: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<Link>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PosterShape {
    Square,
    Poster,
    Landscape,
}

/// The full metadata object, returned by `/meta`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Meta {
    pub id: String,
    #[serde(rename = "type")]
    pub content_type: Option<ContentType>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poster: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poster_shape: Option<PosterShape>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_info: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imdb_rating: Option<String>,
    /// ISO 8601.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub released: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<Link>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub videos: Vec<Video>,
    /// Minutes, as a display string such as `"142 min"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub website: Option<String>,
    #[serde(skip_serializing_if = "MetaBehaviorHints::is_empty")]
    pub behavior_hints: MetaBehaviorHints,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetaBehaviorHints {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_video_id: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_live: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub has_scheduled_videos: bool,
}

impl MetaBehaviorHints {
    pub fn is_empty(&self) -> bool {
        self.default_video_id.is_none() && !self.is_live && !self.has_scheduled_videos
    }
}

/// A typed link shown on the detail page. `imdb`, `share` and `similar` are
/// reserved by Stremio and must not be used as categories.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Link {
    pub name: String,
    pub category: String,
    pub url: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Video {
    pub id: String,
    pub title: String,
    /// ISO 8601.
    pub released: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub streams: Vec<Stream>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub episode: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub season: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overview: Option<String>,
}

// ---- streams ---------------------------------------------------------------

/// Where the bytes come from. The protocol allows exactly one of these per stream,
/// which is why this is an enum rather than a pile of optional fields; it is
/// flattened on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StreamSource {
    #[serde(rename = "url")]
    Url(String),
    #[serde(rename = "ytId")]
    YouTube(String),
    #[serde(rename = "infoHash")]
    InfoHash(String),
    #[serde(rename = "externalUrl")]
    External(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stream {
    #[serde(flatten)]
    pub source: StreamSource,
    /// Only meaningful alongside [`StreamSource::InfoHash`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_idx: Option<usize>,
    /// Usually the quality, e.g. `"1080p"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subtitles: Vec<Subtitle>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
    #[serde(skip_serializing_if = "StreamBehaviorHints::is_empty")]
    pub behavior_hints: StreamBehaviorHints,
}

impl Stream {
    pub fn new(source: StreamSource) -> Self {
        Self {
            source,
            file_idx: None,
            name: None,
            description: None,
            subtitles: Vec::new(),
            sources: Vec::new(),
            behavior_hints: StreamBehaviorHints::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamBehaviorHints {
    /// Set when the URL is not HTTPS or not an MP4, so Stremio plays it outside
    /// the web player.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub not_web_ready: bool,
    /// Groups streams that should binge together, e.g. `"tornas-1080p"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binge_group: Option<String>,
    /// Strongly recommended with a `url` source: subtitle addons match on it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub country_whitelist: Vec<String>,
}

impl StreamBehaviorHints {
    pub fn is_empty(&self) -> bool {
        !self.not_web_ready
            && self.binge_group.is_none()
            && self.filename.is_none()
            && self.video_size.is_none()
            && self.video_hash.is_none()
            && self.country_whitelist.is_empty()
    }
}

// ---- subtitles and addon catalogs ------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Subtitle {
    pub id: String,
    pub url: String,
    /// ISO 639-2.
    pub lang: String,
    /// Overrides the displayed language name, e.g. `"English [CC]"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// An entry in a catalogue of other addons.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddonEntry {
    /// Only `"http"` is officially supported.
    pub transport_name: String,
    /// URL of that addon's `manifest.json`.
    pub transport_url: String,
    pub manifest: Manifest,
}

// ---- responses -------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogResponse {
    pub metas: Vec<MetaPreview>,
    /// Sent instead of `metas` for EPG (`date`) requests.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metas_detailed: Vec<Meta>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetaResponse {
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamResponse {
    pub streams: Vec<Stream>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubtitlesResponse {
    pub subtitles: Vec<Subtitle>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddonCatalogResponse {
    pub addons: Vec<AddonEntry>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_fields_are_omitted() {
        let m = MetaPreview {
            id: "tt0111161".into(),
            content_type: Some(ContentType::Movie),
            name: "The Shawshank Redemption".into(),
            ..Default::default()
        };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["type"], "movie");
        assert!(v.get("poster").is_none(), "absent poster must not be null");
        assert!(v.get("genres").is_none());
    }

    #[test]
    fn a_stream_carries_exactly_one_source_flattened() {
        let s = Stream {
            name: Some("Tornas".into()),
            behavior_hints: StreamBehaviorHints {
                filename: Some("shawshank.mp4".into()),
                video_size: Some(1234),
                ..Default::default()
            },
            ..Stream::new(StreamSource::Url("http://box/video/tt0111161".into()))
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["url"], "http://box/video/tt0111161");
        assert!(v.get("infoHash").is_none());
        assert_eq!(v["behaviorHints"]["filename"], "shawshank.mp4");
        assert!(v["behaviorHints"].get("notWebReady").is_none());

        let torrent = Stream {
            file_idx: Some(0),
            ..Stream::new(StreamSource::InfoHash("abc123".into()))
        };
        let v = serde_json::to_value(&torrent).unwrap();
        assert_eq!(v["infoHash"], "abc123");
        assert_eq!(v["fileIdx"], 0);
        assert!(v.get("url").is_none());
    }

    #[test]
    fn resources_serialise_short_or_long() {
        let short = serde_json::to_value(ResourceEntry::Name(Resource::Catalog)).unwrap();
        assert_eq!(short, serde_json::json!("catalog"));
        let long = serde_json::to_value(ResourceEntry::Detailed {
            name: Resource::Stream,
            types: vec![ContentType::Movie],
            id_prefixes: vec!["tt".into()],
        })
        .unwrap();
        assert_eq!(long["name"], "stream");
        assert_eq!(long["types"][0], "movie");
        assert_eq!(long["idPrefixes"][0], "tt");
    }

    #[test]
    fn behaviour_hints_disappear_when_unset() {
        let m = Manifest {
            id: "org.example".into(),
            name: "Example".into(),
            description: "d".into(),
            version: "1.0.0".into(),
            resources: vec![ResourceEntry::Name(Resource::Catalog)],
            types: vec![ContentType::Movie],
            catalogs: vec![],
            id_prefixes: vec![],
            addon_catalogs: vec![],
            config: vec![],
            background: None,
            logo: None,
            contact_email: None,
            behavior_hints: BehaviorHints::default(),
        };
        let v = serde_json::to_value(&m).unwrap();
        assert!(v.get("behaviorHints").is_none());
        assert!(v.get("idPrefixes").is_none());
    }
}
