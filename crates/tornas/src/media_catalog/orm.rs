//! Persistence for the catalog, isolated here so all the Diesel/SQL lives in one
//! place. [`Catalog`] is the only type the rest of the crate touches; everything
//! below is an implementation detail:
//!
//! - [`schema`] — the Diesel table definitions, matching `migrations/`.
//! - [`records`] — the row structs Diesel reads/writes, and their conversion to the
//!   domain types in [`store`](super::store).
//! - `movies` / `torrents` / `events` — the queries, one file per table.
//! - `migrations/` — the embedded SQL that creates the schema on open.

mod events;
mod movies;
mod records;
mod schema;
mod torrents;

use std::path::Path;

use anyhow::Context;
use diesel::connection::SimpleConnection;
use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;
use diesel_migrations::{EmbeddedMigrations, MigrationHarness, embed_migrations};
use parking_lot::Mutex;

const MIGRATIONS: EmbeddedMigrations = embed_migrations!("src/media_catalog/orm/migrations");

/// The SQLite-backed catalog store. One connection behind a mutex — every call is a
/// few microseconds, so this never needs a pool. The query methods live in the
/// per-table sibling modules (`movies`, `torrents`, `events`).
pub struct Catalog {
    conn: Mutex<SqliteConnection>,
}

fn setup(conn: &mut SqliteConnection, pragmas: &str) -> anyhow::Result<()> {
    conn.batch_execute(pragmas)?;
    conn.run_pending_migrations(MIGRATIONS)
        .map_err(|e| anyhow::anyhow!("running catalog migrations: {e}"))?;
    Ok(())
}

impl Catalog {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let url = path.to_str().context("catalog path is not valid UTF-8")?;
        let mut conn =
            SqliteConnection::establish(url).with_context(|| format!("opening catalog {path:?}"))?;
        setup(&mut conn, "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_in_memory() -> anyhow::Result<Self> {
        let mut conn = SqliteConnection::establish(":memory:")?;
        setup(&mut conn, "PRAGMA foreign_keys=ON;")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Catalog;
    use crate::media_catalog::store::{Movie, TorrentRow};

    fn movie(id: &str, last: i64) -> Movie {
        Movie {
            imdb_id: id.into(),
            tmdb_id: Some(1),
            title: format!("Movie {id}"),
            year: Some(2020),
            overview: None,
            poster_url: None,
            backdrop_url: None,
            runtime_min: Some(90),
            genres: vec!["Drama".into()],
            rating: Some(7.5),
            added_at: last,
            last_used_at: last,
        }
    }

    #[test]
    fn roundtrip_and_cascade() {
        let c = Catalog::open_in_memory().unwrap();
        c.upsert_movie(&movie("tt1", 10), None).unwrap();
        c.insert_torrent(&TorrentRow {
            info_hash: "abc".into(),
            imdb_id: "tt1".into(),
            magnet: "magnet:?xt=urn:btih:abc".into(),
            size_bytes: 123,
            video_file_idx: 0,
            video_file_name: "a.mp4".into(),
            added_at: 10,
            private: false,
            download_limit: None,
            upload_limit: Some(1000),
            peer_limit: None,
            completed_at: None,
        })
        .unwrap();
        assert_eq!(c.list_movies().unwrap().len(), 1);
        assert_eq!(c.torrent_for_movie("tt1").unwrap().unwrap().size_bytes, 123);
        assert_eq!(
            c.torrent_for_movie("tt1").unwrap().unwrap().upload_limit,
            Some(1000)
        );
        c.set_limits("abc", Some(5), None, Some(20)).unwrap();
        let t = c.torrent_for_movie("tt1").unwrap().unwrap();
        assert_eq!(
            (t.download_limit, t.upload_limit, t.peer_limit),
            (Some(5), None, Some(20))
        );
        c.touch("tt1", 99).unwrap();
        assert_eq!(c.get_movie("tt1").unwrap().unwrap().last_used_at, 99);
        assert!(c.delete_movie("tt1").unwrap());
        assert!(c.list_torrents().unwrap().is_empty());
        c.add_event("test", "hello").unwrap();
        assert_eq!(c.recent_events(5).unwrap()[0].message, "hello");
    }

    #[test]
    fn upsert_preserves_added_at_and_refreshes_the_rest() {
        let c = Catalog::open_in_memory().unwrap();
        c.upsert_movie(&movie("tt1", 10), None).unwrap();
        let mut again = movie("tt1", 20);
        again.title = "Renamed".into();
        c.upsert_movie(&again, None).unwrap();
        let got = c.get_movie("tt1").unwrap().unwrap();
        assert_eq!(got.added_at, 10, "added_at is kept on conflict");
        assert_eq!(got.last_used_at, 20, "the rest is refreshed");
        assert_eq!(got.title, "Renamed");
    }

    #[test]
    fn reopening_an_existing_catalog_keeps_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cat.db");
        {
            let c = Catalog::open(&path).unwrap();
            c.upsert_movie(&movie("tt1", 1), None).unwrap();
        }
        let c = Catalog::open(&path).unwrap();
        assert_eq!(c.list_movies().unwrap().len(), 1);
    }
}
