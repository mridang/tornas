//! The movie library, seen as something a TV can browse.

use crate::{
    catalog::Catalog,
    dlna::{Browsable, MediaItem},
};

impl Browsable for Catalog {
    fn folder_name(&self) -> String {
        "Movies".to_owned()
    }

    fn items(&self) -> Vec<MediaItem> {
        let mut movies = self.list_movies().unwrap_or_default();
        movies.sort_by(|a, b| a.title.cmp(&b.title));
        movies
            .into_iter()
            .filter_map(|m| {
                // A movie with no torrent row has nothing to play yet.
                let t = self.torrent_for_movie(&m.imdb_id).ok().flatten()?;
                Some(MediaItem {
                    title: match m.year {
                        Some(y) => format!("{} ({y})", m.title),
                        None => m.title.clone(),
                    },
                    path: format!(
                        "/video/{}/{}",
                        m.imdb_id,
                        url::form_urlencoded::byte_serialize(t.video_file_name.as_bytes())
                            .collect::<String>()
                            .replace('+', "%20")
                    ),
                    id: m.imdb_id,
                    size_bytes: t.size_bytes,
                    mime: mime_guess::from_path(&t.video_file_name).first(),
                })
            })
            .collect()
    }
}
