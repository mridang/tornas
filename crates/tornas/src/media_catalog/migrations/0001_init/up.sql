-- The full catalog schema. `IF NOT EXISTS` so this is a no-op on a database that
-- an earlier (rusqlite) release already created, and a clean create on a fresh one.
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
    added_at        INTEGER NOT NULL,
    private         INTEGER NOT NULL DEFAULT 0,
    download_limit  INTEGER,
    upload_limit    INTEGER,
    peer_limit      INTEGER,
    completed_at    INTEGER
);

CREATE INDEX IF NOT EXISTS torrents_imdb ON torrents(imdb_id);

CREATE TABLE IF NOT EXISTS events (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    ts      INTEGER NOT NULL,
    kind    TEXT NOT NULL,
    message TEXT NOT NULL
);
