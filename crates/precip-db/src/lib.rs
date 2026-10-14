//! SQLite store. Queries are built with sea-query and executed with sqlx.
//! Schema changes are numbered SQL files under `migrations/`, embedded at
//! compile time and applied in order on open; sqlx records which have run
//! in `_sqlx_migrations`.
//!
//! Resources are stored as versions. A new row is written only when a
//! resource first appears or its content changes; otherwise the existing
//! row's `last_seen_seq` advances. Presence at scan `n` is
//! `first_seen_seq <= n <= last_seen_seq`.

mod schedule;
mod schema;

pub use schedule::{
    Metrics, MigrationEvent, PairStatus, PlanEvent, PlanEventRecord,
    RESCHEDULE_REASONS, ScheduleVersion, Verification, WaveMetrics,
    weekday_name, weekday_preset, weekdays_from_spec,
};

use camino::Utf8Path;
use precip_types::scan::{Relationship, Resource, TargetStatus};
use precip_types::{
    Disposition, GroupingConfig, Override, Scan, WorkloadMember,
};
use sea_query::{
    Expr, ExprTrait, OnConflict, Order, Query, SqliteQueryBuilder,
};
use sea_query_sqlx::SqlxBinder;
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::{Row, migrate::Migrator};
use std::collections::{BTreeMap, HashMap};

use schema::{
    DispositionHistory, RelationshipVersions, ResourceVersions, ScanTargets,
    Scans, Settings, WorkloadAliases, WorkloadEnvironments, WorkloadMembers,
    WorkloadOverrides, Workloads,
};

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// What grouping produced for one scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupStats {
    pub resources: usize,
    pub unassigned: usize,
    pub workloads: usize,
    pub pairs: usize,
}

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
    #[error("{0}")]
    Invalid(String),
}

#[derive(Debug, Clone)]
pub struct Store {
    pool: SqlitePool,
    /// A pinned "today" for reproducible schedules; `None` means now.
    today_override: std::sync::Arc<std::sync::Mutex<Option<jiff::civil::Date>>>,
}

/// What recording a scan changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanStats {
    pub seq: i64,
    /// Resources seen for the first time.
    pub added: usize,
    /// Resources whose content differs from their previous version.
    pub changed: usize,
    /// Resources identical to the previous scan.
    pub unchanged: usize,
    /// Resources seen before, absent in the previous scan, back now.
    pub reappeared: usize,
}

/// One row of `scans`.
#[derive(Debug, Clone)]
pub struct ScanSummary {
    pub id: String,
    pub seq: i64,
    pub started: jiff::Timestamp,
    pub finished: Option<jiff::Timestamp>,
    pub caller_arn: String,
}

/// Counts for the overview screen at one scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overview {
    pub by_kind: BTreeMap<String, i64>,
    /// Accounts with at least one non-account resource.
    pub accounts: i64,
    pub regions: i64,
}

/// Resources that appeared, disappeared or changed between two scans.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// Present in both scans with a different content hash.
    pub changed: Vec<String>,
}

/// One change of a pair's disposition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispositionChange {
    pub disposition: Disposition,
    pub previous: Option<Disposition>,
    pub changed_at: jiff::Timestamp,
    /// `default` when the pair was first seen, `user` for a direct change,
    /// `bulk` for a default applied to every pair.
    pub source: String,
    pub note: Option<String>,
}

/// One resource placed in a workload and environment pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberDetail {
    pub arn: String,
    pub kind: String,
    /// For managed services, which service: lambda, dynamodb, rds, ...
    pub service: Option<String>,
    pub name: String,
    pub account_id: String,
    pub region: String,
    pub signal: String,
}

/// Resource count per workload and environment for one scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkloadSummary {
    pub workload: String,
    pub environment: String,
    pub disposition: Disposition,
    pub resources: i64,
    /// Distinct account ids the pair's resources live in, sorted.
    pub accounts: Vec<String>,
    /// Distinct regions the pair's resources live in, sorted.
    pub regions: Vec<String>,
}

fn enum_name<T: serde::Serialize>(v: T) -> Result<String, DbError> {
    Ok(serde_json::to_value(v)?
        .as_str()
        .map(str::to_string)
        .unwrap_or_default())
}

fn disposition_from_name(s: &str) -> Result<Disposition, DbError> {
    Ok(serde_json::from_value(serde_json::Value::String(s.to_string()))?)
}

/// SHA-256 over the canonical JSON of tags and body. `serde_json::Value`
/// objects are sorted maps, so the encoding is canonical.
fn content_hash(r: &Resource) -> Result<String, DbError> {
    let mut h = Sha256::new();
    h.update(serde_json::to_vec(&r.tags)?);
    h.update(b"\n");
    h.update(serde_json::to_vec(&r.body)?);
    Ok(format!("{:x}", h.finalize()))
}

/// The latest version row of one ARN.
struct Current {
    version: i64,
    content_hash: String,
    last_seen_seq: i64,
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

    /// Record a scan. Inserts the scan row, upserts resource and
    /// relationship versions, and returns what changed.
    pub async fn record_scan(&self, scan: &Scan) -> Result<ScanStats, DbError> {
        let mut tx = self.pool.begin().await?;

        let (sql, values) = Query::insert()
            .into_table(Scans::Table)
            .columns([
                Scans::Id,
                Scans::Seq,
                Scans::Started,
                Scans::Finished,
                Scans::CallerArn,
            ])
            .values_panic([
                scan.id.clone().into(),
                Expr::cust("(SELECT COALESCE(MAX(seq), 0) + 1 FROM scans)"),
                scan.started.to_string().into(),
                scan.finished.map(|t| t.to_string()).into(),
                scan.caller_arn.clone().into(),
            ])
            .build_sqlx(SqliteQueryBuilder);
        sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
            .execute(&mut *tx)
            .await?;

        let (sql, values) = Query::select()
            .column(Scans::Seq)
            .from(Scans::Table)
            .and_where(Expr::col(Scans::Id).eq(scan.id.as_str()))
            .build_sqlx(SqliteQueryBuilder);
        let seq: i64 = sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
            .fetch_one(&mut *tx)
            .await?
            .try_get("seq")?;

        for t in &scan.targets {
            let (status, error) = match &t.status {
                TargetStatus::Scanned => ("scanned", None),
                TargetStatus::Empty => ("empty", None),
                TargetStatus::Partial(e) => ("partial", Some(e.clone())),
                TargetStatus::Failed(e) => ("failed", Some(e.clone())),
            };
            let (sql, values) = Query::insert()
                .into_table(ScanTargets::Table)
                .columns([
                    ScanTargets::ScanId,
                    ScanTargets::AccountId,
                    ScanTargets::Region,
                    ScanTargets::Status,
                    ScanTargets::Error,
                ])
                .values_panic([
                    scan.id.clone().into(),
                    t.account_id.clone().into(),
                    t.region.clone().into(),
                    status.into(),
                    error.into(),
                ])
                .build_sqlx(SqliteQueryBuilder);
            sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
                .execute(&mut *tx)
                .await?;
        }

        // Latest version of every ARN, loaded once.
        let (sql, values) = Query::select()
            .columns([
                ResourceVersions::Arn,
                ResourceVersions::Version,
                ResourceVersions::ContentHash,
                ResourceVersions::LastSeenSeq,
            ])
            .from(ResourceVersions::Table)
            .order_by(ResourceVersions::Arn, Order::Asc)
            .order_by(ResourceVersions::Version, Order::Asc)
            .build_sqlx(SqliteQueryBuilder);
        let rows = sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
            .fetch_all(&mut *tx)
            .await?;
        let mut current: HashMap<String, Current> = HashMap::new();
        for row in rows {
            let arn: String = row.try_get("arn")?;
            current.insert(
                arn,
                Current {
                    version: row.try_get("version")?,
                    content_hash: row.try_get("content_hash")?,
                    last_seen_seq: row.try_get("last_seen_seq")?,
                },
            );
        }

        // The same ARN can arrive more than once in one scan: instance
        // type rows carry no account, so every account reports them. First
        // occurrence wins.
        let mut seen = std::collections::HashSet::new();
        let mut duplicates = 0usize;
        let resources: Vec<&Resource> = scan
            .resources
            .iter()
            .filter(|r| {
                let fresh = seen.insert(r.arn.as_str());
                if !fresh {
                    duplicates += 1;
                    tracing::debug!(arn = %r.arn, "duplicate resource in scan");
                }
                fresh
            })
            .collect();
        if duplicates > 0 {
            tracing::debug!(duplicates, "duplicate ARNs dropped");
        }

        let mut stats = ScanStats { seq, ..Default::default() };
        for r in resources {
            let hash = content_hash(r)?;
            let next_version = match current.get(&r.arn) {
                Some(c)
                    if c.content_hash == hash && c.last_seen_seq == seq - 1 =>
                {
                    stats.unchanged += 1;
                    let (sql, values) = Query::update()
                        .table(ResourceVersions::Table)
                        .value(ResourceVersions::LastSeenSeq, seq)
                        .and_where(
                            Expr::col(ResourceVersions::Arn).eq(r.arn.as_str()),
                        )
                        .and_where(
                            Expr::col(ResourceVersions::Version).eq(c.version),
                        )
                        .build_sqlx(SqliteQueryBuilder);
                    sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
                        .execute(&mut *tx)
                        .await?;
                    continue;
                }
                Some(c) if c.content_hash == hash => {
                    stats.reappeared += 1;
                    c.version + 1
                }
                Some(c) => {
                    stats.changed += 1;
                    c.version + 1
                }
                None => {
                    stats.added += 1;
                    1
                }
            };
            let (sql, values) = Query::insert()
                .into_table(ResourceVersions::Table)
                .columns([
                    ResourceVersions::Arn,
                    ResourceVersions::Version,
                    ResourceVersions::Kind,
                    ResourceVersions::AccountId,
                    ResourceVersions::Region,
                    ResourceVersions::Tags,
                    ResourceVersions::Body,
                    ResourceVersions::ContentHash,
                    ResourceVersions::FirstSeenSeq,
                    ResourceVersions::LastSeenSeq,
                ])
                .values_panic([
                    r.arn.clone().into(),
                    next_version.into(),
                    enum_name(r.kind)?.into(),
                    r.account_id.clone().into(),
                    r.region.clone().into(),
                    serde_json::to_string(&r.tags)?.into(),
                    serde_json::to_string(&r.body)?.into(),
                    hash.into(),
                    seq.into(),
                    seq.into(),
                ])
                .build_sqlx(SqliteQueryBuilder);
            sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
                .execute(&mut *tx)
                .await?;
        }

        // Relationships have no content, so a row is either extended or
        // started.
        let (sql, values) = Query::select()
            .columns([
                RelationshipVersions::FromArn,
                RelationshipVersions::Edge,
                RelationshipVersions::ToArn,
                RelationshipVersions::FirstSeenSeq,
                RelationshipVersions::LastSeenSeq,
            ])
            .from(RelationshipVersions::Table)
            .and_where(Expr::col(RelationshipVersions::LastSeenSeq).eq(seq - 1))
            .build_sqlx(SqliteQueryBuilder);
        let rows = sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
            .fetch_all(&mut *tx)
            .await?;
        let mut open: HashMap<(String, String, String), i64> = HashMap::new();
        for row in rows {
            open.insert(
                (
                    row.try_get("from_arn")?,
                    row.try_get("edge")?,
                    row.try_get("to_arn")?,
                ),
                row.try_get("first_seen_seq")?,
            );
        }
        for rel in &scan.relationships {
            let key = (
                rel.from_arn.clone(),
                enum_name(rel.edge)?,
                rel.to_arn.clone(),
            );
            let (sql, values) = match open.get(&key) {
                Some(first) => Query::update()
                    .table(RelationshipVersions::Table)
                    .value(RelationshipVersions::LastSeenSeq, seq)
                    .and_where(
                        Expr::col(RelationshipVersions::FromArn)
                            .eq(key.0.as_str()),
                    )
                    .and_where(
                        Expr::col(RelationshipVersions::Edge)
                            .eq(key.1.as_str()),
                    )
                    .and_where(
                        Expr::col(RelationshipVersions::ToArn)
                            .eq(key.2.as_str()),
                    )
                    .and_where(
                        Expr::col(RelationshipVersions::FirstSeenSeq)
                            .eq(*first),
                    )
                    .build_sqlx(SqliteQueryBuilder),
                None => Query::insert()
                    .into_table(RelationshipVersions::Table)
                    .columns([
                        RelationshipVersions::FromArn,
                        RelationshipVersions::Edge,
                        RelationshipVersions::ToArn,
                        RelationshipVersions::FirstSeenSeq,
                        RelationshipVersions::LastSeenSeq,
                    ])
                    .values_panic([
                        key.0.clone().into(),
                        key.1.clone().into(),
                        key.2.clone().into(),
                        seq.into(),
                        seq.into(),
                    ])
                    .on_conflict(
                        OnConflict::columns([
                            RelationshipVersions::FromArn,
                            RelationshipVersions::Edge,
                            RelationshipVersions::ToArn,
                            RelationshipVersions::FirstSeenSeq,
                        ])
                        .do_nothing()
                        .to_owned(),
                    )
                    .build_sqlx(SqliteQueryBuilder),
            };
            sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
                .execute(&mut *tx)
                .await?;
        }

        tx.commit().await?;
        Ok(stats)
    }

    /// All scans, newest first.
    pub async fn list_scans(&self) -> Result<Vec<ScanSummary>, DbError> {
        let (sql, values) = Query::select()
            .columns([
                Scans::Id,
                Scans::Seq,
                Scans::Started,
                Scans::Finished,
                Scans::CallerArn,
            ])
            .from(Scans::Table)
            .order_by(Scans::Seq, Order::Desc)
            .build_sqlx(SqliteQueryBuilder);
        let rows = sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| {
                let finished: Option<String> = row.try_get("finished")?;
                Ok(ScanSummary {
                    id: row.try_get("id")?,
                    seq: row.try_get("seq")?,
                    started: row.try_get::<String, _>("started")?.parse()?,
                    finished: finished.map(|s| s.parse()).transpose()?,
                    caller_arn: row.try_get("caller_arn")?,
                })
            })
            .collect()
    }

    /// Content hash of every resource present at scan `seq`.
    async fn resources_at(
        &self,
        seq: i64,
    ) -> Result<BTreeMap<String, String>, DbError> {
        let (sql, values) = Query::select()
            .columns([ResourceVersions::Arn, ResourceVersions::ContentHash])
            .from(ResourceVersions::Table)
            .and_where(Expr::col(ResourceVersions::FirstSeenSeq).lte(seq))
            .and_where(Expr::col(ResourceVersions::LastSeenSeq).gte(seq))
            .build_sqlx(SqliteQueryBuilder);
        let rows = sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| Ok((row.try_get("arn")?, row.try_get("content_hash")?)))
            .collect()
    }

    /// Compare an earlier scan with a later one by ARN.
    pub async fn diff_scans(
        &self,
        earlier: i64,
        later: i64,
    ) -> Result<ScanDiff, DbError> {
        let before = self.resources_at(earlier).await?;
        let after = self.resources_at(later).await?;
        let mut diff = ScanDiff::default();
        for (arn, hash) in &after {
            match before.get(arn) {
                None => diff.added.push(arn.clone()),
                Some(old) if old != hash => diff.changed.push(arn.clone()),
                Some(_) => {}
            }
        }
        for arn in before.keys() {
            if !after.contains_key(arn) {
                diff.removed.push(arn.clone());
            }
        }
        Ok(diff)
    }

    /// Number of resources present at scan `seq`.
    pub async fn resource_count(&self, seq: i64) -> Result<i64, DbError> {
        let (sql, values) = Query::select()
            .expr(Expr::col(ResourceVersions::Arn).count())
            .from(ResourceVersions::Table)
            .and_where(Expr::col(ResourceVersions::FirstSeenSeq).lte(seq))
            .and_where(Expr::col(ResourceVersions::LastSeenSeq).gte(seq))
            .build_sqlx(SqliteQueryBuilder);
        let row = sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.try_get(0)?)
    }

    /// Resource counts by kind at scan `seq`, plus distinct accounts and
    /// regions, for the overview.
    pub async fn overview(&self, seq: i64) -> Result<Overview, DbError> {
        let (sql, values) = Query::select()
            .column(ResourceVersions::Kind)
            .expr_as(
                Expr::col(ResourceVersions::Arn).count(),
                sea_query::Alias::new("n"),
            )
            .from(ResourceVersions::Table)
            .and_where(Expr::col(ResourceVersions::FirstSeenSeq).lte(seq))
            .and_where(Expr::col(ResourceVersions::LastSeenSeq).gte(seq))
            .group_by_col(ResourceVersions::Kind)
            .order_by(ResourceVersions::Kind, Order::Asc)
            .build_sqlx(SqliteQueryBuilder);
        let rows = sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
            .fetch_all(&self.pool)
            .await?;
        let mut by_kind = BTreeMap::new();
        for row in rows {
            by_kind.insert(
                row.try_get::<String, _>("kind")?,
                row.try_get::<i64, _>("n")?,
            );
        }

        let (sql, values) = Query::select()
            .expr_as(
                Expr::cust("COUNT(DISTINCT account_id)"),
                sea_query::Alias::new("accounts"),
            )
            .expr_as(
                Expr::cust("COUNT(DISTINCT region)"),
                sea_query::Alias::new("regions"),
            )
            .from(ResourceVersions::Table)
            .and_where(Expr::col(ResourceVersions::FirstSeenSeq).lte(seq))
            .and_where(Expr::col(ResourceVersions::LastSeenSeq).gte(seq))
            .and_where(Expr::col(ResourceVersions::Kind).ne("account"))
            .build_sqlx(SqliteQueryBuilder);
        let row = sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
            .fetch_one(&self.pool)
            .await?;
        Ok(Overview {
            by_kind,
            accounts: row.try_get("accounts")?,
            regions: row.try_get("regions")?,
        })
    }

    /// Account id to name, from the account resources present at `seq`.
    pub async fn account_names(
        &self,
        seq: i64,
    ) -> Result<BTreeMap<String, String>, DbError> {
        let (sql, values) = Query::select()
            .columns([ResourceVersions::AccountId, ResourceVersions::Body])
            .from(ResourceVersions::Table)
            .and_where(Expr::col(ResourceVersions::Kind).eq("account"))
            .and_where(Expr::col(ResourceVersions::FirstSeenSeq).lte(seq))
            .and_where(Expr::col(ResourceVersions::LastSeenSeq).gte(seq))
            .build_sqlx(SqliteQueryBuilder);
        let rows = sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
            .fetch_all(&self.pool)
            .await?;
        let mut names = BTreeMap::new();
        for row in rows {
            let id: String = row.try_get("account_id")?;
            let body: serde_json::Value =
                serde_json::from_str(&row.try_get::<String, _>("body")?)?;
            if let Some(name) = body.get("name").and_then(|v| v.as_str()) {
                names.insert(id, name.to_string());
            }
        }
        Ok(names)
    }

    /// Every version of one resource, oldest first, as (version,
    /// first_seen_seq, last_seen_seq, body).
    pub async fn history(
        &self,
        arn: &str,
    ) -> Result<Vec<(i64, i64, i64, serde_json::Value)>, DbError> {
        let (sql, values) = Query::select()
            .columns([
                ResourceVersions::Version,
                ResourceVersions::FirstSeenSeq,
                ResourceVersions::LastSeenSeq,
                ResourceVersions::Body,
            ])
            .from(ResourceVersions::Table)
            .and_where(Expr::col(ResourceVersions::Arn).eq(arn))
            .order_by(ResourceVersions::Version, Order::Asc)
            .build_sqlx(SqliteQueryBuilder);
        let rows = sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| {
                let body: String = row.try_get("body")?;
                Ok((
                    row.try_get("version")?,
                    row.try_get("first_seen_seq")?,
                    row.try_get("last_seen_seq")?,
                    serde_json::from_str(&body)?,
                ))
            })
            .collect()
    }

    /// The disposition given to every new workload and environment pair.
    pub async fn default_disposition(&self) -> Result<Disposition, DbError> {
        let (sql, values) = Query::select()
            .column(Settings::Value)
            .from(Settings::Table)
            .and_where(Expr::col(Settings::Key).eq("default_disposition"))
            .build_sqlx(SqliteQueryBuilder);
        let row = sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
            .fetch_one(&self.pool)
            .await?;
        let v: String = row.try_get("value")?;
        disposition_from_name(&v)
    }

    pub(crate) async fn project_default_disposition(
        &self,
        disposition: Disposition,
        apply_to_existing: bool,
        at: jiff::Timestamp,
    ) -> Result<(), DbError> {
        let name = enum_name(disposition)?;
        let mut tx = self.pool.begin().await?;
        let (sql, values) = Query::update()
            .table(Settings::Table)
            .value(Settings::Value, name.clone())
            .and_where(Expr::col(Settings::Key).eq("default_disposition"))
            .build_sqlx(SqliteQueryBuilder);
        sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        if apply_to_existing {
            let (sql, values) = Query::select()
                .columns([
                    WorkloadEnvironments::Workload,
                    WorkloadEnvironments::Environment,
                ])
                .from(WorkloadEnvironments::Table)
                .build_sqlx(SqliteQueryBuilder);
            let rows = sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
                .fetch_all(&self.pool)
                .await?;
            for row in rows {
                let w: String = row.try_get("workload")?;
                let e: String = row.try_get("environment")?;
                self.project_disposition(&w, &e, disposition, "bulk", None, at)
                    .await?;
            }
        }
        Ok(())
    }

    /// Group every resource present at a scan into workloads and
    /// environments, replacing that scan's rows in `workload_members`.
    /// Pairs seen for the first time get the default disposition.
    pub async fn group_scan(
        &self,
        scan_id: &str,
        cfg: &GroupingConfig,
    ) -> Result<GroupStats, DbError> {
        let seq = self.seq_of(scan_id).await?;
        let resources = self.live_resources(seq).await?;
        let relationships = self.live_relationships(seq).await?;
        let overrides = self.overrides().await?;
        let aliases = self.aliases().await?;
        let members = precip_engine::group::group(
            &precip_engine::group::Input {
                resources: &resources,
                relationships: &relationships,
                overrides: &overrides,
                aliases: &aliases,
            },
            cfg,
        );

        let default = enum_name(self.default_disposition().await?)?;
        let mut tx = self.pool.begin().await?;
        let (sql, values) = Query::delete()
            .from_table(WorkloadMembers::Table)
            .and_where(Expr::col(WorkloadMembers::ScanId).eq(scan_id))
            .build_sqlx(SqliteQueryBuilder);
        sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
            .execute(&mut *tx)
            .await?;

        let mut pairs = std::collections::BTreeSet::new();
        for m in &members {
            pairs.insert((m.workload.clone(), m.environment.clone()));
            let (sql, values) = Query::insert()
                .into_table(WorkloadMembers::Table)
                .columns([
                    WorkloadMembers::ScanId,
                    WorkloadMembers::Arn,
                    WorkloadMembers::Workload,
                    WorkloadMembers::Environment,
                    WorkloadMembers::Signal,
                ])
                .values_panic([
                    scan_id.into(),
                    m.arn.clone().into(),
                    m.workload.clone().into(),
                    m.environment.clone().into(),
                    enum_name(m.signal)?.into(),
                ])
                .build_sqlx(SqliteQueryBuilder);
            sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
                .execute(&mut *tx)
                .await?;
        }
        for (workload, environment) in &pairs {
            let (sql, values) = Query::insert()
                .into_table(Workloads::Table)
                .columns([Workloads::Name])
                .values_panic([workload.clone().into()])
                .on_conflict(
                    OnConflict::column(Workloads::Name).do_nothing().to_owned(),
                )
                .build_sqlx(SqliteQueryBuilder);
            sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
                .execute(&mut *tx)
                .await?;
            let (sql, values) = Query::insert()
                .into_table(WorkloadEnvironments::Table)
                .columns([
                    WorkloadEnvironments::Workload,
                    WorkloadEnvironments::Environment,
                    WorkloadEnvironments::Disposition,
                ])
                .values_panic([
                    workload.clone().into(),
                    environment.clone().into(),
                    default.clone().into(),
                ])
                .on_conflict(
                    OnConflict::columns([
                        WorkloadEnvironments::Workload,
                        WorkloadEnvironments::Environment,
                    ])
                    .do_nothing()
                    .to_owned(),
                )
                .build_sqlx(SqliteQueryBuilder);
            let inserted = sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            if inserted > 0 {
                let (sql, values) = Query::insert()
                    .into_table(DispositionHistory::Table)
                    .columns([
                        DispositionHistory::Workload,
                        DispositionHistory::Environment,
                        DispositionHistory::Disposition,
                        DispositionHistory::ChangedAt,
                        DispositionHistory::Source,
                    ])
                    .values_panic([
                        workload.clone().into(),
                        environment.clone().into(),
                        default.clone().into(),
                        jiff::Timestamp::now().to_string().into(),
                        "default".into(),
                    ])
                    .build_sqlx(SqliteQueryBuilder);
                sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;

        let unassigned = members
            .iter()
            .filter(|m| m.signal == precip_types::GroupingSignal::Unassigned)
            .count();
        let workloads = pairs
            .iter()
            .map(|(w, _)| w)
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        Ok(GroupStats {
            resources: members.len(),
            unassigned,
            workloads,
            pairs: pairs.len(),
        })
    }

    /// Resources present at scan `seq`.
    async fn live_resources(&self, seq: i64) -> Result<Vec<Resource>, DbError> {
        let (sql, values) = Query::select()
            .columns([
                ResourceVersions::Arn,
                ResourceVersions::Kind,
                ResourceVersions::AccountId,
                ResourceVersions::Region,
                ResourceVersions::Tags,
                ResourceVersions::Body,
            ])
            .from(ResourceVersions::Table)
            .and_where(Expr::col(ResourceVersions::FirstSeenSeq).lte(seq))
            .and_where(Expr::col(ResourceVersions::LastSeenSeq).gte(seq))
            .build_sqlx(SqliteQueryBuilder);
        let rows = sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| {
                let kind: String = row.try_get("kind")?;
                let tags: String = row.try_get("tags")?;
                let body: String = row.try_get("body")?;
                Ok(Resource {
                    arn: row.try_get("arn")?,
                    kind: serde_json::from_value(serde_json::Value::String(
                        kind,
                    ))?,
                    account_id: row.try_get("account_id")?,
                    region: row.try_get("region")?,
                    tags: serde_json::from_str(&tags)?,
                    body: serde_json::from_str(&body)?,
                })
            })
            .collect()
    }

    /// Relationships present at scan `seq`.
    async fn live_relationships(
        &self,
        seq: i64,
    ) -> Result<Vec<Relationship>, DbError> {
        let (sql, values) = Query::select()
            .columns([
                RelationshipVersions::FromArn,
                RelationshipVersions::Edge,
                RelationshipVersions::ToArn,
            ])
            .from(RelationshipVersions::Table)
            .and_where(Expr::col(RelationshipVersions::FirstSeenSeq).lte(seq))
            .and_where(Expr::col(RelationshipVersions::LastSeenSeq).gte(seq))
            .build_sqlx(SqliteQueryBuilder);
        let rows = sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| {
                let edge: String = row.try_get("edge")?;
                Ok(Relationship {
                    from_arn: row.try_get("from_arn")?,
                    edge: serde_json::from_value(serde_json::Value::String(
                        edge,
                    ))?,
                    to_arn: row.try_get("to_arn")?,
                })
            })
            .collect()
    }

    /// Workload names that map onto another workload.
    pub async fn aliases(&self) -> Result<BTreeMap<String, String>, DbError> {
        let (sql, values) = Query::select()
            .columns([WorkloadAliases::Alias, WorkloadAliases::Workload])
            .from(WorkloadAliases::Table)
            .build_sqlx(SqliteQueryBuilder);
        let rows = sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| Ok((row.try_get("alias")?, row.try_get("workload")?)))
            .collect()
    }

    /// Record that `alias` is the same workload as `workload`. Takes effect
    /// on the next grouping.
    pub(crate) async fn project_merge(
        &self,
        alias: &str,
        workload: &str,
    ) -> Result<(), DbError> {
        let (sql, values) = Query::insert()
            .into_table(WorkloadAliases::Table)
            .columns([WorkloadAliases::Alias, WorkloadAliases::Workload])
            .values_panic([alias.into(), workload.into()])
            .on_conflict(
                OnConflict::column(WorkloadAliases::Alias)
                    .update_column(WorkloadAliases::Workload)
                    .to_owned(),
            )
            .build_sqlx(SqliteQueryBuilder);
        sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn seq_of(&self, scan_id: &str) -> Result<i64, DbError> {
        let (sql, values) = Query::select()
            .column(Scans::Seq)
            .from(Scans::Table)
            .and_where(Expr::col(Scans::Id).eq(scan_id))
            .build_sqlx(SqliteQueryBuilder);
        let row = sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.try_get("seq")?)
    }

    /// Every placed resource of a scan.
    pub async fn members(
        &self,
        scan_id: &str,
    ) -> Result<Vec<WorkloadMember>, DbError> {
        let (sql, values) = Query::select()
            .columns([
                WorkloadMembers::Arn,
                WorkloadMembers::Workload,
                WorkloadMembers::Environment,
                WorkloadMembers::Signal,
            ])
            .from(WorkloadMembers::Table)
            .and_where(Expr::col(WorkloadMembers::ScanId).eq(scan_id))
            .order_by(WorkloadMembers::Workload, Order::Asc)
            .order_by(WorkloadMembers::Environment, Order::Asc)
            .order_by(WorkloadMembers::Arn, Order::Asc)
            .build_sqlx(SqliteQueryBuilder);
        let rows = sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| {
                let signal: String = row.try_get("signal")?;
                Ok(WorkloadMember {
                    arn: row.try_get("arn")?,
                    workload: row.try_get("workload")?,
                    environment: row.try_get("environment")?,
                    signal: serde_json::from_value(serde_json::Value::String(
                        signal,
                    ))?,
                })
            })
            .collect()
    }

    /// Set the disposition of one workload and environment pair. Every
    /// resource in the pair takes this disposition.
    /// What a pair is set to now, if the pair exists.
    pub async fn disposition_of(
        &self,
        workload: &str,
        environment: &str,
    ) -> Result<Option<Disposition>, DbError> {
        let (sql, values) = Query::select()
            .column(WorkloadEnvironments::Disposition)
            .from(WorkloadEnvironments::Table)
            .and_where(Expr::col(WorkloadEnvironments::Workload).eq(workload))
            .and_where(
                Expr::col(WorkloadEnvironments::Environment).eq(environment),
            )
            .build_sqlx(SqliteQueryBuilder);
        let row = sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|r| {
            disposition_from_name(&r.try_get::<String, _>("disposition")?)
        })
        .transpose()
    }

    /// Change one pair's disposition and append the change to its history.
    /// A change to the value it already has is not recorded.
    pub(crate) async fn project_disposition(
        &self,
        workload: &str,
        environment: &str,
        disposition: Disposition,
        source: &str,
        note: Option<&str>,
        at: jiff::Timestamp,
    ) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;
        let (sql, values) = Query::select()
            .column(WorkloadEnvironments::Disposition)
            .from(WorkloadEnvironments::Table)
            .and_where(Expr::col(WorkloadEnvironments::Workload).eq(workload))
            .and_where(
                Expr::col(WorkloadEnvironments::Environment).eq(environment),
            )
            .build_sqlx(SqliteQueryBuilder);
        let previous: Option<String> =
            sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
                .fetch_optional(&mut *tx)
                .await?
                .map(|row| row.try_get("disposition"))
                .transpose()?;
        let name = enum_name(disposition)?;
        if previous.as_deref() == Some(name.as_str()) {
            return Ok(());
        }
        let (sql, values) = Query::update()
            .table(WorkloadEnvironments::Table)
            .value(WorkloadEnvironments::Disposition, name.clone())
            .and_where(Expr::col(WorkloadEnvironments::Workload).eq(workload))
            .and_where(
                Expr::col(WorkloadEnvironments::Environment).eq(environment),
            )
            .build_sqlx(SqliteQueryBuilder);
        sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
            .execute(&mut *tx)
            .await?;
        let (sql, values) = Query::insert()
            .into_table(DispositionHistory::Table)
            .columns([
                DispositionHistory::Workload,
                DispositionHistory::Environment,
                DispositionHistory::Disposition,
                DispositionHistory::Previous,
                DispositionHistory::ChangedAt,
                DispositionHistory::Source,
                DispositionHistory::Note,
            ])
            .values_panic([
                workload.into(),
                environment.into(),
                name.into(),
                previous.into(),
                at.to_string().into(),
                source.into(),
                note.into(),
            ])
            .build_sqlx(SqliteQueryBuilder);
        sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// A pair's disposition changes, oldest first.
    pub async fn disposition_history(
        &self,
        workload: &str,
        environment: &str,
    ) -> Result<Vec<DispositionChange>, DbError> {
        let (sql, values) = Query::select()
            .columns([
                DispositionHistory::Disposition,
                DispositionHistory::Previous,
                DispositionHistory::ChangedAt,
                DispositionHistory::Source,
                DispositionHistory::Note,
            ])
            .from(DispositionHistory::Table)
            .and_where(Expr::col(DispositionHistory::Workload).eq(workload))
            .and_where(
                Expr::col(DispositionHistory::Environment).eq(environment),
            )
            .order_by(sea_query::Alias::new("id"), Order::Asc)
            .build_sqlx(SqliteQueryBuilder);
        let rows = sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| {
                let d: String = row.try_get("disposition")?;
                let prev: Option<String> = row.try_get("previous")?;
                Ok(DispositionChange {
                    disposition: disposition_from_name(&d)?,
                    previous: prev
                        .as_deref()
                        .map(disposition_from_name)
                        .transpose()?,
                    changed_at: row
                        .try_get::<String, _>("changed_at")?
                        .parse()?,
                    source: row.try_get("source")?,
                    note: row.try_get("note")?,
                })
            })
            .collect()
    }

    /// Resource counts per workload and environment pair for one scan,
    /// with the pair's disposition.
    pub async fn workload_summary(
        &self,
        scan_id: &str,
    ) -> Result<Vec<WorkloadSummary>, DbError> {
        let seq = self.seq_of(scan_id).await?;
        let (sql, values) = Query::select()
            .column((WorkloadMembers::Table, WorkloadMembers::Workload))
            .column((WorkloadMembers::Table, WorkloadMembers::Environment))
            .column((
                WorkloadEnvironments::Table,
                WorkloadEnvironments::Disposition,
            ))
            .expr_as(
                Expr::col((WorkloadMembers::Table, WorkloadMembers::Arn))
                    .count(),
                sea_query::Alias::new("resources"),
            )
            .expr_as(
                Expr::cust(
                    "GROUP_CONCAT(DISTINCT resource_versions.account_id)",
                ),
                sea_query::Alias::new("accounts"),
            )
            .expr_as(
                Expr::cust("GROUP_CONCAT(DISTINCT resource_versions.region)"),
                sea_query::Alias::new("regions"),
            )
            .from(WorkloadMembers::Table)
            .left_join(
                ResourceVersions::Table,
                Expr::col((ResourceVersions::Table, ResourceVersions::Arn))
                    .equals((WorkloadMembers::Table, WorkloadMembers::Arn))
                    .and(
                        Expr::col((
                            ResourceVersions::Table,
                            ResourceVersions::FirstSeenSeq,
                        ))
                        .lte(seq),
                    )
                    .and(
                        Expr::col((
                            ResourceVersions::Table,
                            ResourceVersions::LastSeenSeq,
                        ))
                        .gte(seq),
                    ),
            )
            .inner_join(
                WorkloadEnvironments::Table,
                Expr::col((
                    WorkloadEnvironments::Table,
                    WorkloadEnvironments::Workload,
                ))
                .equals((WorkloadMembers::Table, WorkloadMembers::Workload))
                .and(
                    Expr::col((
                        WorkloadEnvironments::Table,
                        WorkloadEnvironments::Environment,
                    ))
                    .equals((
                        WorkloadMembers::Table,
                        WorkloadMembers::Environment,
                    )),
                ),
            )
            .and_where(
                Expr::col((WorkloadMembers::Table, WorkloadMembers::ScanId))
                    .eq(scan_id),
            )
            .group_by_col((WorkloadMembers::Table, WorkloadMembers::Workload))
            .group_by_col((
                WorkloadMembers::Table,
                WorkloadMembers::Environment,
            ))
            .order_by(
                (WorkloadMembers::Table, WorkloadMembers::Workload),
                Order::Asc,
            )
            .order_by(
                (WorkloadMembers::Table, WorkloadMembers::Environment),
                Order::Asc,
            )
            .build_sqlx(SqliteQueryBuilder);
        let rows = sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| {
                let d: String = row.try_get("disposition")?;
                let split = |v: Option<String>| -> Vec<String> {
                    let mut out: Vec<String> = v
                        .unwrap_or_default()
                        .split(',')
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect();
                    out.sort();
                    out
                };
                Ok(WorkloadSummary {
                    workload: row.try_get("workload")?,
                    environment: row.try_get("environment")?,
                    disposition: disposition_from_name(&d)?,
                    resources: row.try_get("resources")?,
                    accounts: split(row.try_get("accounts")?),
                    regions: split(row.try_get("regions")?),
                })
            })
            .collect()
    }

    /// The resources of one pair at a scan, with kind and name, ordered
    /// by kind then name.
    pub async fn pair_members(
        &self,
        scan_id: &str,
        workload: &str,
        environment: &str,
    ) -> Result<Vec<MemberDetail>, DbError> {
        let seq = self.seq_of(scan_id).await?;
        let (sql, values) = Query::select()
            .column((WorkloadMembers::Table, WorkloadMembers::Arn))
            .column((WorkloadMembers::Table, WorkloadMembers::Signal))
            .column((ResourceVersions::Table, ResourceVersions::Kind))
            .column((ResourceVersions::Table, ResourceVersions::AccountId))
            .column((ResourceVersions::Table, ResourceVersions::Region))
            .column((ResourceVersions::Table, ResourceVersions::Body))
            .column((ResourceVersions::Table, ResourceVersions::Tags))
            .from(WorkloadMembers::Table)
            .inner_join(
                ResourceVersions::Table,
                Expr::col((ResourceVersions::Table, ResourceVersions::Arn))
                    .equals((WorkloadMembers::Table, WorkloadMembers::Arn))
                    .and(
                        Expr::col((
                            ResourceVersions::Table,
                            ResourceVersions::FirstSeenSeq,
                        ))
                        .lte(seq),
                    )
                    .and(
                        Expr::col((
                            ResourceVersions::Table,
                            ResourceVersions::LastSeenSeq,
                        ))
                        .gte(seq),
                    ),
            )
            .and_where(
                Expr::col((WorkloadMembers::Table, WorkloadMembers::ScanId))
                    .eq(scan_id),
            )
            .and_where(
                Expr::col((WorkloadMembers::Table, WorkloadMembers::Workload))
                    .eq(workload),
            )
            .and_where(
                Expr::col((
                    WorkloadMembers::Table,
                    WorkloadMembers::Environment,
                ))
                .eq(environment),
            )
            .build_sqlx(SqliteQueryBuilder);
        let rows = sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
            .fetch_all(&self.pool)
            .await?;
        let mut out: Vec<MemberDetail> = rows
            .into_iter()
            .map(|row| {
                let body: serde_json::Value =
                    serde_json::from_str(&row.try_get::<String, _>("body")?)?;
                let tags: serde_json::Value =
                    serde_json::from_str(&row.try_get::<String, _>("tags")?)?;
                let arn: String = row.try_get("arn")?;
                // Name: the resource's own name, else its Name tag, else the
                // last path segment of the ARN.
                let name = body
                    .get("name")
                    .and_then(|v| v.as_str())
                    .or_else(|| tags.get("Name").and_then(|v| v.as_str()))
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        arn.rsplit(['/', ':'])
                            .next()
                            .unwrap_or(&arn)
                            .to_string()
                    });
                Ok(MemberDetail {
                    arn,
                    kind: row.try_get("kind")?,
                    service: body
                        .get("service")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    name,
                    account_id: row.try_get("account_id")?,
                    region: row.try_get("region")?,
                    signal: row.try_get("signal")?,
                })
            })
            .collect::<Result<_, DbError>>()?;
        out.sort_by(|a, b| (&a.kind, &a.name).cmp(&(&b.kind, &b.name)));
        Ok(out)
    }

    /// Disposition of every known pair.
    pub async fn pair_dispositions(
        &self,
    ) -> Result<BTreeMap<(String, String), Disposition>, DbError> {
        let (sql, values) = Query::select()
            .columns([
                WorkloadEnvironments::Workload,
                WorkloadEnvironments::Environment,
                WorkloadEnvironments::Disposition,
            ])
            .from(WorkloadEnvironments::Table)
            .build_sqlx(SqliteQueryBuilder);
        let rows = sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| {
                let d: String = row.try_get("disposition")?;
                Ok((
                    (row.try_get("workload")?, row.try_get("environment")?),
                    disposition_from_name(&d)?,
                ))
            })
            .collect()
    }

    /// Instances of sized pairs at a scan, with type and volume data
    /// resolved and fit assessed against `profile`.
    pub async fn sized_instances(
        &self,
        scan_id: &str,
        profile: &precip_types::Profile,
    ) -> Result<Vec<precip_engine::sizing::inputs::SizedInstance>, DbError>
    {
        let seq = self.seq_of(scan_id).await?;
        let resources = self.live_resources(seq).await?;
        let relationships = self.live_relationships(seq).await?;
        let members = self.members(scan_id).await?;
        let dispositions = self.pair_dispositions().await?;
        Ok(precip_engine::sizing::inputs::sized_instances(
            &resources,
            &relationships,
            &members,
            &dispositions,
            profile,
        ))
    }

    /// Remove pairs that no resource of `scan_id` belongs to, and
    /// workloads left with no pairs. With `keep_user_changed`, a pair whose
    /// disposition a user ever set is kept even when absent, so a partial
    /// scan cannot erase a decision. History rows are never removed.
    /// Returns how many pairs were removed.
    pub async fn prune_stale_pairs(
        &self,
        scan_id: &str,
        keep_user_changed: bool,
    ) -> Result<usize, DbError> {
        let live: std::collections::HashSet<(String, String)> = self
            .members(scan_id)
            .await?
            .into_iter()
            .map(|m| (m.workload, m.environment))
            .collect();
        let pairs = self.pair_dispositions().await?;

        let mut user_changed: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();
        if keep_user_changed {
            let (sql, values) = Query::select()
                .columns([
                    DispositionHistory::Workload,
                    DispositionHistory::Environment,
                ])
                .from(DispositionHistory::Table)
                .and_where(Expr::col(DispositionHistory::Source).ne("default"))
                .build_sqlx(SqliteQueryBuilder);
            for row in sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
                .fetch_all(&self.pool)
                .await?
            {
                user_changed.insert((
                    row.try_get("workload")?,
                    row.try_get("environment")?,
                ));
            }
        }

        let mut tx = self.pool.begin().await?;
        let mut removed = 0usize;
        for (pair, _) in pairs {
            if live.contains(&pair) || user_changed.contains(&pair) {
                continue;
            }
            let (sql, values) = Query::delete()
                .from_table(WorkloadEnvironments::Table)
                .and_where(
                    Expr::col(WorkloadEnvironments::Workload)
                        .eq(pair.0.as_str()),
                )
                .and_where(
                    Expr::col(WorkloadEnvironments::Environment)
                        .eq(pair.1.as_str()),
                )
                .build_sqlx(SqliteQueryBuilder);
            sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
                .execute(&mut *tx)
                .await?;
            removed += 1;
        }
        // Workloads with no environments left.
        sqlx::raw_sql(
            "DELETE FROM workloads WHERE name NOT IN (SELECT workload FROM workload_environments)",
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(removed)
    }

    /// Manual placements by ARN.
    pub async fn overrides(
        &self,
    ) -> Result<BTreeMap<String, Override>, DbError> {
        let (sql, values) = Query::select()
            .columns([
                WorkloadOverrides::Arn,
                WorkloadOverrides::Workload,
                WorkloadOverrides::Environment,
            ])
            .from(WorkloadOverrides::Table)
            .build_sqlx(SqliteQueryBuilder);
        let rows = sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| {
                Ok((
                    row.try_get("arn")?,
                    Override {
                        workload: row.try_get("workload")?,
                        environment: row.try_get("environment")?,
                    },
                ))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use precip_types::GroupingConfig;
    use precip_types::scan::{Edge, Relationship, ResourceKind, ScanTarget};

    async fn memory_store() -> Store {
        let options = SqliteConnectOptions::new().in_memory(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("in-memory sqlite");
        MIGRATOR.run(&pool).await.expect("migrations");
        let store = Store { pool, today_override: std::sync::Arc::default() };
        store.freeze_today(jiff::civil::date(2026, 10, 14));
        store
    }

    const I1: &str = "arn:aws:ec2:us-east-1:123456789012:instance/i-1";
    const I2: &str = "arn:aws:ec2:us-east-1:123456789012:instance/i-2";
    const V1: &str = "arn:aws:ec2:us-east-1:123456789012:volume/vol-1";

    fn instance(arn: &str, instance_type: &str) -> Resource {
        Resource {
            arn: arn.into(),
            kind: ResourceKind::Ec2Instance,
            account_id: "123456789012".into(),
            region: "us-east-1".into(),
            tags: BTreeMap::from([("Name".into(), "web".into())]),
            body: serde_json::json!({ "instance_type": instance_type }),
        }
    }

    fn scan(id: &str, day: u8, resources: Vec<Resource>) -> Scan {
        Scan {
            id: id.into(),
            started: format!("2026-10-{day:02}T00:00:00Z").parse().unwrap(),
            finished: None,
            caller_arn: "arn:aws:iam::123456789012:role/precip".into(),
            targets: vec![ScanTarget {
                account_id: "123456789012".into(),
                region: "us-east-1".into(),
                status: TargetStatus::Scanned,
            }],
            resources,
            relationships: vec![Relationship {
                from_arn: V1.into(),
                edge: Edge::AttachedTo,
                to_arn: I1.into(),
            }],
        }
    }

    #[tokio::test]
    async fn unchanged_resources_extend_their_version_instead_of_copying() {
        let store = memory_store().await;
        let s1 = store
            .record_scan(&scan("s1", 1, vec![instance(I1, "m6i.large")]))
            .await
            .expect("s1");
        assert_eq!(s1, ScanStats { seq: 1, added: 1, ..Default::default() });

        let s2 = store
            .record_scan(&scan("s2", 2, vec![instance(I1, "m6i.large")]))
            .await
            .expect("s2");
        assert_eq!(
            s2,
            ScanStats { seq: 2, unchanged: 1, ..Default::default() }
        );

        let history = store.history(I1).await.expect("history");
        assert_eq!(history.len(), 1, "one version row for two scans");
        assert_eq!((history[0].1, history[0].2), (1, 2));
        assert_eq!(store.resource_count(2).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn changed_resource_gets_a_new_version_and_old_one_closes() {
        let store = memory_store().await;
        store
            .record_scan(&scan("s1", 1, vec![instance(I1, "m6i.large")]))
            .await
            .unwrap();
        let s2 = store
            .record_scan(&scan("s2", 2, vec![instance(I1, "m6i.xlarge")]))
            .await
            .unwrap();
        assert_eq!(s2.changed, 1);

        let history = store.history(I1).await.unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!((history[0].0, history[0].1, history[0].2), (1, 1, 1));
        assert_eq!((history[1].0, history[1].1, history[1].2), (2, 2, 2));
        assert_eq!(history[1].3["instance_type"], "m6i.xlarge");

        let d = store.diff_scans(1, 2).await.unwrap();
        assert_eq!(d.changed, vec![I1.to_string()]);
    }

    #[tokio::test]
    async fn hosts_that_go_away_and_come_back_are_tracked() {
        let store = memory_store().await;
        store
            .record_scan(&scan(
                "s1",
                1,
                vec![instance(I1, "m6i.large"), instance(I2, "t3.micro")],
            ))
            .await
            .unwrap();
        let s2 = store
            .record_scan(&scan("s2", 2, vec![instance(I1, "m6i.large")]))
            .await
            .unwrap();
        assert_eq!((s2.unchanged, s2.added), (1, 0));
        let d = store.diff_scans(1, 2).await.unwrap();
        assert_eq!(d.removed, vec![I2.to_string()]);
        assert_eq!(store.resource_count(2).await.unwrap(), 1);

        let s3 = store
            .record_scan(&scan(
                "s3",
                3,
                vec![instance(I1, "m6i.large"), instance(I2, "t3.micro")],
            ))
            .await
            .unwrap();
        assert_eq!(s3.reappeared, 1);
        let history = store.history(I2).await.unwrap();
        assert_eq!(history.len(), 2);
        let d = store.diff_scans(2, 3).await.unwrap();
        assert_eq!(d.added, vec![I2.to_string()]);
    }

    // --- grouping fixtures ---

    const ACCT: &str = "123456789012";

    fn res(
        arn: &str,
        kind: ResourceKind,
        tags: &[(&str, &str)],
        body: serde_json::Value,
    ) -> Resource {
        Resource {
            arn: arn.into(),
            kind,
            account_id: ACCT.into(),
            region: "us-east-1".into(),
            tags: tags
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body,
        }
    }

    fn inst(id: &str, tags: &[(&str, &str)]) -> Resource {
        res(
            &format!("arn:aws:ec2:us-east-1:{ACCT}:instance/{id}"),
            ResourceKind::Ec2Instance,
            tags,
            serde_json::json!({ "instance_type": "t3.micro", "vpc_id": "vpc-1" }),
        )
    }

    fn rel(from: &str, edge: Edge, to: &str) -> Relationship {
        Relationship { from_arn: from.into(), edge, to_arn: to.into() }
    }

    fn fixture(
        id: &str,
        resources: Vec<Resource>,
        relationships: Vec<Relationship>,
    ) -> Scan {
        let mut s = scan(id, 1, resources);
        s.relationships = relationships;
        s
    }

    async fn grouped(s: Scan) -> (Store, Vec<WorkloadMember>) {
        let store = memory_store().await;
        store.record_scan(&s).await.expect("record");
        store
            .group_scan(&s.id, &GroupingConfig::default())
            .await
            .expect("group");
        let members = store.members(&s.id).await.expect("members");
        (store, members)
    }

    fn find<'a>(
        members: &'a [WorkloadMember],
        arn: &str,
    ) -> &'a WorkloadMember {
        members
            .iter()
            .find(|m| m.arn == arn)
            .unwrap_or_else(|| panic!("{arn} not placed"))
    }

    #[tokio::test]
    async fn pair_members_lists_each_resource_with_kind_name_and_signal() {
        let (store, _) = grouped(fixture(
            "s",
            vec![
                inst("i-1", &[("app", "billing-prod"), ("Name", "web-1")]),
                res(
                    V1,
                    ResourceKind::EbsVolume,
                    &[],
                    serde_json::json!({ "size_gib": 8 }),
                ),
            ],
            vec![rel(V1, Edge::AttachedTo, I1)],
        ))
        .await;
        let members = store.pair_members("s", "billing", "prod").await.unwrap();
        let summary: Vec<(String, String, String)> =
            members.into_iter().map(|m| (m.kind, m.name, m.signal)).collect();
        assert_eq!(
            summary,
            vec![
                ("ebs_volume".into(), "vol-1".into(), "attached".into()),
                ("ec2_instance".into(), "web-1".into(), "tag".into()),
            ]
        );
        assert!(
            store.pair_members("s", "billing", "dev").await.unwrap().is_empty()
        );
    }

    #[tokio::test]
    async fn disposition_changes_are_kept_as_lineage() {
        let (store, _) = grouped(fixture(
            "s",
            vec![inst("i-1", &[("app", "billing-prod")])],
            vec![],
        ))
        .await;
        store
            .set_disposition("billing", "prod", Disposition::Retire)
            .await
            .unwrap();
        store
            .set_disposition("billing", "prod", Disposition::Retire)
            .await
            .unwrap();
        store
            .set_disposition_with(
                "billing",
                "prod",
                Disposition::Rehost,
                "user",
                Some("stakeholder review"),
            )
            .await
            .unwrap();
        let h = store.disposition_history("billing", "prod").await.unwrap();
        let steps: Vec<(Option<Disposition>, Disposition, &str)> = h
            .iter()
            .map(|c| (c.previous, c.disposition, c.source.as_str()))
            .collect();
        assert_eq!(
            steps,
            vec![
                (None, Disposition::Rehost, "default"),
                (Some(Disposition::Rehost), Disposition::Retire, "user"),
                (Some(Disposition::Retire), Disposition::Rehost, "user"),
            ],
            "unchanged sets are not recorded"
        );
        assert_eq!(h[2].note.as_deref(), Some("stakeholder review"));
        assert!(h[0].changed_at <= h[2].changed_at);
    }

    #[tokio::test]
    async fn pruning_drops_untouched_stale_pairs_and_keeps_user_decisions() {
        let (store, _) = grouped(fixture(
            "s1",
            vec![
                inst("i-1", &[("app", "old-prod")]),
                inst("i-2", &[("app", "gone-prod")]),
                inst("i-3", &[("app", "kept-prod")]),
            ],
            vec![],
        ))
        .await;
        store
            .set_disposition("kept", "prod", Disposition::Retain)
            .await
            .unwrap();
        // Next scan: only i-1 remains, regrouped under a new name.
        let mut s2 =
            fixture("s2", vec![inst("i-1", &[("app", "new-prod")])], vec![]);
        s2.started = "2026-10-02T00:00:00Z".parse().unwrap();
        store.record_scan(&s2).await.unwrap();
        store.group_scan("s2", &GroupingConfig::default()).await.unwrap();

        let removed = store.prune_stale_pairs("s2", true).await.unwrap();
        assert_eq!(
            removed, 2,
            "old and gone go; kept stays because a user set it"
        );
        let names: Vec<String> = store
            .pair_dispositions()
            .await
            .unwrap()
            .into_keys()
            .map(|(w, _)| w)
            .collect();
        assert_eq!(names, vec!["kept".to_string(), "new".to_string()]);
        assert!(
            !store.disposition_history("old", "prod").await.unwrap().is_empty(),
            "history is lineage, never pruned"
        );

        let removed = store.prune_stale_pairs("s2", false).await.unwrap();
        assert_eq!(removed, 1, "forced prune removes the user-set pair too");
    }

    #[tokio::test]
    async fn waves_spans_and_rollback_round_trip() {
        use jiff::civil::date;
        use precip_engine::schedule::Status;
        let (store, _) = grouped(fixture(
            "s",
            vec![
                inst("i-1", &[("app", "shop-prod")]),
                inst("i-2", &[("app", "shop-dev")]),
                inst("i-3", &[("app", "erp-prod")]),
            ],
            vec![],
        ))
        .await;
        let mut rules = store.global_rules().await.unwrap();
        rules.per_day = 2;
        rules.per_week = 10;
        rules.nonprod_before_prod = false;
        store.set_global_rules(&rules).await.unwrap();
        store.set_wave(2, "finance", Some(date(2026, 12, 1))).await.unwrap();
        store.set_pair_plan("erp", "prod", 2, 3, 2).await.unwrap();
        assert!(matches!(
            store.set_pair_plan("erp", "prod", 0, 1, 1).await,
            Err(DbError::Invalid(_))
        ));
        let waves = store.waves().await.unwrap();
        assert_eq!(waves.len(), 2);
        assert_eq!(waves[1].name, "finance");
        assert!(matches!(store.remove_wave(1).await, Err(DbError::Invalid(_))));
        store.set_wave(3, "spare", None).await.unwrap();
        assert_eq!(store.remove_wave(3).await.unwrap(), 0, "nothing was in it");
        assert_eq!(store.waves().await.unwrap().len(), 2);

        let today = date(2026, 10, 14);
        let (_, s) = store.compute_schedule(today, "t").await.unwrap();
        let erp = s.slots.iter().find(|x| x.workload == "erp").unwrap();
        assert_eq!(erp.wave, 2);
        assert_eq!(
            (erp.date, erp.end),
            (date(2026, 10, 15), date(2026, 10, 17))
        );
        assert_eq!(erp.weight, 2);
        assert_eq!(s.wave_ends[&1], today, "both shop pairs fit on day one");

        // Rollback only applies to a migrated pair, reopens it, and shows
        // in the rates.
        assert!(matches!(
            store
                .set_pair_status(
                    "shop",
                    "dev",
                    Status::RolledBack,
                    None,
                    today,
                    None
                )
                .await,
            Err(DbError::Invalid(_))
        ));
        store
            .set_pair_status(
                "shop",
                "dev",
                Status::Migrated,
                Some(today),
                today,
                None,
            )
            .await
            .unwrap();
        store
            .set_pair_status(
                "shop",
                "dev",
                Status::RolledBack,
                None,
                today,
                Some("db lag"),
            )
            .await
            .unwrap();
        assert_eq!(
            store.pair_status("shop", "dev").await.unwrap().status,
            Status::RolledBack
        );
        let (_, s) = store.compute_schedule(today, "t").await.unwrap();
        let dev = s.slots.iter().find(|x| x.environment == "dev").unwrap();
        assert!(dev.reason.starts_with("rolled back"));
        let m = store.metrics(today).await.unwrap();
        assert_eq!(m.rolled_back, 1);
        assert!((m.rollback_rate - 1.0).abs() < 1e-9);
        assert_eq!(m.waves.len(), 2);
        assert_eq!((m.waves[1].number, m.waves[1].total), (2, 1));
        assert_eq!(m.waves[1].projected_end, Some(date(2026, 10, 17)));
    }

    #[tokio::test]
    async fn verification_flags_migrated_pairs_still_in_aws() {
        use jiff::civil::date;
        use precip_engine::schedule::Status;
        let (store, _) = grouped(fixture(
            "s1",
            vec![
                inst("i-1", &[("app", "shop-prod")]),
                inst("i-2", &[("app", "shop-dev")]),
            ],
            vec![],
        ))
        .await;
        let today = date(2026, 10, 14);
        store
            .set_pair_status(
                "shop",
                "prod",
                Status::Migrated,
                Some(today),
                today,
                None,
            )
            .await
            .unwrap();
        let v = store.verify_migrated("s1").await.unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!((v[0].workload.as_str(), v[0].remaining), ("shop", 1));

        // The next scan no longer sees the instance: nothing to flag. A
        // terminated instance does not count either.
        let mut gone = inst("i-2", &[("app", "shop-dev")]);
        gone.body["state"] = serde_json::json!("terminated");
        let s2 = fixture("s2", vec![gone], vec![]);
        store.record_scan(&s2).await.unwrap();
        store.group_scan("s2", &GroupingConfig::default()).await.unwrap();
        store
            .set_pair_status(
                "shop",
                "dev",
                Status::Migrated,
                Some(today),
                today,
                None,
            )
            .await
            .unwrap();
        assert!(store.verify_migrated("s2").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn senseless_status_changes_are_refused() {
        use jiff::civil::date;
        use precip_engine::schedule::Status;
        let (store, _) = grouped(fixture(
            "s",
            vec![
                inst("i-1", &[("app", "shop-prod")]),
                inst("i-2", &[("app", "shop-dev")]),
            ],
            vec![],
        ))
        .await;
        let today = date(2026, 10, 14);
        let status = |s: Status, d: Option<jiff::civil::Date>| {
            store.set_pair_status("shop", "prod", s, d, today, None)
        };
        let refused =
            |r: Result<(), DbError>| matches!(r, Err(DbError::Invalid(_)));

        // Nothing agreed yet: there is no date to miss.
        assert!(refused(status(Status::Missed, None).await));
        // Production waits for its other environments by rule, so no
        // agreed date for prod until dev has migrated.
        assert!(matches!(
            status(Status::Confirmed, Some(date(2026, 10, 20))).await,
            Err(DbError::Invalid(msg)) if msg.contains("waits for shop / dev")
        ));
        store
            .set_pair_status("shop", "dev", Status::Migrated, None, today, None)
            .await
            .unwrap();
        status(Status::Confirmed, Some(date(2026, 10, 20))).await.unwrap();
        // The agreed date is still ahead: it cannot have been missed.
        assert!(refused(status(Status::Missed, None).await));
        status(Status::Migrated, None).await.unwrap();
        assert!(refused(status(Status::Migrated, None).await));
        assert!(refused(
            status(Status::Confirmed, Some(date(2026, 11, 1))).await
        ));

        let b = precip_engine::schedule::Blackout {
            name: "freeze".into(),
            start: date(2026, 12, 20),
            end: date(2027, 1, 5),
            pair: None,
        };
        store.add_blackout(&b).await.unwrap();
        assert_eq!(store.blackouts().await.unwrap().len(), 1);
        store.remove_blackout(&b).await.unwrap();
        assert!(store.blackouts().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_event_log_rebuilds_the_plan() {
        use jiff::civil::date;
        use precip_engine::schedule::{Blackout, Status};
        let (store, _) = grouped(fixture(
            "s",
            vec![
                inst("i-1", &[("app", "shop-prod")]),
                inst("i-2", &[("app", "shop-dev")]),
            ],
            vec![],
        ))
        .await;
        let today = date(2026, 10, 14);
        let mut rules = store.global_rules().await.unwrap();
        rules.per_day = 2;
        rules.target_end = Some(date(2026, 12, 1));
        store.set_global_rules(&rules).await.unwrap();
        store
            .add_blackout(&Blackout {
                name: "freeze".into(),
                start: date(2026, 10, 15),
                end: date(2026, 10, 16),
                pair: None,
            })
            .await
            .unwrap();
        store.set_pair_plan("shop", "prod", 2, 2, 1).await.unwrap();
        store.set_wave(2, "later", None).await.unwrap();
        store.add_dependency("shop", "prod", "shop", "dev").await.unwrap();
        store
            .set_pair_status(
                "shop",
                "dev",
                Status::Confirmed,
                Some(today),
                today,
                None,
            )
            .await
            .unwrap();

        // The log opens with the defaults the migration backfilled, then
        // one event per change, each with the schedule version it made.
        let events = store.events().await.unwrap();
        assert_eq!(events.len(), 7);
        assert!(events[0].event.describe().starts_with("rules: 1 per day"));
        assert!(events[1..].iter().all(|e| e.version_id.is_some()));
        assert!(events[1..].iter().all(|e| e.ends.is_some()));
        assert!(events[1].event.describe().starts_with("rules: 2 per day"));
        let (_, before) = store.compute_schedule(today, "t").await.unwrap();
        let (latest_id, _, latest) =
            store.latest_schedule().await.unwrap().unwrap();
        assert_eq!(latest, before);
        assert!(latest_id > 0);

        // Dispositions, the default and merges are events too.
        store
            .set_disposition_with(
                "shop",
                "prod",
                Disposition::Retire,
                "user",
                Some("unused"),
            )
            .await
            .unwrap();
        store
            .set_default_disposition(Disposition::Replatform, false)
            .await
            .unwrap();
        store.merge_workload("shop-eu", "shop").await.unwrap();
        let (_, before) = store.compute_schedule(today, "t").await.unwrap();
        assert_eq!(before.slots.len(), 1, "retired prod left the calendar");

        // Wipe the projections and rebuild them from the log alone.
        let (_, after) = store.replay().await.unwrap();
        assert_eq!(
            store.disposition_of("shop", "prod").await.unwrap(),
            Some(Disposition::Retire)
        );
        assert_eq!(
            store.default_disposition().await.unwrap(),
            Disposition::Replatform
        );
        assert_eq!(store.aliases().await.unwrap()["shop-eu"], "shop");
        let history = store.disposition_history("shop", "prod").await.unwrap();
        assert_eq!(history.last().unwrap().note.as_deref(), Some("unused"));
        assert!(
            store
                .events()
                .await
                .unwrap()
                .iter()
                .any(|e| e.event.describe() == "shop-eu is part of shop")
        );
        assert_eq!(after.slots, before.slots);
        assert_eq!(store.global_rules().await.unwrap(), rules);
        assert_eq!(store.blackouts().await.unwrap().len(), 1);
        assert_eq!(store.waves().await.unwrap()[1].name, "later");
        assert_eq!(
            store.pair_status("shop", "dev").await.unwrap().status,
            Status::Confirmed
        );
        let m = store.metrics(today).await.unwrap();
        assert_eq!(
            m.scheduled, 1,
            "the status event replayed into migration_events"
        );
    }

    #[tokio::test]
    async fn reconfirming_frees_the_old_date_for_another_pair() {
        use jiff::civil::date;
        use precip_engine::schedule::Status;
        let (store, _) = grouped(fixture(
            "s",
            vec![
                inst("i-1", &[("app", "shop-prod")]),
                inst("i-2", &[("app", "shop-dev")]),
            ],
            vec![],
        ))
        .await;
        let mut rules = store.global_rules().await.unwrap();
        rules.per_day = 1;
        rules.nonprod_before_prod = false;
        store.set_global_rules(&rules).await.unwrap();
        let today = date(2026, 10, 14);
        let dev_date = |s: &precip_engine::schedule::Schedule| {
            s.slots.iter().find(|x| x.environment == "dev").unwrap().date
        };

        store
            .set_pair_status(
                "shop",
                "prod",
                Status::Confirmed,
                Some(today),
                today,
                None,
            )
            .await
            .unwrap();
        let (_, s1) = store.compute_schedule(today, "t").await.unwrap();
        assert_eq!(dev_date(&s1), date(2026, 10, 15), "today is taken by prod");

        store
            .set_pair_status(
                "shop",
                "prod",
                Status::Confirmed,
                Some(date(2026, 10, 20)),
                today,
                None,
            )
            .await
            .unwrap();
        let (_, s2) = store.compute_schedule(today, "t").await.unwrap();
        assert_eq!(
            dev_date(&s2),
            today,
            "prod moved on, so today is open again"
        );
        assert_eq!(
            s2.slots.iter().filter(|x| x.locked).count(),
            1,
            "only one confirmed slot for prod"
        );
    }

    #[tokio::test]
    async fn schedule_round_trips_rules_status_and_versions() {
        use jiff::civil::date;
        use precip_engine::schedule::{Blackout, Status};
        let (store, _) = grouped(fixture(
            "s",
            vec![
                inst("i-1", &[("app", "shop-prod")]),
                inst("i-2", &[("app", "shop-dev")]),
            ],
            vec![],
        ))
        .await;
        let mut rules = store.global_rules().await.unwrap();
        rules.per_day = 1;
        rules.target_end = Some(date(2026, 10, 20));
        // This test agrees prod's date while dev is still open.
        rules.nonprod_before_prod = false;
        store.set_global_rules(&rules).await.unwrap();
        store
            .add_blackout(&Blackout {
                name: "freeze".into(),
                start: date(2026, 10, 15),
                end: date(2026, 10, 16),
                pair: None,
            })
            .await
            .unwrap();
        store
            .set_pair_rule(
                "shop",
                "prod",
                &[jiff::civil::Weekday::Saturday],
                None,
                0,
            )
            .await
            .unwrap();

        let today = date(2026, 10, 14);
        let (v1, s1) = store.compute_schedule(today, "command").await.unwrap();
        let by: BTreeMap<&str, &precip_engine::schedule::Slot> =
            s1.slots.iter().map(|x| (x.environment.as_str(), x)).collect();
        assert_eq!(by["dev"].date, today);
        assert_eq!(by["prod"].date, date(2026, 10, 17), "saturday, after dev");
        assert_eq!(by["prod"].alternatives.len(), 2);

        // Confirming in the past is refused; confirming a future date holds.
        assert!(matches!(
            store
                .set_pair_status(
                    "shop",
                    "prod",
                    Status::Confirmed,
                    Some(date(2026, 10, 1)),
                    today,
                    None
                )
                .await,
            Err(DbError::Invalid(_))
        ));
        store
            .set_pair_status(
                "shop",
                "prod",
                Status::Confirmed,
                Some(date(2026, 10, 24)),
                today,
                Some("ok with owners"),
            )
            .await
            .unwrap();
        let (_, s2) =
            store.compute_schedule(today, "status_change").await.unwrap();
        let prod = s2.slots.iter().find(|x| x.environment == "prod").unwrap();
        assert!(prod.locked);
        assert_eq!(prod.date, date(2026, 10, 24));

        store
            .set_pair_status(
                "shop",
                "dev",
                Status::Migrated,
                Some(date(2026, 10, 14)),
                today,
                None,
            )
            .await
            .unwrap();

        // Time passes the confirmed date with no migration: back to the pool.
        let (_, s3) =
            store.compute_schedule(date(2026, 10, 26), "open").await.unwrap();
        assert_eq!(s3.expired, vec![("shop".to_string(), "prod".to_string())]);
        assert_eq!(
            store.pair_status("shop", "prod").await.unwrap().status,
            Status::Missed
        );
        let events: Vec<String> = store
            .migration_events("shop", "prod")
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.event)
            .collect();
        assert_eq!(events, vec!["confirmed", "missed"]);

        // Every applied event made a version of its own, plus the three
        // explicit computes.
        let versions = store.schedule_versions().await.unwrap();
        assert!(versions.len() >= 3);
        assert!(versions.iter().any(|v| v.id == v1));

        let m = store.metrics(date(2026, 10, 26)).await.unwrap();
        assert_eq!((m.total, m.migrated, m.missed), (2, 1, 1));
        assert_eq!(
            (m.scheduled, m.rescheduled),
            (1, 1),
            "only prod was ever confirmed, and it was missed"
        );
        assert!((m.reschedule_rate - 1.0).abs() < 1e-9);
        assert!(
            (m.first_time_success_rate - 1.0).abs() < 1e-9,
            "dev migrated without a miss"
        );
        assert_eq!(m.migrated_last_30_days, 1);
        assert_eq!(
            m.first_projected_end,
            Some(date(2026, 10, 15)),
            "the first version came from the rules change, before prod's weekend rule"
        );

        // Confirming another future date replaces the confirmed one and
        // is recorded as a reschedule; there is only ever one.
        let later = date(2026, 10, 26);
        store
            .set_pair_status(
                "shop",
                "prod",
                Status::Confirmed,
                Some(date(2026, 10, 31)),
                later,
                None,
            )
            .await
            .unwrap();
        store
            .set_pair_status(
                "shop",
                "prod",
                Status::Confirmed,
                Some(date(2026, 11, 7)),
                later,
                None,
            )
            .await
            .unwrap();
        let st = store.pair_status("shop", "prod").await.unwrap();
        assert_eq!(st.confirmed_date, Some(date(2026, 11, 7)));
        let events: Vec<String> = store
            .migration_events("shop", "prod")
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.event)
            .collect();
        assert_eq!(
            events,
            vec!["confirmed", "missed", "confirmed", "reconfirmed"]
        );
        store
            .set_pair_status(
                "shop",
                "prod",
                Status::Migrated,
                Some(date(2026, 11, 7)),
                date(2026, 11, 8),
                None,
            )
            .await
            .unwrap();
        let m = store.metrics(date(2026, 11, 8)).await.unwrap();
        assert_eq!((m.scheduled, m.rescheduled, m.migrated), (1, 1, 2));
        assert!((m.first_time_success_rate - 0.5).abs() < 1e-9);
        assert_eq!(
            m.average_slip_days, 14.0,
            "2026-10-24 first confirmed, migrated 2026-11-07"
        );
    }

    #[tokio::test]
    async fn merging_a_workload_applies_on_regroup() {
        let asg = "arn:aws:autoscaling:us-east-1:1:autoScalingGroup:x:autoScalingGroupName/vouch-us-prod";
        let (store, m) = grouped(fixture(
            "s",
            vec![
                res(
                    asg,
                    ResourceKind::AutoScalingGroup,
                    &[],
                    serde_json::json!({ "name": "vouch-us-prod" }),
                ),
                inst("i-1", &[]),
                inst("i-2", &[("Service", "vouch"), ("env", "prod")]),
            ],
            vec![rel(I1, Edge::MemberOfAsg, asg)],
        ))
        .await;
        assert_eq!(find(&m, I1).workload, "vouch-us");
        store.merge_workload("vouch-us", "vouch").await.unwrap();
        store.group_scan("s", &GroupingConfig::default()).await.unwrap();
        let m = store.members("s").await.unwrap();
        assert!(m.iter().all(|x| x.workload == "vouch"), "{m:?}");
        assert_eq!(store.workload_summary("s").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn manual_override_by_arn_wins_and_regrouping_replaces_rows() {
        let (store, m) = grouped(fixture(
            "s",
            vec![inst("i-1", &[("precip:workload", "x")])],
            vec![],
        ))
        .await;
        assert_eq!(find(&m, I1).workload, "x");
        let (sql, values) = Query::insert()
            .into_table(WorkloadOverrides::Table)
            .columns([
                WorkloadOverrides::Arn,
                WorkloadOverrides::Workload,
                WorkloadOverrides::Environment,
            ])
            .values_panic([I1.into(), "legacy".into(), "prod".into()])
            .build_sqlx(SqliteQueryBuilder);
        sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
            .execute(&store.pool)
            .await
            .unwrap();
        store.group_scan("s", &GroupingConfig::default()).await.unwrap();
        let m = store.members("s").await.unwrap();
        assert_eq!(m.len(), 1, "regrouping replaces, never duplicates");
        assert_eq!(
            (find(&m, I1).workload.as_str(), find(&m, I1).signal),
            ("legacy", precip_types::GroupingSignal::Override)
        );
    }

    #[tokio::test]
    async fn pairs_get_the_default_disposition_and_can_be_changed() {
        let (store, _) = grouped(fixture(
            "s",
            vec![inst("i-1", &[("app", "billing-prod")])],
            vec![],
        ))
        .await;
        let summary = store.workload_summary("s").await.unwrap();
        assert_eq!(
            summary,
            vec![WorkloadSummary {
                workload: "billing".into(),
                environment: "prod".into(),
                disposition: Disposition::Rehost,
                resources: 1,
                accounts: vec![ACCT.into()],
                regions: vec!["us-east-1".into()],
            }]
        );

        store
            .set_disposition("billing", "prod", Disposition::Relocate)
            .await
            .unwrap();
        assert_eq!(
            store.workload_summary("s").await.unwrap()[0].disposition,
            Disposition::Relocate
        );

        store
            .set_default_disposition(Disposition::Replatform, false)
            .await
            .unwrap();
        let s2 = {
            let mut s = fixture(
                "s2",
                vec![
                    inst("i-1", &[("app", "billing-prod")]),
                    inst("i-2", &[("app", "billing-dev")]),
                ],
                vec![],
            );
            s.started = "2026-10-02T00:00:00Z".parse().unwrap();
            s
        };
        store.record_scan(&s2).await.unwrap();
        store.group_scan("s2", &GroupingConfig::default()).await.unwrap();
        let by_env: BTreeMap<String, Disposition> = store
            .workload_summary("s2")
            .await
            .unwrap()
            .into_iter()
            .map(|x| (x.environment, x.disposition))
            .collect();
        assert_eq!(
            by_env["prod"],
            Disposition::Relocate,
            "existing pair untouched"
        );
        assert_eq!(
            by_env["dev"],
            Disposition::Replatform,
            "new pair takes default"
        );

        store.set_default_disposition(Disposition::Retain, true).await.unwrap();
        assert!(
            store
                .workload_summary("s2")
                .await
                .unwrap()
                .iter()
                .all(|x| x.disposition == Disposition::Retain)
        );
    }

    #[tokio::test]
    async fn duplicate_arns_within_a_scan_are_recorded_once() {
        let store = memory_store().await;
        let stats = store
            .record_scan(&scan(
                "s1",
                1,
                vec![instance(I1, "m6i.large"), instance(I1, "m6i.large")],
            ))
            .await
            .expect("duplicate must not fail the scan");
        assert_eq!(stats.added, 1);
        assert_eq!(store.resource_count(1).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn recording_the_same_scan_id_twice_is_rejected() {
        let store = memory_store().await;
        let s = scan("s1", 1, vec![]);
        store.record_scan(&s).await.expect("first");
        let err = store.record_scan(&s).await.unwrap_err();
        assert!(matches!(err, DbError::Sqlx(_)));
    }
}
