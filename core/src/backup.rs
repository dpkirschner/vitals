//! A consistent, still-encrypted snapshot of the live DB, for the nightly
//! off-machine backup. Safe while the server holds the DB open.
//!
//! Why in-app and not `sqlite3 .backup`: an encrypted DB is SQLCipher, which
//! the stock `sqlite3` can't open. Doing it here reuses the exact cipher build
//! and the 0600 key-file handling. The live DB is in WAL mode; the backup API
//! reads through the WAL, so recent uncheckpointed writes are included.
//! Same design as outflow's `core::backup`.
//!
//! How:
//! 1. Open the source WITHOUT migrating (`Store::open*` would run migrations,
//!    and a newer CLI must never upgrade the schema under a running server).
//!    The source must already exist.
//! 2. In one read transaction on the source: count every table's rows, then
//!    copy all pages with SQLite's online backup API in a single step. Both
//!    see the same snapshot, so the counts match the copy exactly even while
//!    the server writes.
//! 3. The copy goes to `<to>.partial` (created new, 0600), keyed with the same
//!    key, switched to a single file (`journal_mode=DELETE`), then validated
//!    (integrity_check, schema version, per-table row counts) and renamed to
//!    `<to>`. A failed check deletes the partial file, so `<to>` only ever
//!    holds a verified backup.

use rusqlite::{backup::Backup, Connection, OpenFlags};
use serde::Serialize;
use std::fs::OpenOptions;
use std::path::Path;

/// One table's row count in the backup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TableCount {
    pub table: String,
    pub rows: i64,
}

/// What a verified backup contains.
#[derive(Debug, Clone, Serialize)]
pub struct BackupReport {
    /// Where the backup was written.
    pub path: String,
    pub bytes: u64,
    /// `PRAGMA user_version` (the store's schema version).
    pub schema_version: i64,
    /// Whether the copy is SQLCipher-encrypted (a key was given).
    pub encrypted: bool,
    /// Every table and its row count, by name. Matches the source snapshot.
    pub tables: Vec<TableCount>,
}

impl BackupReport {
    pub fn total_rows(&self) -> i64 {
        self.tables.iter().map(|t| t.rows).sum()
    }
}

/// Snapshot `src` into a new file at `to`, verified. `key` is the SQLCipher
/// key for both (None = plaintext). Refuses to overwrite `to`.
pub fn backup_db(src: &str, to: &str, key: Option<&str>) -> Result<BackupReport, String> {
    #[cfg(not(feature = "encryption"))]
    if key.is_some() {
        return Err("a DB key is set but this build has no encryption; \
                    rebuild with --features encryption"
            .into());
    }
    if !Path::new(src).is_file() {
        return Err(format!("{src}: no such database"));
    }
    if Path::new(to).exists() {
        return Err(format!("{to} already exists; backup never overwrites"));
    }
    let partial = format!("{to}.partial");
    create_private(&partial)?;
    let result = copy_and_verify(src, &partial, key).and_then(|mut report| {
        std::fs::rename(&partial, to).map_err(|e| format!("rename {partial} → {to}: {e}"))?;
        report.path = to.to_string();
        Ok(report)
    });
    if result.is_err() {
        remove_with_sidecars(&partial);
    }
    result
}

/// Create an empty file with mode 0600, failing if it exists. SQLite treats
/// an empty file as a new DB and keeps its permissions.
fn create_private(path: &str) -> Result<(), String> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
        .map(drop)
        .map_err(|e| format!("create {path}: {e}"))
}

fn remove_with_sidecars(path: &str) {
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{path}{suffix}"));
    }
}

fn open_existing(path: &str, key: Option<&str>, flags: OpenFlags) -> Result<Connection, String> {
    let conn = Connection::open_with_flags(path, flags).map_err(|e| format!("open {path}: {e}"))?;
    if let Some(k) = key {
        conn.pragma_update(None, "key", k)
            .map_err(|e| format!("key {path}: {e}"))?;
    }
    conn.busy_timeout(std::time::Duration::from_secs(10))
        .map_err(|e| format!("{path}: {e}"))?;
    // The first real read is where a wrong or missing key shows up.
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
        r.get::<_, i64>(0)
    })
    .map_err(|e| format!("read {path}: {e} (wrong or missing DB key?)"))?;
    Ok(conn)
}

fn schema_version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
}

fn table_counts(conn: &Connection) -> rusqlite::Result<Vec<TableCount>> {
    let names: Vec<String> = conn
        .prepare(
            "SELECT name FROM sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    names
        .into_iter()
        .map(|table| {
            let sql = format!("SELECT count(*) FROM \"{}\"", table.replace('"', "\"\""));
            let rows = conn.query_row(&sql, [], |r| r.get(0))?;
            Ok(TableCount { table, rows })
        })
        .collect()
}

fn copy_and_verify(src: &str, dst: &str, key: Option<&str>) -> Result<BackupReport, String> {
    // Read-write without CREATE: a WAL reader needs to write the -shm file,
    // and a mistyped path must not create an empty DB. Nothing is written.
    let source = open_existing(
        src,
        key,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let mut dest = open_existing(
        dst,
        key,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;

    // One read transaction: the counts and the copied pages are one snapshot.
    let err = |e: rusqlite::Error| format!("back up {src}: {e}");
    source.execute_batch("BEGIN").map_err(err)?;
    let snapshot = (|| {
        let version = schema_version(&source)?;
        let tables = table_counts(&source)?;
        let backup = Backup::new(&source, &mut dest)?;
        match backup.step(-1)? {
            rusqlite::backup::StepResult::Done => Ok((version, tables)),
            other => Err(rusqlite::Error::ToSqlConversionFailure(
                format!("backup step stopped early: {other:?}").into(),
            )),
        }
    })();
    let _ = source.execute_batch("COMMIT");
    drop(source);
    let (version, tables) = snapshot.map_err(err)?;

    // The copy carries the source's WAL flag; a backup should be one file.
    dest.query_row("PRAGMA journal_mode=DELETE", [], |r| r.get::<_, String>(0))
        .map_err(|e| format!("{dst}: {e}"))?;
    drop(dest);

    // Verify from a fresh, read-only connection.
    let check = open_existing(dst, key, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let integrity: String = check
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .map_err(|e| format!("verify {dst}: {e}"))?;
    if integrity != "ok" {
        return Err(format!("verify {dst}: integrity_check said {integrity}"));
    }
    let got_version = schema_version(&check).map_err(|e| format!("verify {dst}: {e}"))?;
    if got_version != version {
        return Err(format!(
            "verify {dst}: schema version {got_version}, source had {version}"
        ));
    }
    let got_tables = table_counts(&check).map_err(|e| format!("verify {dst}: {e}"))?;
    if got_tables != tables {
        return Err(format!("verify {dst}: row counts differ from the source"));
    }
    drop(check);

    let bytes = std::fs::metadata(dst)
        .map_err(|e| format!("stat {dst}: {e}"))?
        .len();
    Ok(BackupReport {
        path: dst.to_string(),
        bytes,
        schema_version: version,
        encrypted: key.is_some(),
        tables,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

    pub(super) struct TempDir(std::path::PathBuf);
    impl TempDir {
        pub(super) fn new(tag: &str) -> Self {
            let mut p = std::env::temp_dir();
            p.push(format!("vitals-backup-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
        pub(super) fn path(&self, name: &str) -> String {
            self.0.join(name).to_string_lossy().into_owned()
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub(super) fn add_sources(s: &Store, names: &[&str]) {
        for n in names {
            s.conn
                .execute("INSERT INTO sources (name) VALUES (?1)", [n])
                .unwrap();
        }
    }

    pub(super) fn count_sources(s: &Store) -> i64 {
        s.conn
            .query_row("SELECT count(*) FROM sources", [], |r| r.get(0))
            .unwrap()
    }

    fn rows(r: &BackupReport, table: &str) -> i64 {
        r.tables.iter().find(|t| t.table == table).unwrap().rows
    }

    #[test]
    fn copies_the_live_wal_db_while_it_is_open() {
        let dir = TempDir::new("wal");
        let (src, to) = (dir.path("live.db"), dir.path("snap.db"));
        // `live` stays open like the server; its few writes sit in the -wal
        // file, under the autocheckpoint.
        let live = Store::open(&src).unwrap();
        add_sources(&live, &["Synthetic Watch", "Synthetic Phone", "Scale"]);
        assert!(Path::new(&format!("{src}-wal")).exists());

        let r = backup_db(&src, &to, None).unwrap();
        assert_eq!(r.path, to);
        assert!(!r.encrypted);
        assert_eq!(rows(&r, "sources"), 3);
        assert!(!Path::new(&format!("{to}.partial")).exists());
        assert!(!Path::new(&format!("{to}-wal")).exists());
        let mode: String = Connection::open(&to)
            .unwrap()
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "delete");
        assert_eq!(count_sources(&Store::open(&to).unwrap()), 3);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&to).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        drop(live);
    }

    #[test]
    fn refuses_to_overwrite() {
        let dir = TempDir::new("exists");
        let (src, to) = (dir.path("live.db"), dir.path("snap.db"));
        Store::open(&src).unwrap();
        std::fs::write(&to, "keep me").unwrap();
        assert!(backup_db(&src, &to, None)
            .unwrap_err()
            .contains("already exists"));
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "keep me");
    }

    #[test]
    fn missing_source_creates_nothing() {
        let dir = TempDir::new("missing");
        let (src, to) = (dir.path("nope.db"), dir.path("snap.db"));
        assert!(backup_db(&src, &to, None).is_err());
        assert!(!Path::new(&src).exists(), "must not create the source");
        assert!(!Path::new(&to).exists());
    }
}

#[cfg(all(test, feature = "encryption"))]
mod encryption_tests {
    use super::tests::{add_sources, count_sources, TempDir};
    use super::*;
    use crate::Store;

    #[test]
    fn encrypted_backup_needs_the_same_key() {
        let dir = TempDir::new("enc");
        let (src, to) = (dir.path("live.db"), dir.path("snap.db"));
        let live = Store::open_encrypted(&src, "correct horse").unwrap();
        add_sources(&live, &["Synthetic Watch"]);

        let r = backup_db(&src, &to, Some("correct horse")).unwrap();
        assert!(r.encrypted);
        assert_eq!(
            count_sources(&Store::open_encrypted(&to, "correct horse").unwrap()),
            1
        );
        assert!(Store::open_encrypted(&to, "wrong key").is_err());
        assert!(Store::open(&to).is_err(), "the copy must stay encrypted");
        drop(live);
    }

    #[test]
    fn wrong_or_missing_key_fails_and_leaves_nothing() {
        let dir = TempDir::new("enc-bad");
        let src = dir.path("live.db");
        Store::open_encrypted(&src, "correct horse").unwrap();
        for key in [Some("wrong key"), None] {
            let to = dir.path("snap.db");
            assert!(backup_db(&src, &to, key).is_err());
            assert!(!Path::new(&to).exists());
            assert!(!Path::new(&format!("{to}.partial")).exists());
        }
    }
}
