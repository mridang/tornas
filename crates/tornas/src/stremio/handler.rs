//! What an addon implements.
//!
//! One trait per resource, so a type opts into exactly what it can answer and the
//! compiler checks the rest. Implementors write plain `async fn` — the traits use
//! return-position `impl Future` rather than a boxing macro.

use std::future::Future;

use super::extra::Extra;
use super::model::{
    AddonCatalogResponse, CatalogResponse, ContentType, MetaResponse, StreamResponse,
    SubtitlesResponse,
};

/// Why a request could not be answered. Everything else is the addon's own error,
/// reported to Stremio as a 500.
#[derive(Debug)]
pub enum Error {
    /// No such catalogue, id or type here. Stremio moves on to the next addon.
    NotFound,
    /// The addon broke. The message is logged, never sent to the client.
    Internal(anyhow::Error),
}

impl Error {
    pub fn internal(e: impl Into<anyhow::Error>) -> Self {
        Self::Internal(e.into())
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "not found"),
            Self::Internal(e) => write!(f, "{e:#}"),
        }
    }
}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e)
    }
}

/// A response plus its caching policy. Stremio caches aggressively when told to,
/// which matters for a box that is asleep most of the day.
#[derive(Debug, Clone)]
pub struct Reply<T> {
    pub body: T,
    /// Seconds; becomes `Cache-Control: max-age=…`.
    pub cache_max_age: Option<u32>,
    /// Seconds; `stale-while-revalidate`.
    pub stale_revalidate: Option<u32>,
    /// Seconds; `stale-if-error`.
    pub stale_error: Option<u32>,
}

impl<T> Reply<T> {
    pub fn new(body: T) -> Self {
        Self {
            body,
            cache_max_age: None,
            stale_revalidate: None,
            stale_error: None,
        }
    }

    pub fn cache_max_age(mut self, secs: u32) -> Self {
        self.cache_max_age = Some(secs);
        self
    }

    pub fn stale_revalidate(mut self, secs: u32) -> Self {
        self.stale_revalidate = Some(secs);
        self
    }

    pub fn stale_error(mut self, secs: u32) -> Self {
        self.stale_error = Some(secs);
        self
    }

    /// `Cache-Control` value, or `None` when nothing was set.
    pub fn cache_control(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(s) = self.cache_max_age {
            parts.push(format!("max-age={s}"));
        }
        if let Some(s) = self.stale_revalidate {
            parts.push(format!("stale-while-revalidate={s}"));
        }
        if let Some(s) = self.stale_error {
            parts.push(format!("stale-if-error={s}"));
        }
        (!parts.is_empty()).then(|| {
            parts.push("public".to_owned());
            parts.join(", ")
        })
    }
}

impl<T> From<T> for Reply<T> {
    fn from(body: T) -> Self {
        Self::new(body)
    }
}

/// The user-data segment of the URL, when the addon declares `config`.
pub type Config = Option<String>;

/// Where this request reached us, e.g. `http://box.lan:3030`, with no trailing
/// slash. Addons that serve their own files need it to build absolute URLs, and
/// only the transport knows it.
pub type BaseUrl = String;

#[derive(Debug, Clone)]
pub struct CatalogRequest {
    pub base_url: BaseUrl,
    pub content_type: ContentType,
    pub id: String,
    pub extra: Extra,
    pub config: Config,
}

#[derive(Debug, Clone)]
pub struct MetaRequest {
    pub base_url: BaseUrl,
    pub content_type: ContentType,
    pub id: String,
    pub config: Config,
}

#[derive(Debug, Clone)]
pub struct StreamRequest {
    pub base_url: BaseUrl,
    pub content_type: ContentType,
    /// A *video* id: the meta id for a movie, `tt123:1:2` for an episode.
    pub id: String,
    pub config: Config,
}

#[derive(Debug, Clone)]
pub struct SubtitlesRequest {
    pub base_url: BaseUrl,
    pub content_type: ContentType,
    pub id: String,
    pub extra: Extra,
    pub config: Config,
}

#[derive(Debug, Clone)]
pub struct AddonCatalogRequest {
    pub base_url: BaseUrl,
    pub content_type: ContentType,
    pub id: String,
    pub config: Config,
}

pub trait CatalogHandler: Send + Sync + 'static {
    fn catalog(
        &self,
        req: CatalogRequest,
    ) -> impl Future<Output = Result<Reply<CatalogResponse>, Error>> + Send;
}

pub trait MetaHandler: Send + Sync + 'static {
    fn meta(
        &self,
        req: MetaRequest,
    ) -> impl Future<Output = Result<Reply<MetaResponse>, Error>> + Send;
}

pub trait StreamHandler: Send + Sync + 'static {
    fn stream(
        &self,
        req: StreamRequest,
    ) -> impl Future<Output = Result<Reply<StreamResponse>, Error>> + Send;
}

pub trait SubtitlesHandler: Send + Sync + 'static {
    fn subtitles(
        &self,
        req: SubtitlesRequest,
    ) -> impl Future<Output = Result<Reply<SubtitlesResponse>, Error>> + Send;
}

pub trait AddonCatalogHandler: Send + Sync + 'static {
    fn addon_catalog(
        &self,
        req: AddonCatalogRequest,
    ) -> impl Future<Output = Result<Reply<AddonCatalogResponse>, Error>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_control_is_built_from_what_was_set() {
        let r = Reply::new(()).cache_max_age(60);
        assert_eq!(r.cache_control().as_deref(), Some("max-age=60, public"));

        let r = Reply::new(())
            .cache_max_age(60)
            .stale_revalidate(30)
            .stale_error(600);
        assert_eq!(
            r.cache_control().as_deref(),
            Some("max-age=60, stale-while-revalidate=30, stale-if-error=600, public")
        );

        assert_eq!(Reply::new(()).cache_control(), None);
    }
}
