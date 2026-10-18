//! vitals core: the pure domain for a local Apple Health archive — export
//! parsing, the SQLite store, daily rollups and the read queries. No network
//! and no GUI (same rule as outflow's core).

pub mod geo;
pub mod health_export;
pub mod import;
pub mod query;
pub mod rollup;
pub mod secrets;
pub mod store;
pub mod time;
pub mod units;

pub use import::{import_path, ImportReport};
pub use store::Store;
