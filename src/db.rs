use rusqlite::{Connection, Result, params};
use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::config::CRON_HOURS;

const MAX_INTERVAL_HOURS: i64 = 720; // 30 days
const SECONDS_PER_HOUR: i64 = 3600;
const BACKOFF_MULTIPLIER: f64 = 1.5;

/// Helper function to retrieve the current UTC timestamp in seconds.
fn current_unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Initializes the SQLite database, sets pragmas (WAL mode, normal synchronous, busy timeout),
/// and creates the required tables and indexes if they do not exist.
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

/// Specifies the context under which a manga's status or chapter list was checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckTrigger {
    /// Periodic background cron check: backs off if no new chapters found
    Cron { new_chapters: bool },
    /// User requested or viewed: updates last_checked, resets or preserves interval, never backs off
    UserRequest { new_chapters: bool },
    /// Download finished: updates chapter counts, resets interval on success
    DownloadComplete { success: bool },
}

/// Inserts or updates manga metadata and next-check scheduling intervals in the database.
///
/// Uses an adaptive exponential backoff for periodic cron checks when no new chapters are found,
/// while resetting to the base interval whenever new chapters are discovered or a download completes.
pub fn upsert_manga(
    conn: &Connection,
    id: &str,
    title: &str,
    status: &str,
    remote_chapters: usize,
    local_chapters: Option<usize>,
    trigger: CheckTrigger,
) -> Result<()> {
    let now = current_unix_timestamp();
    let base_interval = (*CRON_HOURS).max(1);

    let (new_interval, next_check) = match trigger {
        CheckTrigger::Cron { new_chapters: true }
        | CheckTrigger::UserRequest { new_chapters: true }
        | CheckTrigger::DownloadComplete { success: true } => {
            (base_interval, now + (base_interval * SECONDS_PER_HOUR))
        }
        CheckTrigger::Cron {
            new_chapters: false,
        } => {
            let current_interval = conn
                .query_row(
                    "SELECT check_interval_hours FROM mangas WHERE id = ?1",
                    params![id],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap_or(base_interval);

            let max_interval = MAX_INTERVAL_HOURS.max(base_interval);
            let backed_off = ((current_interval as f64 * BACKOFF_MULTIPLIER) as i64)
                .clamp(base_interval, max_interval);
            (backed_off, now + (backed_off * SECONDS_PER_HOUR))
        }
        CheckTrigger::UserRequest {
            new_chapters: false,
        }
        | CheckTrigger::DownloadComplete { success: false } => {
            let current_interval = conn
                .query_row(
                    "SELECT check_interval_hours FROM mangas WHERE id = ?1",
                    params![id],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap_or(base_interval);
            (
                current_interval,
                now + (current_interval * SECONDS_PER_HOUR),
            )
        }
    };

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

/// Manga entry ready for a background check or download update.
pub struct MangaCheck {
    pub id: String,
}

/// Queries the database for mangas that are due for a periodic check or have missing local chapters.
pub fn get_mangas_to_check(conn: &Connection) -> Result<Vec<MangaCheck>> {
    let now = current_unix_timestamp();

    let mut stmt =
        conn.prepare("SELECT id FROM mangas WHERE (status != 'Completed' AND next_check <= ?1) OR (local_chapters < remote_chapters)")?;

    let mangas = stmt.query_map(params![now], |row| Ok(MangaCheck { id: row.get(0)? }))?;
    mangas.collect()
}

/// Retrieves the title of a manga by its ID from the local database.
pub fn get_manga_title(conn: &Connection, id: &str) -> Result<Option<String>> {
    let mut stmt = conn.prepare("SELECT title FROM mangas WHERE id = ?1")?;
    let mut rows = stmt.query(params![id])?;
    if let Some(row) = rows.next()? {
        Ok(Some(row.get(0)?))
    } else {
        Ok(None)
    }
}
