//! DLNA/UPnP content directory over the catalog. One flat "Movies" folder whose
//! items point at the same /video URLs Stremio uses.

use std::sync::Arc;

use upnp_serve::services::content_directory::{
    ContentDirectoryBrowseProvider,
    browse::response::{Container, Item, ItemOrContainer},
};

use crate::engine::Engine;

pub struct CatalogBrowser {
    pub engine: Arc<Engine>,
}

const ROOT: usize = 0;

impl CatalogBrowser {
    fn root(&self) -> ItemOrContainer {
        let count = self
            .engine
            .catalog
            .list_movies()
            .map(|m| m.len())
            .unwrap_or(0);
        ItemOrContainer::Container(Container {
            id: ROOT,
            parent_id: None,
            children_count: Some(count),
            title: "Movies".to_owned(),
        })
    }

    fn items(&self, http_host: &str) -> Vec<(usize, ItemOrContainer)> {
        let mut movies = self.engine.catalog.list_movies().unwrap_or_default();
        movies.sort_by(|a, b| a.title.cmp(&b.title));
        movies
            .into_iter()
            .enumerate()
            .filter_map(|(i, m)| {
                let t = self
                    .engine
                    .catalog
                    .torrent_for_movie(&m.imdb_id)
                    .ok()
                    .flatten()?;
                let id = i + 1;
                let title = match m.year {
                    Some(y) => format!("{} ({y})", m.title),
                    None => m.title.clone(),
                };
                Some((
                    id,
                    ItemOrContainer::Item(Item {
                        id,
                        parent_id: ROOT,
                        title,
                        mime_type: mime_guess::from_path(&t.video_file_name).first(),
                        url: format!(
                            "http://{http_host}/video/{}/{}",
                            m.imdb_id,
                            urlencode(&t.video_file_name)
                        ),
                        size: t.size_bytes,
                    }),
                ))
            })
            .collect()
    }
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}

impl ContentDirectoryBrowseProvider for CatalogBrowser {
    fn browse_direct_children(&self, parent_id: usize, http_host: &str) -> Vec<ItemOrContainer> {
        if parent_id != ROOT {
            return vec![];
        }
        self.items(http_host).into_iter().map(|(_, i)| i).collect()
    }

    fn browse_metadata(&self, object_id: usize, http_host: &str) -> Vec<ItemOrContainer> {
        if object_id == ROOT {
            return vec![self.root()];
        }
        self.items(http_host)
            .into_iter()
            .filter(|(id, _)| *id == object_id)
            .map(|(_, i)| i)
            .collect()
    }
}
