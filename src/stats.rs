//! SQLite-backed users, per-user settings and usage statistics.

use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

pub struct Stats {
    conn: Mutex<Connection>,
}

#[derive(Debug, Default, PartialEq)]
pub struct PeriodStats {
    pub requests: i64,
    pub errors: i64,
    pub audio_seconds: f64,
}

#[derive(Debug, PartialEq)]
pub struct TopUser {
    pub name: String,
    pub requests: i64,
    pub audio_seconds: f64,
}

pub struct Record<'a> {
    pub user_id: i64,
    pub username: &'a str,
    pub mode: &'a str,
    pub kind: &'a str,
    pub audio_seconds: f64,
    pub error: Option<&'a str>,
    pub provider: &'a str,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Start timestamp for "day" / "week" / "month" / "overall".
pub fn period_start(period: &str) -> i64 {
    let day = 86_400;
    match period {
        "day" => now() - day,
        "week" => now() - 7 * day,
        "month" => now() - 30 * day,
        _ => 0,
    }
}

impl Stats {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS users (
                 user_id    INTEGER PRIMARY KEY,
                 username   TEXT NOT NULL DEFAULT '',
                 first_name TEXT NOT NULL DEFAULT '',
                 last_name  TEXT NOT NULL DEFAULT '',
                 language   TEXT,
                 last_seen  INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS requests (
                 id            INTEGER PRIMARY KEY AUTOINCREMENT,
                 ts            INTEGER NOT NULL,
                 user_id       INTEGER NOT NULL,
                 username      TEXT NOT NULL,
                 mode          TEXT NOT NULL,
                 kind          TEXT NOT NULL,
                 audio_seconds REAL NOT NULL DEFAULT 0,
                 ok            INTEGER NOT NULL,
                 error         TEXT,
                 provider      TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS requests_ts ON requests(ts);",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn register_user(
        &self,
        user_id: i64,
        username: &str,
        first: &str,
        last: &str,
    ) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO users (user_id, username, first_name, last_name, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(user_id) DO UPDATE SET
                 username=excluded.username, first_name=excluded.first_name,
                 last_name=excluded.last_name, last_seen=excluded.last_seen",
            params![user_id, username, first, last, now()],
        )?;
        Ok(())
    }

    pub fn set_language(&self, user_id: i64, language: Option<&str>) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO users (user_id, language, last_seen) VALUES (?1, ?2, ?3)
             ON CONFLICT(user_id) DO UPDATE SET language=excluded.language",
            params![user_id, language, now()],
        )?;
        Ok(())
    }

    /// Some(Some(code)) = fixed language, Some(None) = explicit auto,
    /// None = never set (use the server default).
    pub fn language(&self, user_id: i64) -> Result<Option<Option<String>>> {
        let conn = self.conn.lock().unwrap();
        let row: Option<Option<String>> = conn
            .query_row(
                "SELECT language FROM users WHERE user_id=?1",
                params![user_id],
                |r| r.get(0),
            )
            .optional()?;
        // NULL language means "never set"; "auto" is stored explicitly.
        Ok(match row.flatten() {
            None => None,
            Some(l) if l == "auto" => Some(None),
            Some(l) => Some(Some(l)),
        })
    }

    pub fn record(&self, r: &Record) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO requests (ts, user_id, username, mode, kind, audio_seconds, ok, error, provider)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![now(), r.user_id, r.username, r.mode, r.kind, r.audio_seconds, r.error.is_none() as i64, r.error, r.provider],
        )?;
        Ok(())
    }

    pub fn summary(&self, since: i64) -> Result<PeriodStats> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(1 - ok), 0), COALESCE(SUM(audio_seconds), 0) FROM requests WHERE ts >= ?1",
            params![since],
            |r| Ok(PeriodStats { requests: r.get(0)?, errors: r.get(1)?, audio_seconds: r.get(2)? }),
        )?)
    }

    pub fn top_users(&self, since: i64, limit: i64) -> Result<Vec<TopUser>> {
        let conn = self.conn.lock().unwrap();
        let mut st = conn.prepare(
            "SELECT username, COUNT(*) AS n, COALESCE(SUM(audio_seconds), 0)
             FROM requests WHERE ts >= ?1 GROUP BY user_id ORDER BY n DESC LIMIT ?2",
        )?;
        let rows = st.query_map(params![since, limit], |r| {
            Ok(TopUser {
                name: r.get(0)?,
                requests: r.get(1)?,
                audio_seconds: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn user_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages_and_stats() {
        let s = Stats::in_memory().unwrap();
        s.register_user(1, "marko", "Marko", "K").unwrap();
        assert_eq!(s.language(1).unwrap(), None);
        s.set_language(1, Some("sr")).unwrap();
        assert_eq!(s.language(1).unwrap(), Some(Some("sr".into())));
        s.set_language(1, Some("auto")).unwrap();
        assert_eq!(s.language(1).unwrap(), Some(None));
        // Re-registering keeps the language.
        s.register_user(1, "marko", "Marko", "K").unwrap();
        assert_eq!(s.language(1).unwrap(), Some(None));
        s.set_language(2, Some("ru")).unwrap();
        assert_eq!(s.user_count().unwrap(), 2);

        let rec = |user_id, username, secs, error| Record {
            user_id,
            username,
            mode: "private",
            kind: "voice",
            audio_seconds: secs,
            error,
            provider: "x",
        };
        s.record(&rec(1, "@marko", 30.0, None)).unwrap();
        s.record(&rec(1, "@marko", 10.0, Some("boom"))).unwrap();
        s.record(&rec(2, "@ana", 5.0, None)).unwrap();

        let sum = s.summary(period_start("day")).unwrap();
        assert_eq!(
            sum,
            PeriodStats {
                requests: 3,
                errors: 1,
                audio_seconds: 45.0
            }
        );
        let top = s.top_users(0, 10).unwrap();
        assert_eq!(
            top[0],
            TopUser {
                name: "@marko".into(),
                requests: 2,
                audio_seconds: 40.0
            }
        );
        assert_eq!(top.len(), 2);
    }
}
