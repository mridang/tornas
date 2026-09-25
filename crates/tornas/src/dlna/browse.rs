//! A UPnP ContentDirectory over any [`Browsable`] library: one flat folder whose
//! items point at URLs on this server.

use std::sync::Arc;

use upnp_serve::services::content_directory::{
    ContentDirectoryBrowseProvider,
    browse::response::{Container, Item, ItemOrContainer},
};

use super::library::Browsable;

const ROOT: usize = 0;

/// Adapts a [`Browsable`] to the `upnp-serve` provider interface.
pub struct Directory<L: Browsable> {
    library: Arc<L>,
}

impl<L: Browsable> Directory<L> {
    pub fn new(library: Arc<L>) -> Self {
        Self { library }
    }

    fn root(&self) -> ItemOrContainer {
        ItemOrContainer::Container(Container {
            id: ROOT,
            parent_id: None,
            children_count: Some(self.library.items().len()),
            title: self.library.folder_name(),
        })
    }

    /// Object ids are positions in the listing, numbered from 1 so that 0 stays the
    /// root container.
    fn entries(&self, http_host: &str) -> Vec<(usize, ItemOrContainer)> {
        self.library
            .items()
            .into_iter()
            .enumerate()
            .map(|(i, m)| {
                let id = i + 1;
                (
                    id,
                    ItemOrContainer::Item(Item {
                        id,
                        parent_id: ROOT,
                        title: m.title,
                        mime_type: m.mime,
                        url: format!("http://{http_host}{}", m.path),
                        size: m.size_bytes,
                    }),
                )
            })
            .collect()
    }
}

impl<L: Browsable> ContentDirectoryBrowseProvider for Directory<L> {
    fn browse_direct_children(&self, parent_id: usize, http_host: &str) -> Vec<ItemOrContainer> {
        if parent_id != ROOT {
            return vec![];
        }
        self.entries(http_host)
            .into_iter()
            .map(|(_, i)| i)
            .collect()
    }

    fn browse_metadata(&self, object_id: usize, http_host: &str) -> Vec<ItemOrContainer> {
        if object_id == ROOT {
            return vec![self.root()];
        }
        self.entries(http_host)
            .into_iter()
            .filter(|(id, _)| *id == object_id)
            .map(|(_, i)| i)
            .collect()
    }
}
