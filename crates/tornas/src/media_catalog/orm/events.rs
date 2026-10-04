//! Event-log queries.

use diesel::prelude::*;

use super::Catalog;
use super::schema::events;
use crate::media_catalog::store::Event;

/// How many events to keep; older ones are pruned on each insert.
const KEEP: i64 = 500;

impl Catalog {
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
        // Keep only the newest `KEEP`: find the KEEP-th-newest id and drop anything
        // older than it.
        let cutoff: Option<i32> = events::table
            .select(dsl::id)
            .order(dsl::id.desc())
            .offset(KEEP - 1)
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
