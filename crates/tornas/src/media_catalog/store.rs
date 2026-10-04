//! SQLite catalog: movies with TMDB metadata, the torrent backing each movie, and a
//! short event log. Queries go through Diesel (see [`super::schema`]); the schema
//! itself is created by the embedded migrations in `migrations/`. One connection
//! behind a mutex — every call is a few microseconds, so this never needs a pool.

use std::path::Path;

use anyhow::Context;
use diesel::connection::SimpleConnection;
use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;
use diesel::upsert::excluded;
use diesel_migrations::{EmbeddedMigrations, MigrationHarness, embed_migrations};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::schema::{events, movies, torrents};

const MIGRATIONS: EmbeddedMigrations = embed_migrations!("src/media_catalog/migrations");

// ---- domain types (what callers see) ---------------------------------------

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
    /// When the download finished, as unix seconds; `None` while still downloading.
    #[serde(default)]
    pub completed_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub ts: i64,
    pub kind: String,
    pub message: String,
}

// ---- Diesel row types (what the database stores) ----------------------------
//
// SQLite stores integers as i64 and has no JSON or unsigned types, so these mirror
// the columns in native types and convert to/from the domain types above.

#[derive(Queryable)]
struct MovieRecord {
    imdb_id: String,
    tmdb_id: Option<i64>,
    title: String,
    year: Option<i32>,
    overview: Option<String>,
    poster_url: Option<String>,
    backdrop_url: Option<String>,
    runtime_min: Option<i64>,
    genres: String,
    rating: Option<f64>,
    #[allow(dead_code)]
    tmdb_json: Option<String>,
    added_at: i64,
    last_used_at: i64,
}

impl From<MovieRecord> for Movie {
    fn from(r: MovieRecord) -> Self {
        Movie {
            imdb_id: r.imdb_id,
            tmdb_id: r.tmdb_id,
            title: r.title,
            year: r.year,
            overview: r.overview,
            poster_url: r.poster_url,
            backdrop_url: r.backdrop_url,
            runtime_min: r.runtime_min,
            genres: serde_json::from_str(&r.genres).unwrap_or_default(),
            rating: r.rating,
            added_at: r.added_at,
            last_used_at: r.last_used_at,
        }
    }
}

#[derive(Insertable)]
#[diesel(table_name = movies)]
struct MovieInsert<'a> {
    imdb_id: &'a str,
    tmdb_id: Option<i64>,
    title: &'a str,
    year: Option<i32>,
    overview: Option<&'a str>,
    poster_url: Option<&'a str>,
    backdrop_url: Option<&'a str>,
    runtime_min: Option<i64>,
    genres: String,
    rating: Option<f64>,
    tmdb_json: Option<&'a str>,
    added_at: i64,
    last_used_at: i64,
}

#[derive(Queryable)]
struct TorrentRecord {
    info_hash: String,
    imdb_id: String,
    magnet: String,
    size_bytes: i64,
    video_file_idx: i64,
    video_file_name: String,
    added_at: i64,
    private: bool,
    download_limit: Option<i64>,
    upload_limit: Option<i64>,
    peer_limit: Option<i64>,
    completed_at: Option<i64>,
}

impl From<TorrentRecord> for TorrentRow {
    fn from(r: TorrentRecord) -> Self {
        TorrentRow {
            info_hash: r.info_hash,
            imdb_id: r.imdb_id,
            magnet: r.magnet,
            size_bytes: r.size_bytes as u64,
            video_file_idx: r.video_file_idx as usize,
            video_file_name: r.video_file_name,
            added_at: r.added_at,
            private: r.private,
            download_limit: r.download_limit.map(|v| v as u32),
            upload_limit: r.upload_limit.map(|v| v as u32),
            peer_limit: r.peer_limit.map(|v| v as u32),
            completed_at: r.completed_at,
        }
    }
}

/// Inserted columns for a torrent. `completed_at` is deliberately absent so that
/// re-adding a torrent (an `INSERT OR REPLACE`) resets it to NULL, i.e. "downloading
/// again", exactly as the previous raw-SQL insert did.
#[derive(Insertable)]
#[diesel(table_name = torrents)]
struct TorrentInsert<'a> {
    info_hash: &'a str,
    imdb_id: &'a str,
    magnet: &'a str,
    size_bytes: i64,
    video_file_idx: i64,
    video_file_name: &'a str,
    added_at: i64,
    private: bool,
    download_limit: Option<i64>,
    upload_limit: Option<i64>,
    peer_limit: Option<i64>,
}

// ---- the store -------------------------------------------------------------

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

    pub fn upsert_movie(&self, m: &Movie, tmdb_json: Option<&str>) -> anyhow::Result<()> {
        use movies::dsl;
        let row = MovieInsert {
            imdb_id: &m.imdb_id,
            tmdb_id: m.tmdb_id,
            title: &m.title,
            year: m.year,
            overview: m.overview.as_deref(),
            poster_url: m.poster_url.as_deref(),
            backdrop_url: m.backdrop_url.as_deref(),
            runtime_min: m.runtime_min,
            genres: serde_json::to_string(&m.genres)?,
            rating: m.rating,
            tmdb_json,
            added_at: m.added_at,
            last_used_at: m.last_used_at,
        };
        let conn = &mut *self.conn.lock();
        // On conflict, keep imdb_id and added_at; refresh everything else, including
        // last_used_at.
        diesel::insert_into(movies::table)
            .values(&row)
            .on_conflict(movies::imdb_id)
            .do_update()
            .set((
                dsl::tmdb_id.eq(excluded(dsl::tmdb_id)),
                dsl::title.eq(excluded(dsl::title)),
                dsl::year.eq(excluded(dsl::year)),
                dsl::overview.eq(excluded(dsl::overview)),
                dsl::poster_url.eq(excluded(dsl::poster_url)),
                dsl::backdrop_url.eq(excluded(dsl::backdrop_url)),
                dsl::runtime_min.eq(excluded(dsl::runtime_min)),
                dsl::genres.eq(excluded(dsl::genres)),
                dsl::rating.eq(excluded(dsl::rating)),
                dsl::tmdb_json.eq(excluded(dsl::tmdb_json)),
                dsl::last_used_at.eq(excluded(dsl::last_used_at)),
            ))
            .execute(conn)?;
        Ok(())
    }

    pub fn insert_torrent(&self, t: &TorrentRow) -> anyhow::Result<()> {
        let row = TorrentInsert {
            info_hash: &t.info_hash,
            imdb_id: &t.imdb_id,
            magnet: &t.magnet,
            size_bytes: t.size_bytes as i64,
            video_file_idx: t.video_file_idx as i64,
            video_file_name: &t.video_file_name,
            added_at: t.added_at,
            private: t.private,
            download_limit: t.download_limit.map(i64::from),
            upload_limit: t.upload_limit.map(i64::from),
            peer_limit: t.peer_limit.map(i64::from),
        };
        let conn = &mut *self.conn.lock();
        diesel::replace_into(torrents::table)
            .values(&row)
            .execute(conn)?;
        Ok(())
    }

    /// Set a movie's overrides; `None` clears one back to the global value.
    pub fn set_limits(
        &self,
        info_hash: &str,
        download: Option<u32>,
        upload: Option<u32>,
        peers: Option<u32>,
    ) -> anyhow::Result<()> {
        use torrents::dsl;
        let conn = &mut *self.conn.lock();
        diesel::update(torrents::table.filter(dsl::info_hash.eq(info_hash)))
            .set((
                dsl::download_limit.eq(download.map(i64::from)),
                dsl::upload_limit.eq(upload.map(i64::from)),
                dsl::peer_limit.eq(peers.map(i64::from)),
            ))
            .execute(conn)?;
        Ok(())
    }

    pub fn list_movies(&self) -> anyhow::Result<Vec<Movie>> {
        let conn = &mut *self.conn.lock();
        let rows: Vec<MovieRecord> = movies::table
            .order(movies::added_at.desc())
            .load(conn)?;
        Ok(rows.into_iter().map(Movie::from).collect())
    }

    pub fn get_movie(&self, imdb_id: &str) -> anyhow::Result<Option<Movie>> {
        let conn = &mut *self.conn.lock();
        let row: Option<MovieRecord> = movies::table
            .filter(movies::imdb_id.eq(imdb_id))
            .first(conn)
            .optional()?;
        Ok(row.map(Movie::from))
    }

    pub fn list_torrents(&self) -> anyhow::Result<Vec<TorrentRow>> {
        let conn = &mut *self.conn.lock();
        let rows: Vec<TorrentRecord> = torrents::table.load(conn)?;
        Ok(rows.into_iter().map(TorrentRow::from).collect())
    }

    pub fn torrent_for_movie(&self, imdb_id: &str) -> anyhow::Result<Option<TorrentRow>> {
        let conn = &mut *self.conn.lock();
        let row: Option<TorrentRecord> = torrents::table
            .filter(torrents::imdb_id.eq(imdb_id))
            .order(torrents::added_at.desc())
            .first(conn)
            .optional()?;
        Ok(row.map(TorrentRow::from))
    }

    pub fn torrent_by_hash(&self, info_hash: &str) -> anyhow::Result<Option<TorrentRow>> {
        let conn = &mut *self.conn.lock();
        let row: Option<TorrentRecord> = torrents::table
            .filter(torrents::info_hash.eq(info_hash))
            .first(conn)
            .optional()?;
        Ok(row.map(TorrentRow::from))
    }

    /// Mark a torrent's download complete (unix seconds). Set by the engine when a
    /// download finishes; readers use it to show only fully-downloaded media.
    pub fn mark_complete(&self, info_hash: &str, ts: i64) -> anyhow::Result<()> {
        use torrents::dsl;
        let conn = &mut *self.conn.lock();
        diesel::update(torrents::table.filter(dsl::info_hash.eq(info_hash)))
            .set(dsl::completed_at.eq(Some(ts)))
            .execute(conn)?;
        Ok(())
    }

    pub fn delete_movie(&self, imdb_id: &str) -> anyhow::Result<bool> {
        let conn = &mut *self.conn.lock();
        // Foreign keys are ON, so the torrents cascade.
        let n = diesel::delete(movies::table.filter(movies::imdb_id.eq(imdb_id))).execute(conn)?;
        Ok(n > 0)
    }

    pub fn touch(&self, imdb_id: &str, ts: i64) -> anyhow::Result<()> {
        use movies::dsl;
        let conn = &mut *self.conn.lock();
        diesel::update(movies::table.filter(dsl::imdb_id.eq(imdb_id)))
            .set(dsl::last_used_at.eq(ts))
            .execute(conn)?;
        Ok(())
    }

    pub fn add_event(&self, kind: &str, message: &str) -> anyhow::Result<()> {
        use events::dsl;
        let conn = &mut *self.conn.lock();
        diesel::insert_into(events::table)
            .values((
                dsl::ts.eq(crate::utils::now_secs()),
                dsl::kind.eq(kind),
                dsl::message.eq(message),
            ))
            .execute(conn)?;
        // Keep only the newest 500: find the 500th-newest id and drop anything older.
        let cutoff: Option<i32> = events::table
            .select(dsl::id)
            .order(dsl::id.desc())
            .offset(499)
            .limit(1)
            .first(conn)
            .optional()?;
        if let Some(c) = cutoff {
            diesel::delete(events::table.filter(dsl::id.lt(c))).execute(conn)?;
        }
        Ok(())
    }

    pub fn recent_events(&self, n: usize) -> anyhow::Result<Vec<Event>> {
        let conn = &mut *self.conn.lock();
        let rows: Vec<(i64, String, String)> = events::table
            .order(events::id.desc())
            .limit(n as i64)
            .select((events::ts, events::kind, events::message))
            .load(conn)?;
        Ok(rows
            .into_iter()
            .map(|(ts, kind, message)| Event { ts, kind, message })
            .collect())
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
        // Migrations are idempotent: reopening the same file is a no-op and the data
        // survives.
        let c = Catalog::open(&path).unwrap();
        assert_eq!(c.list_movies().unwrap().len(), 1);
    }
}
