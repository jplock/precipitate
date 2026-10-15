//! SQLite store. Queries are built with sea-query and executed with sqlx.
//! Schema changes are numbered SQL files under `migrations/`, embedded at
//! compile time and applied in order on open; sqlx records which have run
//! in `_sqlx_migrations`.
//!
//! Resources are stored as versions. A new row is written only when a
//! resource first appears or its content changes; otherwise the existing
//! row's `last_seen_seq` advances. Presence at scan `n` is
//! `first_seen_seq <= n <= last_seen_seq`.

pub mod export;
mod kinds;
mod query;
mod scans;
mod schedule;
mod schema;
mod workloads;

pub use kinds::{DispositionSource, EventKind, MigrationEventKind, Trigger};
pub use scans::{Overview, ScanDiff, ScanStats, ScanSummary};
pub use schedule::{
    Metrics, MigrationEvent, PairStatus, PlanEvent, PlanEventRecord,
    RESCHEDULE_REASONS, ScheduleVersion, Verification, WaveMetrics,
};
pub use workloads::{
    DispositionChange, GroupStats, MemberDetail, WorkloadSummary,
};

use camino::Utf8Path;
use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("database")]
    Sqlx(#[from] sqlx::Error),
    #[error("applying migrations")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("encoding resource as JSON")]
    Json(#[from] serde_json::Error),
    #[error("parsing stored timestamp")]
    Timestamp(#[from] jiff::Error),
    #[error("reading a stored name")]
    Name(#[from] precip_types::UnknownName),
    #[error(transparent)]
    Infallible(#[from] std::convert::Infallible),
    #[error("reading a weekday rule")]
    Weekdays(#[from] precip_engine::schedule::UnknownWeekday),
    #[error("{0}")]
    Invalid(String),
}

#[derive(Debug, Clone)]
pub struct Store {
    pool: SqlitePool,
    /// A pinned "today" for reproducible schedules; `None` means now.
    today_override: std::sync::Arc<std::sync::Mutex<Option<jiff::civil::Date>>>,
}

impl Store {
    /// Open or create the database at `path` and apply pending migrations.
    pub async fn open(path: &Utf8Path) -> Result<Self, DbError> {
        let options = SqliteConnectOptions::new()
            .filename(path.as_std_path())
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new().connect_with(options).await?;
        MIGRATOR.run(&pool).await?;
        Ok(Self { pool, today_override: std::sync::Arc::default() })
    }
}

#[cfg(test)]
mod tests;
