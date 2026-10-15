//! Recording scans: resources and relationships as versions, and what
//! changed between scans.

use crate::query::{Bound, RowExt};
use crate::schema::{
    RelationshipVersions, ResourceVersions, ScanTargets, Scans,
};
use crate::{DbError, Store};
use precip_types::Scan;
use precip_types::scan::{Relationship, Resource, TargetStatus};
use sea_query::{Expr, ExprTrait, Func, OnConflict, Order, Query};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::collections::{BTreeMap, HashMap};

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

/// SHA-256 over the canonical JSON of tags and the stored body.
/// `serde_json::Value` objects are sorted maps, so the encoding is
/// canonical.
pub(crate) fn content_hash(
    tags: &BTreeMap<String, String>,
    body: &serde_json::Value,
) -> Result<String, DbError> {
    let mut h = Sha256::new();
    h.update(serde_json::to_vec(tags)?);
    h.update(b"\n");
    h.update(serde_json::to_vec(body)?);
    Ok(format!("{:x}", h.finalize()))
}

/// The latest version row of one ARN.
pub(crate) struct Current {
    version: i64,
    content_hash: String,
    last_seen_seq: i64,
}

impl Store {
    /// Record a scan. Inserts the scan row, upserts resource and
    /// relationship versions, and returns what changed.
    pub async fn record_scan(&self, scan: &Scan) -> Result<ScanStats, DbError> {
        let mut tx = self.pool.begin().await?;

        (Query::insert()
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
                Expr::from(
                    Query::select()
                        .expr(
                            Func::coalesce([
                                Expr::col(Scans::Seq).max(),
                                Expr::val(0),
                            ])
                            .add(1),
                        )
                        .from(Scans::Table)
                        .take(),
                ),
                scan.started.to_string().into(),
                scan.finished.map(|t| t.to_string()).into(),
                scan.caller_arn.as_str().into(),
            ]))
        .bound()
        .execute(&mut *tx)
        .await?;

        let seq: i64 = (Query::select()
            .column(Scans::Seq)
            .from(Scans::Table)
            .and_where(Expr::col(Scans::Id).eq(scan.id.as_str())))
        .bound()
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
            (Query::insert()
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
                    t.account_id.as_str().into(),
                    t.region.as_str().into(),
                    status.into(),
                    error.into(),
                ]))
            .bound()
            .execute(&mut *tx)
            .await?;
        }

        // Latest version of every ARN, loaded once.
        let rows = (Query::select()
            .columns([
                ResourceVersions::Arn,
                ResourceVersions::Version,
                ResourceVersions::ContentHash,
                ResourceVersions::LastSeenSeq,
            ])
            .from(ResourceVersions::Table)
            .order_by(ResourceVersions::Arn, Order::Asc)
            .order_by(ResourceVersions::Version, Order::Asc))
        .bound()
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
            let body = r.body.to_value()?;
            let hash = content_hash(&r.tags, &body)?;
            let next_version = match current.get(r.arn.as_str()) {
                Some(c)
                    if c.content_hash == hash && c.last_seen_seq == seq - 1 =>
                {
                    stats.unchanged += 1;
                    (Query::update()
                        .table(ResourceVersions::Table)
                        .value(ResourceVersions::LastSeenSeq, seq)
                        .and_where(
                            Expr::col(ResourceVersions::Arn).eq(r.arn.as_str()),
                        )
                        .and_where(
                            Expr::col(ResourceVersions::Version).eq(c.version),
                        ))
                    .bound()
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
            (Query::insert()
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
                    r.arn.as_str().into(),
                    next_version.into(),
                    r.kind().as_str().into(),
                    r.account_id.as_str().into(),
                    r.region.as_str().into(),
                    serde_json::to_string(&r.tags)?.into(),
                    serde_json::to_string(&body)?.into(),
                    hash.into(),
                    seq.into(),
                    seq.into(),
                ]))
            .bound()
            .execute(&mut *tx)
            .await?;
        }

        // Relationships have no content, so a row is either extended or
        // started.
        let rows = (Query::select()
            .columns([
                RelationshipVersions::FromArn,
                RelationshipVersions::Edge,
                RelationshipVersions::ToArn,
                RelationshipVersions::FirstSeenSeq,
                RelationshipVersions::LastSeenSeq,
            ])
            .from(RelationshipVersions::Table)
            .and_where(
                Expr::col(RelationshipVersions::LastSeenSeq).eq(seq - 1),
            ))
        .bound()
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
                rel.from_arn.to_string(),
                rel.edge.as_str().to_string(),
                rel.to_arn.to_string(),
            );
            let query = match open.get(&key) {
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
                    .bound(),
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
                    .bound(),
            };
            query.execute(&mut *tx).await?;
        }

        tx.commit().await?;
        Ok(stats)
    }

    /// All scans, newest first.
    pub async fn list_scans(&self) -> Result<Vec<ScanSummary>, DbError> {
        let rows = (Query::select()
            .columns([
                Scans::Id,
                Scans::Seq,
                Scans::Started,
                Scans::Finished,
                Scans::CallerArn,
            ])
            .from(Scans::Table)
            .order_by(Scans::Seq, Order::Desc))
        .bound()
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                let finished: Option<String> = row.try_get("finished")?;
                Ok(ScanSummary {
                    id: row.try_get("id")?,
                    seq: row.try_get("seq")?,
                    started: row.parsed("started")?,
                    finished: finished.map(|s| s.parse()).transpose()?,
                    caller_arn: row.try_get("caller_arn")?,
                })
            })
            .collect()
    }

    /// Content hash of every resource present at scan `seq`.
    pub(crate) async fn resources_at(
        &self,
        seq: i64,
    ) -> Result<BTreeMap<String, String>, DbError> {
        let rows = (Query::select()
            .columns([ResourceVersions::Arn, ResourceVersions::ContentHash])
            .from(ResourceVersions::Table)
            .and_where(Expr::col(ResourceVersions::FirstSeenSeq).lte(seq))
            .and_where(Expr::col(ResourceVersions::LastSeenSeq).gte(seq)))
        .bound()
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
        let row = (Query::select()
            .expr(Expr::col(ResourceVersions::Arn).count())
            .from(ResourceVersions::Table)
            .and_where(Expr::col(ResourceVersions::FirstSeenSeq).lte(seq))
            .and_where(Expr::col(ResourceVersions::LastSeenSeq).gte(seq)))
        .bound()
        .fetch_one(&self.pool)
        .await?;
        Ok(row.try_get(0)?)
    }

    /// Resource counts by kind at scan `seq`, plus distinct accounts and
    /// regions, for the overview.
    pub async fn overview(&self, seq: i64) -> Result<Overview, DbError> {
        let rows = (Query::select()
            .column(ResourceVersions::Kind)
            .expr_as(
                Expr::col(ResourceVersions::Arn).count(),
                sea_query::Alias::new("n"),
            )
            .from(ResourceVersions::Table)
            .and_where(Expr::col(ResourceVersions::FirstSeenSeq).lte(seq))
            .and_where(Expr::col(ResourceVersions::LastSeenSeq).gte(seq))
            .group_by_col(ResourceVersions::Kind)
            .order_by(ResourceVersions::Kind, Order::Asc))
        .bound()
        .fetch_all(&self.pool)
        .await?;
        let mut by_kind = BTreeMap::new();
        for row in rows {
            by_kind.insert(
                row.try_get::<String, _>("kind")?,
                row.try_get::<i64, _>("n")?,
            );
        }

        let row = (Query::select()
            .expr_as(
                Func::count_distinct(Expr::col(ResourceVersions::AccountId)),
                sea_query::Alias::new("accounts"),
            )
            .expr_as(
                Func::count_distinct(Expr::col(ResourceVersions::Region)),
                sea_query::Alias::new("regions"),
            )
            .from(ResourceVersions::Table)
            .and_where(Expr::col(ResourceVersions::FirstSeenSeq).lte(seq))
            .and_where(Expr::col(ResourceVersions::LastSeenSeq).gte(seq))
            .and_where(Expr::col(ResourceVersions::Kind).ne("account")))
        .bound()
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
        let rows = (Query::select()
            .columns([ResourceVersions::AccountId, ResourceVersions::Body])
            .from(ResourceVersions::Table)
            .and_where(Expr::col(ResourceVersions::Kind).eq("account"))
            .and_where(Expr::col(ResourceVersions::FirstSeenSeq).lte(seq))
            .and_where(Expr::col(ResourceVersions::LastSeenSeq).gte(seq)))
        .bound()
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
        let rows = (Query::select()
            .columns([
                ResourceVersions::Version,
                ResourceVersions::FirstSeenSeq,
                ResourceVersions::LastSeenSeq,
                ResourceVersions::Body,
            ])
            .from(ResourceVersions::Table)
            .and_where(Expr::col(ResourceVersions::Arn).eq(arn))
            .order_by(ResourceVersions::Version, Order::Asc))
        .bound()
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

    /// Resources present at scan `seq`.
    pub(crate) async fn live_resources(
        &self,
        seq: i64,
    ) -> Result<Vec<Resource>, DbError> {
        let rows = (Query::select()
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
            .and_where(Expr::col(ResourceVersions::LastSeenSeq).gte(seq)))
        .bound()
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                let tags: String = row.try_get("tags")?;
                let body: String = row.try_get("body")?;
                Ok(Resource {
                    arn: row.parsed("arn")?,
                    account_id: row.parsed("account_id")?,
                    region: row.parsed("region")?,
                    tags: serde_json::from_str(&tags)?,
                    body: precip_types::Body::from_value(
                        row.parsed("kind")?,
                        serde_json::from_str(&body)?,
                    )?,
                })
            })
            .collect()
    }

    /// Relationships present at scan `seq`.
    pub(crate) async fn live_relationships(
        &self,
        seq: i64,
    ) -> Result<Vec<Relationship>, DbError> {
        let rows = (Query::select()
            .columns([
                RelationshipVersions::FromArn,
                RelationshipVersions::Edge,
                RelationshipVersions::ToArn,
            ])
            .from(RelationshipVersions::Table)
            .and_where(Expr::col(RelationshipVersions::FirstSeenSeq).lte(seq))
            .and_where(Expr::col(RelationshipVersions::LastSeenSeq).gte(seq)))
        .bound()
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                let edge: String = row.try_get("edge")?;
                Ok(Relationship {
                    from_arn: row.parsed("from_arn")?,
                    edge: edge.parse()?,
                    to_arn: row.parsed("to_arn")?,
                })
            })
            .collect()
    }

    pub(crate) async fn seq_of(&self, scan_id: &str) -> Result<i64, DbError> {
        let row = (Query::select()
            .column(Scans::Seq)
            .from(Scans::Table)
            .and_where(Expr::col(Scans::Id).eq(scan_id)))
        .bound()
        .fetch_one(&self.pool)
        .await?;
        Ok(row.try_get("seq")?)
    }
}
