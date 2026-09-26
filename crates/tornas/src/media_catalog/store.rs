//! SQLite catalog: movies with TMDB metadata, the torrent backing each movie,
//! and a short event log. One connection behind a mutex; every call is a few
//! microseconds so this never needs a pool.

use std::path::Path;

use anyhow::Context;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Movie {
    pub imdb_id: String,
    pub tmdb_id: Option<i64>,
    pub title: String,
    pub year: Option<i32>,
    pub overview: Option<String>,
    pub poster_url: Option<String>,
    pub backdrop_url: Option<String>,
    pub runtime_min: Option<i64>,
    pub genres: Vec<String>,
    pub rating: Option<f64>,
    pub added_at: i64,
    pub last_used_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentRow {
    pub info_hash: String,
    pub imdb_id: String,
    pub magnet: String,
    pub size_bytes: u64,
    pub video_file_idx: usize,
    pub video_file_name: String,
    pub added_at: i64,
    /// BEP 27 private torrent: never given public trackers.
    #[serde(default)]
    pub private: bool,
    /// Per-movie overrides, in bytes per second and peers. `None` uses the global value.
    #[serde(default)]
    pub download_limit: Option<u32>,
    #[serde(default)]
    pub upload_limit: Option<u32>,
    #[serde(default)]
    pub peer_limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub ts: i64,
    pub kind: String,
    pub message: String,
}

pub struct Catalog {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS movies (
    imdb_id      TEXT PRIMARY KEY,
    tmdb_id      INTEGER,
    title        TEXT NOT NULL,
    year         INTEGER,
    overview     TEXT,
    poster_url   TEXT,
    backdrop_url TEXT,
    runtime_min  INTEGER,
    genres       TEXT NOT NULL DEFAULT '[]',
    rating       REAL,
    tmdb_json    TEXT,
    added_at     INTEGER NOT NULL,
    last_used_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS torrents (
    info_hash       TEXT PRIMARY KEY,
    imdb_id         TEXT NOT NULL REFERENCES movies(imdb_id) ON DELETE CASCADE,
    magnet          TEXT NOT NULL,
    size_bytes      INTEGER NOT NULL,
    video_file_idx  INTEGER NOT NULL,
    video_file_name TEXT NOT NULL,
    added_at        INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS torrents_imdb ON torrents(imdb_id);
CREATE TABLE IF NOT EXISTS events (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    ts      INTEGER NOT NULL,
    kind    TEXT NOT NULL,
    message TEXT NOT NULL
);
"#;

/// Add columns introduced after the first release to existing catalogs.
fn migrate(conn: &Connection) -> anyhow::Result<()> {
    let have: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('torrents')")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for (col, ddl) in [
        (
            "private",
            "ALTER TABLE torrents ADD COLUMN private INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "download_limit",
            "ALTER TABLE torrents ADD COLUMN download_limit INTEGER",
        ),
        (
            "upload_limit",
            "ALTER TABLE torrents ADD COLUMN upload_limit INTEGER",
        ),
        (
            "peer_limit",
            "ALTER TABLE torrents ADD COLUMN peer_limit INTEGER",
        ),
    ] {
        if !have.iter().any(|c| c == col) {
            conn.execute_batch(ddl)?;
        }
    }
    Ok(())
}

impl Catalog {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening catalog {path:?}"))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn upsert_movie(&self, m: &Movie, tmdb_json: Option<&str>) -> anyhow::Result<()> {
        self.conn.lock().execute(
            "INSERT INTO movies (imdb_id, tmdb_id, title, year, overview, poster_url, backdrop_url,
                runtime_min, genres, rating, tmdb_json, added_at, last_used_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(imdb_id) DO UPDATE SET tmdb_id=excluded.tmdb_id, title=excluded.title,
                year=excluded.year, overview=excluded.overview, poster_url=excluded.poster_url,
                backdrop_url=excluded.backdrop_url, runtime_min=excluded.runtime_min,
                genres=excluded.genres, rating=excluded.rating, tmdb_json=excluded.tmdb_json,
                last_used_at=excluded.last_used_at",
            params![
                m.imdb_id,
                m.tmdb_id,
                m.title,
                m.year,
                m.overview,
                m.poster_url,
                m.backdrop_url,
                m.runtime_min,
                serde_json::to_string(&m.genres)?,
                m.rating,
                tmdb_json,
                m.added_at,
                m.last_used_at
            ],
        )?;
        Ok(())
    }

    pub fn insert_torrent(&self, t: &TorrentRow) -> anyhow::Result<()> {
        self.conn.lock().execute(
            "INSERT OR REPLACE INTO torrents (info_hash, imdb_id, magnet, size_bytes, video_file_idx,
                video_file_name, added_at, private, download_limit, upload_limit, peer_limit)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                t.info_hash,
                t.imdb_id,
                t.magnet,
                t.size_bytes as i64,
                t.video_file_idx as i64,
                t.video_file_name,
                t.added_at,
                t.private,
                t.download_limit,
                t.upload_limit,
                t.peer_limit
            ],
        )?;
        Ok(())
    }

    fn row_to_movie(r: &rusqlite::Row<'_>) -> rusqlite::Result<Movie> {
        let genres: String = r.get("genres")?;
        Ok(Movie {
            imdb_id: r.get("imdb_id")?,
            tmdb_id: r.get("tmdb_id")?,
            title: r.get("title")?,
            year: r.get("year")?,
            overview: r.get("overview")?,
            poster_url: r.get("poster_url")?,
            backdrop_url: r.get("backdrop_url")?,
            runtime_min: r.get("runtime_min")?,
            genres: serde_json::from_str(&genres).unwrap_or_default(),
            rating: r.get("rating")?,
            added_at: r.get("added_at")?,
            last_used_at: r.get("last_used_at")?,
        })
    }

    fn row_to_torrent(r: &rusqlite::Row<'_>) -> rusqlite::Result<TorrentRow> {
        Ok(TorrentRow {
            info_hash: r.get("info_hash")?,
            imdb_id: r.get("imdb_id")?,
            magnet: r.get("magnet")?,
            size_bytes: r.get::<_, i64>("size_bytes")? as u64,
            video_file_idx: r.get::<_, i64>("video_file_idx")? as usize,
            video_file_name: r.get("video_file_name")?,
            added_at: r.get("added_at")?,
            private: r.get("private")?,
            download_limit: r.get("download_limit")?,
            upload_limit: r.get("upload_limit")?,
            peer_limit: r.get("peer_limit")?,
        })
    }

    /// Set a movie's overrides; `None` clears one back to the global value.
    pub fn set_limits(
        &self,
        info_hash: &str,
        download: Option<u32>,
        upload: Option<u32>,
        peers: Option<u32>,
    ) -> anyhow::Result<()> {
        self.conn.lock().execute(
            "UPDATE torrents SET download_limit = ?2, upload_limit = ?3, peer_limit = ?4 WHERE info_hash = ?1",
            params![info_hash, download, upload, peers],
        )?;
        Ok(())
    }

    pub fn list_movies(&self) -> anyhow::Result<Vec<Movie>> {
        let conn = self.conn.lock();
        let mut st = conn.prepare("SELECT * FROM movies ORDER BY added_at DESC")?;
        let rows = st.query_map([], Self::row_to_movie)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn get_movie(&self, imdb_id: &str) -> anyhow::Result<Option<Movie>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row(
                "SELECT * FROM movies WHERE imdb_id = ?1",
                [imdb_id],
                Self::row_to_movie,
            )
            .optional()?)
    }

    pub fn list_torrents(&self) -> anyhow::Result<Vec<TorrentRow>> {
        let conn = self.conn.lock();
        let mut st = conn.prepare("SELECT * FROM torrents")?;
        let rows = st.query_map([], Self::row_to_torrent)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn torrent_for_movie(&self, imdb_id: &str) -> anyhow::Result<Option<TorrentRow>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row(
                "SELECT * FROM torrents WHERE imdb_id = ?1 ORDER BY added_at DESC LIMIT 1",
                [imdb_id],
                Self::row_to_torrent,
            )
            .optional()?)
    }

    pub fn torrent_by_hash(&self, info_hash: &str) -> anyhow::Result<Option<TorrentRow>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row(
                "SELECT * FROM torrents WHERE info_hash = ?1",
                [info_hash],
                Self::row_to_torrent,
            )
            .optional()?)
    }

    pub fn delete_movie(&self, imdb_id: &str) -> anyhow::Result<bool> {
        let n = self
            .conn
            .lock()
            .execute("DELETE FROM movies WHERE imdb_id = ?1", [imdb_id])?;
        Ok(n > 0)
    }

    pub fn touch(&self, imdb_id: &str, ts: i64) -> anyhow::Result<()> {
        self.conn.lock().execute(
            "UPDATE movies SET last_used_at = ?2 WHERE imdb_id = ?1",
            params![imdb_id, ts],
        )?;
        Ok(())
    }

    pub fn add_event(&self, kind: &str, message: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO events (ts, kind, message) VALUES (?1, ?2, ?3)",
            params![crate::utils::now_secs(), kind, message],
        )?;
        conn.execute(
            "DELETE FROM events WHERE id NOT IN (SELECT id FROM events ORDER BY id DESC LIMIT 500)",
            [],
        )?;
        Ok(())
    }

    pub fn recent_events(&self, n: usize) -> anyhow::Result<Vec<Event>> {
        let conn = self.conn.lock();
        let mut st =
            conn.prepare("SELECT ts, kind, message FROM events ORDER BY id DESC LIMIT ?1")?;
        let rows = st.query_map([n as i64], |r| {
            Ok(Event {
                ts: r.get(0)?,
                kind: r.get(1)?,
                message: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

#[cfg(test)]
mod migration_tests {
    use super::*;

    #[test]
    fn upgrades_an_old_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        {
            // The first release's schema, before private and the limit columns.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE movies (imdb_id TEXT PRIMARY KEY, tmdb_id INTEGER, title TEXT NOT NULL,
                   year INTEGER, overview TEXT, poster_url TEXT, backdrop_url TEXT, runtime_min INTEGER,
                   genres TEXT NOT NULL DEFAULT '[]', rating REAL, tmdb_json TEXT, added_at INTEGER NOT NULL,
                   last_used_at INTEGER NOT NULL);
                 CREATE TABLE torrents (info_hash TEXT PRIMARY KEY, imdb_id TEXT NOT NULL, magnet TEXT NOT NULL,
                   size_bytes INTEGER NOT NULL, video_file_idx INTEGER NOT NULL, video_file_name TEXT NOT NULL,
                   added_at INTEGER NOT NULL);
                 INSERT INTO movies VALUES ('tt1', 1, 'M', 2000, NULL, NULL, NULL, 90, '[]', NULL, NULL, 1, 1);
                 INSERT INTO torrents VALUES ('h', 'tt1', 'magnet:?x', 5, 0, 'a.mp4', 1);",
            )
            .unwrap();
        }
        let c = Catalog::open(&path).unwrap();
        let t = c.torrent_for_movie("tt1").unwrap().unwrap();
        assert!(!t.private);
        assert_eq!(
            (t.download_limit, t.upload_limit, t.peer_limit),
            (None, None, None)
        );
        // opening again is a no-op
        drop(c);
        Catalog::open(&path).unwrap();
    }
}
