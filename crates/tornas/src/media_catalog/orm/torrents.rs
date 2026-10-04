//! Torrent queries.

use diesel::prelude::*;

use super::Catalog;
use super::records::{TorrentInsert, TorrentRecord};
use super::schema::torrents;
use crate::media_catalog::store::TorrentRow;

impl Catalog {
    pub fn insert_torrent(&self, t: &TorrentRow) -> anyhow::Result<()> {
        let row = TorrentInsert::from(t);
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
}
