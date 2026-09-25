//! Assembling an addon, and deriving its manifest from what was assembled.
//!
//! The manifest's `resources` array is not something you write — it is generated
//! from the handlers you register, so an addon cannot advertise a resource it
//! cannot answer. Registering a handler changes the builder's type, which is how
//! the compiler knows which resources exist.
//!
//! ```ignore
//! let addon = AddonBuilder::new("org.example.movies", "Movies", "1.0.0")
//!     .description("What is on the box")
//!     .types([ContentType::Movie])
//!     .id_prefixes(["tt"])
//!     .catalogs([CatalogDef::new(ContentType::Movie, "local", "Local")], library.clone())
//!     .stream([ContentType::Movie], library)
//!     .build()?;                       // resources: ["catalog", "stream"]
//! ```

use super::handler::{
    AddonCatalogHandler, AddonCatalogRequest, CatalogHandler, CatalogRequest, Error, MetaHandler,
    MetaRequest, Reply, StreamHandler, StreamRequest, SubtitlesHandler, SubtitlesRequest,
};
use super::model::{
    AddonCatalogResponse, BehaviorHints, CatalogDef, CatalogResponse, ConfigField, ContentType,
    Manifest, MetaResponse, Resource, ResourceEntry, StreamResponse, SubtitlesResponse,
};

/// Stands in for a resource the addon does not serve. Every request to it is a
/// protocol 404, and the manifest never mentions it.
#[derive(Debug, Clone, Copy, Default)]
pub struct Unsupported;

impl CatalogHandler for Unsupported {
    async fn catalog(&self, _: CatalogRequest) -> Result<Reply<CatalogResponse>, Error> {
        Err(Error::NotFound)
    }
}
impl MetaHandler for Unsupported {
    async fn meta(&self, _: MetaRequest) -> Result<Reply<MetaResponse>, Error> {
        Err(Error::NotFound)
    }
}
impl StreamHandler for Unsupported {
    async fn stream(&self, _: StreamRequest) -> Result<Reply<StreamResponse>, Error> {
        Err(Error::NotFound)
    }
}
impl SubtitlesHandler for Unsupported {
    async fn subtitles(&self, _: SubtitlesRequest) -> Result<Reply<SubtitlesResponse>, Error> {
        Err(Error::NotFound)
    }
}
impl AddonCatalogHandler for Unsupported {
    async fn addon_catalog(
        &self,
        _: AddonCatalogRequest,
    ) -> Result<Reply<AddonCatalogResponse>, Error> {
        Err(Error::NotFound)
    }
}

/// A finished addon: the manifest plus the handlers behind it.
pub struct Addon<
    C = Unsupported,
    M = Unsupported,
    S = Unsupported,
    Sb = Unsupported,
    Ac = Unsupported,
> {
    pub(super) manifest: Manifest,
    pub(super) catalog: C,
    pub(super) meta: M,
    pub(super) stream: S,
    pub(super) subtitles: Sb,
    pub(super) addon_catalog: Ac,
}

impl<C, M, S, Sb, Ac> Addon<C, M, S, Sb, Ac> {
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// The manifest as served under a user-data URL. Stremio refuses to install an
    /// addon that still says it needs configuring, so those hints come off once the
    /// user has configured it.
    pub fn manifest_for(&self, config: Option<&str>) -> Manifest {
        let mut m = self.manifest.clone();
        if config.is_some() {
            m.behavior_hints.configurable = false;
            m.behavior_hints.configuration_required = false;
        }
        m
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
    /// `config` fields exist but nothing marks the addon configurable, so Stremio
    /// would never show the button.
    ConfigWithoutButton,
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingField(n) => write!(f, "manifest field {n} is required"),
            Self::NoTypes => write!(f, "no content types declared"),
            Self::UnknownCatalogType(t) => {
                write!(f, "catalog {t} has a type this addon does not serve")
            }
            Self::ConfigWithoutButton => write!(
                f,
                "manifest.config is set but behaviorHints.configurable is not, so Stremio \
                 would never show the configure button"
            ),
        }
    }
}

impl std::error::Error for BuildError {}

pub struct AddonBuilder<
    C = Unsupported,
    M = Unsupported,
    S = Unsupported,
    Sb = Unsupported,
    Ac = Unsupported,
> {
    id: String,
    name: String,
    version: String,
    description: String,
    types: Vec<ContentType>,
    id_prefixes: Vec<String>,
    catalogs: Vec<CatalogDef>,
    addon_catalogs: Vec<CatalogDef>,
    config: Vec<ConfigField>,
    behavior_hints: BehaviorHints,
    logo: Option<String>,
    background: Option<String>,
    contact_email: Option<String>,
    meta_types: Vec<ContentType>,
    stream_types: Vec<ContentType>,
    subtitle_types: Vec<ContentType>,
    catalog_handler: C,
    meta_handler: M,
    stream_handler: S,
    subtitles_handler: Sb,
    addon_catalog_handler: Ac,
}

impl AddonBuilder {
    /// `id` is a reverse-DNS identity, e.g. `org.example.movies`.
    pub fn new(id: impl Into<String>, name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            version: version.into(),
            description: String::new(),
            types: Vec::new(),
            id_prefixes: Vec::new(),
            catalogs: Vec::new(),
            addon_catalogs: Vec::new(),
            config: Vec::new(),
            behavior_hints: BehaviorHints::default(),
            logo: None,
            background: None,
            contact_email: None,
            meta_types: Vec::new(),
            stream_types: Vec::new(),
            subtitle_types: Vec::new(),
            catalog_handler: Unsupported,
            meta_handler: Unsupported,
            stream_handler: Unsupported,
            subtitles_handler: Unsupported,
            addon_catalog_handler: Unsupported,
        }
    }
}

impl<C, M, S, Sb, Ac> AddonBuilder<C, M, S, Sb, Ac> {
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

    pub fn background(mut self, url: impl Into<String>) -> Self {
        self.background = Some(url.into());
        self
    }

    pub fn contact_email(mut self, email: impl Into<String>) -> Self {
        self.contact_email = Some(email.into());
        self
    }

    pub fn behavior_hints(mut self, h: BehaviorHints) -> Self {
        self.behavior_hints = h;
        self
    }

    pub fn config(mut self, fields: impl IntoIterator<Item = ConfigField>) -> Self {
        self.config = fields.into_iter().collect();
        self
    }

    /// Serve these catalogues, answered by `handler`.
    pub fn catalogs<H: CatalogHandler>(
        self,
        defs: impl IntoIterator<Item = CatalogDef>,
        handler: H,
    ) -> AddonBuilder<H, M, S, Sb, Ac> {
        AddonBuilder {
            catalogs: defs.into_iter().collect(),
            catalog_handler: handler,
            id: self.id,
            name: self.name,
            version: self.version,
            description: self.description,
            types: self.types,
            id_prefixes: self.id_prefixes,
            addon_catalogs: self.addon_catalogs,
            config: self.config,
            behavior_hints: self.behavior_hints,
            logo: self.logo,
            background: self.background,
            contact_email: self.contact_email,
            meta_types: self.meta_types,
            stream_types: self.stream_types,
            subtitle_types: self.subtitle_types,
            meta_handler: self.meta_handler,
            stream_handler: self.stream_handler,
            subtitles_handler: self.subtitles_handler,
            addon_catalog_handler: self.addon_catalog_handler,
        }
    }

    /// Serve metadata for these types.
    pub fn meta<H: MetaHandler>(
        self,
        types: impl IntoIterator<Item = ContentType>,
        handler: H,
    ) -> AddonBuilder<C, H, S, Sb, Ac> {
        AddonBuilder {
            meta_types: types.into_iter().collect(),
            meta_handler: handler,
            id: self.id,
            name: self.name,
            version: self.version,
            description: self.description,
            types: self.types,
            id_prefixes: self.id_prefixes,
            catalogs: self.catalogs,
            addon_catalogs: self.addon_catalogs,
            config: self.config,
            behavior_hints: self.behavior_hints,
            logo: self.logo,
            background: self.background,
            contact_email: self.contact_email,
            stream_types: self.stream_types,
            subtitle_types: self.subtitle_types,
            catalog_handler: self.catalog_handler,
            stream_handler: self.stream_handler,
            subtitles_handler: self.subtitles_handler,
            addon_catalog_handler: self.addon_catalog_handler,
        }
    }

    /// Serve streams for these types.
    pub fn stream<H: StreamHandler>(
        self,
        types: impl IntoIterator<Item = ContentType>,
        handler: H,
    ) -> AddonBuilder<C, M, H, Sb, Ac> {
        AddonBuilder {
            stream_types: types.into_iter().collect(),
            stream_handler: handler,
            id: self.id,
            name: self.name,
            version: self.version,
            description: self.description,
            types: self.types,
            id_prefixes: self.id_prefixes,
            catalogs: self.catalogs,
            addon_catalogs: self.addon_catalogs,
            config: self.config,
            behavior_hints: self.behavior_hints,
            logo: self.logo,
            background: self.background,
            contact_email: self.contact_email,
            meta_types: self.meta_types,
            subtitle_types: self.subtitle_types,
            catalog_handler: self.catalog_handler,
            meta_handler: self.meta_handler,
            subtitles_handler: self.subtitles_handler,
            addon_catalog_handler: self.addon_catalog_handler,
        }
    }

    /// Serve subtitles for these types.
    pub fn subtitles<H: SubtitlesHandler>(
        self,
        types: impl IntoIterator<Item = ContentType>,
        handler: H,
    ) -> AddonBuilder<C, M, S, H, Ac> {
        AddonBuilder {
            subtitle_types: types.into_iter().collect(),
            subtitles_handler: handler,
            id: self.id,
            name: self.name,
            version: self.version,
            description: self.description,
            types: self.types,
            id_prefixes: self.id_prefixes,
            catalogs: self.catalogs,
            addon_catalogs: self.addon_catalogs,
            config: self.config,
            behavior_hints: self.behavior_hints,
            logo: self.logo,
            background: self.background,
            contact_email: self.contact_email,
            meta_types: self.meta_types,
            stream_types: self.stream_types,
            catalog_handler: self.catalog_handler,
            meta_handler: self.meta_handler,
            stream_handler: self.stream_handler,
            addon_catalog_handler: self.addon_catalog_handler,
        }
    }

    /// Serve catalogues *of other addons*.
    pub fn addon_catalogs<H: AddonCatalogHandler>(
        self,
        defs: impl IntoIterator<Item = CatalogDef>,
        handler: H,
    ) -> AddonBuilder<C, M, S, Sb, H> {
        AddonBuilder {
            addon_catalogs: defs.into_iter().collect(),
            addon_catalog_handler: handler,
            id: self.id,
            name: self.name,
            version: self.version,
            description: self.description,
            types: self.types,
            id_prefixes: self.id_prefixes,
            catalogs: self.catalogs,
            config: self.config,
            behavior_hints: self.behavior_hints,
            logo: self.logo,
            background: self.background,
            contact_email: self.contact_email,
            meta_types: self.meta_types,
            stream_types: self.stream_types,
            subtitle_types: self.subtitle_types,
            catalog_handler: self.catalog_handler,
            meta_handler: self.meta_handler,
            stream_handler: self.stream_handler,
            subtitles_handler: self.subtitles_handler,
        }
    }

    pub fn build(mut self) -> Result<Addon<C, M, S, Sb, Ac>, BuildError> {
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
            (Resource::Subtitles, &self.subtitle_types),
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
        if !self.addon_catalogs.is_empty() {
            resources.push(ResourceEntry::Name(Resource::AddonCatalog));
        }

        // Types default to the union of what the handlers were registered for, so a
        // small addon never has to say the same thing twice.
        if self.types.is_empty() {
            for t in self
                .catalogs
                .iter()
                .map(|c| c.content_type)
                .chain(self.meta_types.iter().copied())
                .chain(self.stream_types.iter().copied())
                .chain(self.subtitle_types.iter().copied())
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
        let needs_button = !self.config.is_empty();
        let has_button =
            self.behavior_hints.configurable || self.behavior_hints.configuration_required;
        if needs_button && !has_button {
            return Err(BuildError::ConfigWithoutButton);
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
                addon_catalogs: self.addon_catalogs,
                config: self.config,
                background: self.background,
                logo: self.logo,
                contact_email: self.contact_email,
                behavior_hints: self.behavior_hints,
            },
            catalog: self.catalog_handler,
            meta: self.meta_handler,
            stream: self.stream_handler,
            subtitles: self.subtitles_handler,
            addon_catalog: self.addon_catalog_handler,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stremio::model::ConfigFieldType;

    struct Dummy;
    impl CatalogHandler for Dummy {
        async fn catalog(&self, _: CatalogRequest) -> Result<Reply<CatalogResponse>, Error> {
            Ok(Reply::new(CatalogResponse::default()))
        }
    }
    impl StreamHandler for Dummy {
        async fn stream(&self, _: StreamRequest) -> Result<Reply<StreamResponse>, Error> {
            Ok(Reply::new(StreamResponse::default()))
        }
    }

    fn base() -> AddonBuilder {
        AddonBuilder::new("org.example", "Example", "1.0.0").description("d")
    }

    #[test]
    fn resources_come_from_the_handlers_that_were_registered() {
        let addon = base()
            .catalogs(
                [CatalogDef::new(ContentType::Movie, "local", "Local")],
                Dummy,
            )
            .stream([ContentType::Movie], Dummy)
            .id_prefixes(["tt"])
            .build()
            .unwrap();
        let v = serde_json::to_value(addon.manifest()).unwrap();
        assert_eq!(v["resources"][0], "catalog");
        assert_eq!(v["resources"][1]["name"], "stream");
        assert_eq!(v["resources"][1]["idPrefixes"][0], "tt");
        // Never advertised: nothing implements them.
        assert_eq!(v["resources"].as_array().unwrap().len(), 2);
        assert_eq!(
            v["types"][0], "movie",
            "types default to what was registered"
        );
    }

    #[test]
    fn a_configured_url_may_be_installed() {
        let addon = base()
            .config([ConfigField {
                key: "token".into(),
                field_type: ConfigFieldType::Password,
                default: None,
                title: Some("API token".into()),
                options: vec![],
                required: true,
            }])
            .behavior_hints(BehaviorHints {
                configuration_required: true,
                ..Default::default()
            })
            .stream([ContentType::Movie], Dummy)
            .build()
            .unwrap();
        assert!(addon.manifest().behavior_hints.configuration_required);
        let configured = addon.manifest_for(Some("abc123"));
        assert!(
            !configured.behavior_hints.configuration_required,
            "a configured instance must look installable"
        );
    }

    #[test]
    fn refuses_manifests_stremio_would_reject() {
        assert!(matches!(
            AddonBuilder::new("", "Example", "1.0.0")
                .description("d")
                .build(),
            Err(BuildError::MissingField("id"))
        ));
        assert!(matches!(
            base()
                .types([ContentType::Series])
                .catalogs(
                    [CatalogDef::new(ContentType::Movie, "local", "Local")],
                    Dummy
                )
                .build(),
            Err(BuildError::UnknownCatalogType(_))
        ));
        assert!(matches!(
            base()
                .config([ConfigField {
                    key: "k".into(),
                    field_type: ConfigFieldType::Text,
                    default: None,
                    title: None,
                    options: vec![],
                    required: false,
                }])
                .stream([ContentType::Movie], Dummy)
                .build(),
            Err(BuildError::ConfigWithoutButton)
        ));
    }
}
