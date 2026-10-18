//! SQLite archive. Every table is `IF NOT EXISTS` in [`SCHEMA`]; additive
//! changes go through [`Store::run_migrations`] keyed on `PRAGMA user_version`
//! (same scheme as outflow). Ingest is idempotent: re-importing an export, or a
//! newer export that overlaps an older one, only adds what is new.

use rusqlite::{params, Connection, OptionalExtension};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS types (
    id INTEGER PRIMARY KEY,
    identifier TEXT NOT NULL UNIQUE,
    -- Most recently seen unit; individual records keep their own.
    unit TEXT,
    -- Denormalized per-type stats, refreshed after each import so
    -- /api/types never scans millions of rows.
    record_count INTEGER NOT NULL DEFAULT 0,
    first_ts INTEGER,
    last_ts INTEGER
);

CREATE TABLE IF NOT EXISTS sources (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);

CREATE TABLE IF NOT EXISTS devices (
    id INTEGER PRIMARY KEY,
    description TEXT NOT NULL UNIQUE
);

CREATE TABLE IF NOT EXISTS records (
    id INTEGER PRIMARY KEY,
    type_id INTEGER NOT NULL REFERENCES types(id),
    source_id INTEGER NOT NULL REFERENCES sources(id),
    device_id INTEGER REFERENCES devices(id),
    start_ts INTEGER NOT NULL,
    end_ts INTEGER NOT NULL,
    tz_offset_min INTEGER NOT NULL,
    created_ts INTEGER,
    value_num REAL,
    value_text TEXT,
    unit TEXT
);
-- The export has no record ids, so this natural key is the dedupe key
-- (INVARIANTS #1). IFNULL because NULLs never collide in a UNIQUE index.
-- Column order doubles as the (type, time-range) query index.
CREATE UNIQUE INDEX IF NOT EXISTS records_natural ON records(
    type_id, start_ts, source_id, end_ts, IFNULL(value_num, ''), IFNULL(value_text, '')
);

CREATE TABLE IF NOT EXISTS record_metadata (
    record_id INTEGER NOT NULL REFERENCES records(id),
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (record_id, key)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS workouts (
    id INTEGER PRIMARY KEY,
    -- activity|start_ts|end_ts|source — the export has no workout ids either.
    natural_key TEXT NOT NULL UNIQUE,
    activity_type TEXT NOT NULL,
    source_id INTEGER NOT NULL REFERENCES sources(id),
    device_id INTEGER REFERENCES devices(id),
    start_ts INTEGER NOT NULL,
    end_ts INTEGER NOT NULL,
    tz_offset_min INTEGER NOT NULL,
    duration_s REAL,
    metadata_json TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS workouts_start ON workouts(start_ts);

CREATE TABLE IF NOT EXISTS workout_stats (
    workout_id INTEGER NOT NULL REFERENCES workouts(id),
    type TEXT NOT NULL,
    sum REAL,
    avg REAL,
    min REAL,
    max REAL,
    unit TEXT,
    PRIMARY KEY (workout_id, type)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS workout_events (
    id INTEGER PRIMARY KEY,
    workout_id INTEGER NOT NULL REFERENCES workouts(id),
    kind TEXT NOT NULL,
    ts INTEGER NOT NULL,
    duration_s REAL
);
CREATE INDEX IF NOT EXISTS workout_events_workout ON workout_events(workout_id);

CREATE TABLE IF NOT EXISTS routes (
    id INTEGER PRIMARY KEY,
    workout_id INTEGER NOT NULL REFERENCES workouts(id),
    file TEXT NOT NULL,
    point_count INTEGER NOT NULL DEFAULT 0,
    distance_m REAL,
    start_ts INTEGER,
    end_ts INTEGER,
    UNIQUE (workout_id, file)
);

-- Precise location history. Full-token only over the API (INVARIANTS #4).
CREATE TABLE IF NOT EXISTS route_points (
    route_id INTEGER NOT NULL REFERENCES routes(id),
    ts INTEGER NOT NULL,
    lat REAL NOT NULL,
    lon REAL NOT NULL,
    ele REAL,
    speed REAL,
    course REAL,
    h_acc REAL,
    PRIMARY KEY (route_id, ts)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS activity_summaries (
    date TEXT PRIMARY KEY,
    active_kcal REAL,
    active_goal REAL,
    move_min REAL,
    move_goal REAL,
    exercise_min REAL,
    exercise_goal REAL,
    stand_hours REAL,
    stand_goal REAL
);

-- Derived. Rebuilt by rollup::rebuild for the days an import touched; safe to
-- drop and rebuild from records at any time.
CREATE TABLE IF NOT EXISTS daily (
    date TEXT NOT NULL,
    metric TEXT NOT NULL,
    value REAL NOT NULL,
    PRIMARY KEY (date, metric)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS import_log (
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,
    file TEXT NOT NULL,
    export_ts INTEGER,
    records_added INTEGER NOT NULL,
    records_skipped INTEGER NOT NULL,
    workouts_added INTEGER NOT NULL,
    routes_added INTEGER NOT NULL,
    route_points_added INTEGER NOT NULL,
    activity_upserted INTEGER NOT NULL,
    days_rebuilt INTEGER NOT NULL,
    elapsed_ms INTEGER NOT NULL
);
"#;

/// Schema version, stamped into `PRAGMA user_version` by `run_migrations`.
///
/// - v1: initial schema.
const SCHEMA_VERSION: i64 = 1;

pub struct Store {
    pub(crate) conn: Connection,
}

impl Store {
    pub fn open(path: &str) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        let s = Store { conn };
        s.migrate()?;
        Ok(s)
    }

    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        let s = Store { conn };
        s.migrate()?;
        Ok(s)
    }

    #[cfg(feature = "encryption")]
    pub fn open_encrypted(path: &str, key: &str) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "key", key)?;
        let s = Store { conn };
        s.migrate()?;
        Ok(s)
    }

    fn migrate(&self) -> rusqlite::Result<()> {
        // WAL so a CLI import can run while the server keeps answering reads
        // from its own connection; busy_timeout covers the commit handoff.
        self.conn.busy_timeout(std::time::Duration::from_secs(10))?;
        let _: String = self
            .conn
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        self.conn.pragma_update(None, "synchronous", "NORMAL")?;
        // Fresh DBs get every table/column from SCHEMA (all `IF NOT EXISTS`).
        self.conn.execute_batch(SCHEMA)?;
        self.run_migrations()
    }

    fn run_migrations(&self) -> rusqlite::Result<()> {
        let _version: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?;
        // Future additive changes go here as `if version < N { … }` blocks
        // using add_column_if_missing; columns are append-only.
        self.conn
            .pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(())
    }

    #[allow(dead_code)]
    fn add_column_if_missing(&self, table: &str, column: &str, decl: &str) -> rusqlite::Result<()> {
        let exists: bool = self
            .conn
            .prepare(&format!("PRAGMA table_info({table})"))?
            .query_map([], |r| r.get::<_, String>(1))?
            .filter_map(Result::ok)
            .any(|c| c == column);
        if !exists {
            self.conn
                .execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"))?;
        }
        Ok(())
    }

    pub fn count_records(&self) -> rusqlite::Result<i64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM records", [], |r| r.get(0))
    }

    // ── id lookups (get-or-create) ────────────────────────────────────────

    pub(crate) fn type_id(&self, identifier: &str, unit: Option<&str>) -> rusqlite::Result<i64> {
        self.conn
            .prepare_cached(
                "INSERT INTO types(identifier, unit) VALUES (?1, ?2)
                 ON CONFLICT(identifier) DO UPDATE SET unit = COALESCE(excluded.unit, unit)",
            )?
            .execute(params![identifier, unit])?;
        self.conn
            .prepare_cached("SELECT id FROM types WHERE identifier = ?1")?
            .query_row([identifier], |r| r.get(0))
    }

    pub(crate) fn source_id(&self, name: &str) -> rusqlite::Result<i64> {
        self.conn
            .prepare_cached("INSERT OR IGNORE INTO sources(name) VALUES (?1)")?
            .execute([name])?;
        self.conn
            .prepare_cached("SELECT id FROM sources WHERE name = ?1")?
            .query_row([name], |r| r.get(0))
    }

    pub(crate) fn device_id(&self, description: &str) -> rusqlite::Result<i64> {
        self.conn
            .prepare_cached("INSERT OR IGNORE INTO devices(description) VALUES (?1)")?
            .execute([description])?;
        self.conn
            .prepare_cached("SELECT id FROM devices WHERE description = ?1")?
            .query_row([description], |r| r.get(0))
    }

    pub(crate) fn lookup_type_id(&self, identifier: &str) -> rusqlite::Result<Option<i64>> {
        self.conn
            .prepare_cached("SELECT id FROM types WHERE identifier = ?1")?
            .query_row([identifier], |r| r.get(0))
            .optional()
    }

    /// Refresh the denormalized per-type count / first / last.
    pub(crate) fn refresh_type_stats(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch(
            "UPDATE types SET
                record_count = (SELECT COUNT(*) FROM records r WHERE r.type_id = types.id),
                first_ts = (SELECT MIN(start_ts) FROM records r WHERE r.type_id = types.id),
                last_ts = (SELECT MAX(start_ts) FROM records r WHERE r.type_id = types.id)",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_stamps_schema_version() {
        let s = Store::open_in_memory().unwrap();
        let v: i64 = s
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

    #[test]
    fn reopening_is_idempotent() {
        let path = std::env::temp_dir().join(format!("vitals-reopen-{}.db", std::process::id()));
        let p = path.to_str().unwrap();
        {
            let s = Store::open(p).unwrap();
            s.type_id("HKQuantityTypeIdentifierStepCount", Some("count"))
                .unwrap();
        }
        let s = Store::open(p).unwrap();
        assert_eq!(
            s.lookup_type_id("HKQuantityTypeIdentifierStepCount")
                .unwrap(),
            Some(1)
        );
        drop(s);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{p}{suffix}"));
        }
    }

    #[test]
    fn natural_key_dedupes_null_values() {
        let s = Store::open_in_memory().unwrap();
        let t = s
            .type_id("HKCategoryTypeIdentifierMindfulSession", None)
            .unwrap();
        let src = s.source_id("x").unwrap();
        let ins =
            "INSERT OR IGNORE INTO records(type_id, source_id, start_ts, end_ts, tz_offset_min)
                   VALUES (?1, ?2, 100, 200, 0)";
        assert_eq!(s.conn.execute(ins, params![t, src]).unwrap(), 1);
        // Both value columns NULL: a plain UNIQUE would let this through.
        assert_eq!(s.conn.execute(ins, params![t, src]).unwrap(), 0);
    }

    #[test]
    fn lookups_are_get_or_create() {
        let s = Store::open_in_memory().unwrap();
        let a = s.source_id("Watch").unwrap();
        assert_eq!(s.source_id("Watch").unwrap(), a);
        assert_ne!(s.source_id("iPhone").unwrap(), a);
        let d = s.device_id("name:Apple Watch").unwrap();
        assert_eq!(s.device_id("name:Apple Watch").unwrap(), d);
        let t = s.type_id("T", Some("km")).unwrap();
        assert_eq!(s.type_id("T", None).unwrap(), t);
        let unit: Option<String> = s
            .conn
            .query_row("SELECT unit FROM types WHERE id = ?1", [t], |r| r.get(0))
            .unwrap();
        assert_eq!(unit.as_deref(), Some("km"));
    }
}
