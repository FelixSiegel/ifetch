use rusqlite::{Connection, Result, params};
use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::config::CRON_HOURS;

const MAX_INTERVAL_HOURS: i64 = 720; // 30 days
const SECONDS_PER_HOUR: i64 = 3600;
const BACKOFF_MULTIPLIER: f64 = 1.5;

pub fn init_db(path: impl AsRef<Path>) -> Result<Connection> {
    let conn = Connection::open(path)?;

    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = 5000;",
    )?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS mangas (
            id TEXT PRIMARY KEY,
            title TEXT,
            status TEXT,
            remote_chapters INTEGER,
            local_chapters INTEGER,
            last_checked INTEGER,
            next_check INTEGER,
            check_interval_hours INTEGER
        )",
        [],
    )?;

    // Create a composite index to speed up query, preventing full table scans
    // https://stackoverflow.com/questions/795031/how-do-composite-indexes-work#795068
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_mangas_next_check ON mangas(status, next_check)",
        [],
    )?;

    Ok(conn)
}

pub fn upsert_manga(
    conn: &Connection,
    id: &str,
    title: &str,
    status: &str,
    remote_chapters: usize,
    local_chapters: Option<usize>,
    did_update: bool,
) -> Result<()> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    let new_interval = if did_update {
        *CRON_HOURS
    } else {
        let current_interval = conn
            .query_row(
                "SELECT check_interval_hours FROM mangas WHERE id = ?1",
                params![id],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(*CRON_HOURS);

        let backed_off = (current_interval as f64 * BACKOFF_MULTIPLIER) as i64;
        backed_off.min(MAX_INTERVAL_HOURS)
    };

    let next_check = now + (new_interval * SECONDS_PER_HOUR);
    let local_val = local_chapters.map(|c| c as i64);

    conn.execute(
        "INSERT INTO mangas (id, title, status, remote_chapters, local_chapters, last_checked, next_check, check_interval_hours)
         VALUES (?1, ?2, ?3, ?4, COALESCE(?5, 0), ?6, ?7, ?8)
         ON CONFLICT(id) DO UPDATE SET
         title = excluded.title,
         status = excluded.status,
         remote_chapters = excluded.remote_chapters,
         local_chapters = COALESCE(?5, mangas.local_chapters),
         last_checked = excluded.last_checked,
         next_check = excluded.next_check,
         check_interval_hours = excluded.check_interval_hours",
        params![id, title, status, remote_chapters as i64, local_val, now, next_check, new_interval],
    )?;

    Ok(())
}

pub struct MangaCheck {
    pub id: String,
}

pub fn get_mangas_to_check(conn: &Connection) -> Result<Vec<MangaCheck>> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    let mut stmt =
        conn.prepare("SELECT id FROM mangas WHERE (status != 'Completed' AND next_check <= ?1) OR (local_chapters < remote_chapters)")?;

    let mangas = stmt.query_map(params![now], |row| Ok(MangaCheck { id: row.get(0)? }))?;
    mangas.collect()
}

pub fn get_manga_title(conn: &Connection, id: &str) -> Result<Option<String>> {
    let mut stmt = conn.prepare("SELECT title FROM mangas WHERE id = ?1")?;
    let mut rows = stmt.query(params![id])?;
    if let Some(row) = rows.next()? {
        Ok(Some(row.get(0)?))
    } else {
        Ok(None)
    }
}
