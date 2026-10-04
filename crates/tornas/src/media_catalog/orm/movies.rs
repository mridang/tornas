//! Movie queries.

use diesel::prelude::*;
use diesel::upsert::excluded;

use super::Catalog;
use super::records::{MovieInsert, MovieRecord};
use super::schema::movies;
use crate::media_catalog::store::Movie;

impl Catalog {
    pub fn upsert_movie(&self, m: &Movie, tmdb_json: Option<&str>) -> anyhow::Result<()> {
        use movies::dsl;
        let row = MovieInsert::new(m, tmdb_json)?;
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

    pub fn list_movies(&self) -> anyhow::Result<Vec<Movie>> {
        let conn = &mut *self.conn.lock();
        let rows: Vec<MovieRecord> = movies::table.order(movies::added_at.desc()).load(conn)?;
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

    pub fn delete_movie(&self, imdb_id: &str) -> anyhow::Result<bool> {
        let conn = &mut *self.conn.lock();
        // Foreign keys are ON, so the torrents cascade.
        let n = diesel::delete(movies::table.filter(movies::imdb_id.eq(imdb_id))).execute(conn)?;
        Ok(n > 0)
    }

    pub fn touch(&self, imdb_id: &str, ts: i64) -> anyhow::Result<()> {
        let conn = &mut *self.conn.lock();
        diesel::update(movies::table.filter(movies::imdb_id.eq(imdb_id)))
            .set(movies::last_used_at.eq(ts))
            .execute(conn)?;
        Ok(())
    }
}
