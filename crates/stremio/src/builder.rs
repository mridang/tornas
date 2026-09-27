//! Assembling an addon, and deriving its manifest from what was declared.
//!
//! The manifest's `resources` array is generated from what you declare — a catalogue
//! list makes it a catalogue addon, meta/stream types add those resources — so it
//! can only advertise what it was set up to answer. The handler (one object
//! implementing [`Handler`]) is attached last, at [`AddonBuilder::build`].
//!
//! ```ignore
//! let addon = AddonBuilder::new("org.example.movies", "Movies", "1.0.0")
//!     .description("What is on the box")
//!     .id_prefixes(["tt"])
//!     .catalogs([CatalogDef::new(ContentType::Movie, "local", "Local")])
//!     .stream([ContentType::Movie])
//!     .build(library)?;                // resources: ["catalog", "stream"]
//! ```

use super::handler::Handler;
use super::model::{CatalogDef, ContentType, Manifest, Resource, ResourceEntry};

/// A finished addon: the manifest plus the one handler behind it.
pub struct Addon<H> {
    pub(super) manifest: Manifest,
    pub(super) handler: H,
}

impl<H> Addon<H> {
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
}

#[derive(Debug)]
pub enum BuildError {
    /// id, name, description or version was empty.
    MissingField(&'static str),
    /// No `types` declared, but resources were registered.
    NoTypes,
    /// A catalogue declares a type the addon does not serve.
    UnknownCatalogType(String),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingField(n) => write!(f, "manifest field {n} is required"),
            Self::NoTypes => write!(f, "no content types declared"),
            Self::UnknownCatalogType(t) => {
                write!(f, "catalog {t} has a type this addon does not serve")
            }
        }
    }
}

impl std::error::Error for BuildError {}

/// Declares an addon's manifest; the handler is supplied to [`Self::build`].
#[derive(Default)]
pub struct AddonBuilder {
    id: String,
    name: String,
    version: String,
    description: String,
    types: Vec<ContentType>,
    id_prefixes: Vec<String>,
    catalogs: Vec<CatalogDef>,
    logo: Option<String>,
    meta_types: Vec<ContentType>,
    stream_types: Vec<ContentType>,
}

impl AddonBuilder {
    /// `id` is a reverse-DNS identity, e.g. `org.example.movies`.
    pub fn new(id: impl Into<String>, name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            version: version.into(),
            ..Self::default()
        }
    }

    pub fn description(mut self, d: impl Into<String>) -> Self {
        self.description = d.into();
        self
    }

    pub fn types(mut self, t: impl IntoIterator<Item = ContentType>) -> Self {
        self.types = t.into_iter().collect();
        self
    }

    /// Id prefixes this addon answers for, e.g. `tt` for IMDb ids. Stremio uses it
    /// to avoid asking about ids it knows we cannot serve.
    pub fn id_prefixes(mut self, p: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.id_prefixes = p.into_iter().map(Into::into).collect();
        self
    }

    pub fn logo(mut self, url: impl Into<String>) -> Self {
        self.logo = Some(url.into());
        self
    }

    /// Declare the catalogues this addon publishes.
    pub fn catalogs(mut self, defs: impl IntoIterator<Item = CatalogDef>) -> Self {
        self.catalogs = defs.into_iter().collect();
        self
    }

    /// Declare that the addon serves metadata for these types.
    pub fn meta(mut self, types: impl IntoIterator<Item = ContentType>) -> Self {
        self.meta_types = types.into_iter().collect();
        self
    }

    /// Declare that the addon serves streams for these types.
    pub fn stream(mut self, types: impl IntoIterator<Item = ContentType>) -> Self {
        self.stream_types = types.into_iter().collect();
        self
    }

    /// Finish the addon, attaching the `handler` that answers requests.
    pub fn build<H: Handler>(mut self, handler: H) -> Result<Addon<H>, BuildError> {
        for (name, v) in [
            ("id", &self.id),
            ("name", &self.name),
            ("version", &self.version),
            ("description", &self.description),
        ] {
            if v.trim().is_empty() {
                return Err(BuildError::MissingField(name));
            }
        }

        let mut resources = Vec::new();
        if !self.catalogs.is_empty() {
            resources.push(ResourceEntry::Name(Resource::Catalog));
        }
        for (resource, types) in [
            (Resource::Meta, &self.meta_types),
            (Resource::Stream, &self.stream_types),
        ] {
            if types.is_empty() {
                continue;
            }
            resources.push(ResourceEntry::Detailed {
                name: resource,
                types: types.clone(),
                id_prefixes: self.id_prefixes.clone(),
            });
        }

        // Types default to the union of what was declared, so a small addon never
        // has to say the same thing twice.
        if self.types.is_empty() {
            for t in self
                .catalogs
                .iter()
                .map(|c| c.content_type)
                .chain(self.meta_types.iter().copied())
                .chain(self.stream_types.iter().copied())
            {
                if !self.types.contains(&t) {
                    self.types.push(t);
                }
            }
        }
        if self.types.is_empty() && !resources.is_empty() {
            return Err(BuildError::NoTypes);
        }
        if let Some(c) = self
            .catalogs
            .iter()
            .find(|c| !self.types.contains(&c.content_type))
        {
            return Err(BuildError::UnknownCatalogType(c.id.clone()));
        }

        Ok(Addon {
            manifest: Manifest {
                id: self.id,
                name: self.name,
                description: self.description,
                version: self.version,
                resources,
                types: self.types,
                catalogs: self.catalogs,
                id_prefixes: self.id_prefixes,
                logo: self.logo,
            },
            handler,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::{CatalogRequest, Error, MetaRequest, Reply, StreamRequest};
    use crate::model::{CatalogResponse, MetaResponse, StreamResponse};

    struct Dummy;
    impl Handler for Dummy {
        async fn catalog(&self, _: CatalogRequest) -> Result<Reply<CatalogResponse>, Error> {
            Ok(Reply::new(CatalogResponse::default()))
        }
        async fn meta(&self, _: MetaRequest) -> Result<Reply<MetaResponse>, Error> {
            Err(Error::NotFound)
        }
        async fn stream(&self, _: StreamRequest) -> Result<Reply<StreamResponse>, Error> {
            Ok(Reply::new(StreamResponse::default()))
        }
    }

    fn base() -> AddonBuilder {
        AddonBuilder::new("org.example", "Example", "1.0.0").description("d")
    }

    #[test]
    fn resources_come_from_what_was_declared() {
        let addon = base()
            .catalogs([CatalogDef::new(ContentType::Movie, "local", "Local")])
            .stream([ContentType::Movie])
            .id_prefixes(["tt"])
            .build(Dummy)
            .unwrap();
        let v = serde_json::to_value(addon.manifest()).unwrap();
        assert_eq!(v["resources"][0], "catalog");
        assert_eq!(v["resources"][1]["name"], "stream");
        assert_eq!(v["resources"][1]["idPrefixes"][0], "tt");
        // Meta was never declared, so it is not advertised.
        assert_eq!(v["resources"].as_array().unwrap().len(), 2);
        assert_eq!(v["types"][0], "movie", "types default to what was declared");
    }

    #[test]
    fn a_missing_field_is_refused() {
        assert!(matches!(
            AddonBuilder::new("", "Example", "1.0.0")
                .description("d")
                .build(Dummy),
            Err(BuildError::MissingField("id"))
        ));
    }
}
