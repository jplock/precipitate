//! Grouping a scan into workloads, and the disposition of each workload
//! and environment pair.

use crate::kinds::{DispositionSource, Setting};
use crate::query::{Bound, RowExt};
use crate::schema::{
    DispositionHistory, ResourceVersions, Settings, WorkloadAliases,
    WorkloadEnvironments, WorkloadMembers, WorkloadOverrides, Workloads,
};
use crate::{DbError, Store};
use precip_engine::sizing::inputs::{self, SizedInstance};
use precip_engine::{group, terraform};
use precip_types::{
    Arn, Disposition, GroupingConfig, PairKey, WorkloadMember, WorkloadName,
};
use sea_query::{
    Expr, ExprTrait, Func, IntoColumnRef, OnConflict, Order, Query,
};
use sqlx::Row;
use sqlx::sqlite::SqliteConnection;
use std::collections::BTreeMap;

/// The settings key holding the disposition new pairs get.
/// The history source of a disposition a pair got when first seen.
/// What grouping produced for one scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupStats {
    pub resources: usize,
    pub unassigned: usize,
    pub workloads: usize,
    pub pairs: usize,
}

/// One change of a pair's disposition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispositionChange {
    pub disposition: Disposition,
    pub previous: Option<Disposition>,
    pub changed_at: jiff::Timestamp,
    /// `default` when the pair was first seen, `user` for a direct change,
    /// `bulk` for a default applied to every pair.
    pub source: DispositionSource,
    pub note: Option<String>,
}

/// One resource placed in a workload and environment pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberDetail {
    pub arn: String,
    pub kind: String,
    /// For managed services, the provider's product: lambda, dynamodb,
    /// rds, ...
    pub service: Option<String>,
    pub name: String,
    pub account_id: String,
    pub region: String,
    pub signal: String,
}

/// Resource count per workload and environment for one scan.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
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

/// Every value of a column in the group, comma separated. Duplicates
/// are kept; callers that need distinct values remove them.
pub(crate) fn group_concat(col: impl IntoColumnRef) -> Expr {
    Func::cust(sea_query::Alias::new("GROUP_CONCAT")).arg(Expr::col(col)).into()
}

impl Store {
    /// The disposition given to every new workload and environment pair.
    pub async fn default_disposition(&self) -> Result<Disposition, DbError> {
        let row = (Query::select()
            .column(Settings::Value)
            .from(Settings::Table)
            .and_where(
                Expr::col(Settings::Key)
                    .eq(Setting::DefaultDisposition.as_str()),
            ))
        .bound()
        .fetch_one(&self.pool)
        .await?;
        row.parsed("value")
    }

    pub(crate) async fn project_default_disposition(
        conn: &mut SqliteConnection,
        disposition: Disposition,
        apply_to_existing: bool,
        at: jiff::Timestamp,
    ) -> Result<(), DbError> {
        let name = disposition.as_str();
        (Query::update()
            .table(Settings::Table)
            .value(Settings::Value, name)
            .and_where(
                Expr::col(Settings::Key)
                    .eq(Setting::DefaultDisposition.as_str()),
            ))
        .bound()
        .execute(&mut *conn)
        .await?;
        if apply_to_existing {
            let rows = (Query::select()
                .columns([
                    WorkloadEnvironments::Workload,
                    WorkloadEnvironments::Environment,
                ])
                .from(WorkloadEnvironments::Table))
            .bound()
            .fetch_all(&mut *conn)
            .await?;
            for row in rows {
                let w: String = row.try_get("workload")?;
                let e: String = row.try_get("environment")?;
                Self::project_disposition(
                    conn,
                    &w,
                    &e,
                    disposition,
                    DispositionSource::Bulk,
                    None,
                    at,
                )
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
        let members = group::group(
            &group::Input {
                resources: &resources,
                relationships: &relationships,
                overrides: &overrides,
                aliases: &aliases,
            },
            cfg,
        );

        let default = self.default_disposition().await?.as_str();
        let mut tx = self.pool.begin().await?;
        (Query::delete()
            .from_table(WorkloadMembers::Table)
            .and_where(Expr::col(WorkloadMembers::ScanId).eq(scan_id)))
        .bound()
        .execute(&mut *tx)
        .await?;

        let mut pairs = std::collections::BTreeSet::new();
        for m in &members {
            pairs.insert(m.pair.clone());
            (Query::insert()
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
                    m.arn.as_str().into(),
                    m.pair.workload.as_str().into(),
                    m.pair.environment.as_str().into(),
                    m.signal.as_str().into(),
                ]))
            .bound()
            .execute(&mut *tx)
            .await?;
        }
        for PairKey { workload, environment } in &pairs {
            (Query::insert()
                .into_table(Workloads::Table)
                .columns([Workloads::Name])
                .values_panic([workload.as_str().into()])
                .on_conflict(
                    OnConflict::column(Workloads::Name).do_nothing().to_owned(),
                ))
            .bound()
            .execute(&mut *tx)
            .await?;
            let inserted = (Query::insert()
                .into_table(WorkloadEnvironments::Table)
                .columns([
                    WorkloadEnvironments::Workload,
                    WorkloadEnvironments::Environment,
                    WorkloadEnvironments::Disposition,
                ])
                .values_panic([
                    workload.as_str().into(),
                    environment.as_str().into(),
                    default.into(),
                ])
                .on_conflict(
                    OnConflict::columns([
                        WorkloadEnvironments::Workload,
                        WorkloadEnvironments::Environment,
                    ])
                    .do_nothing()
                    .to_owned(),
                ))
            .bound()
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if inserted > 0 {
                (Query::insert()
                    .into_table(DispositionHistory::Table)
                    .columns([
                        DispositionHistory::Workload,
                        DispositionHistory::Environment,
                        DispositionHistory::Disposition,
                        DispositionHistory::ChangedAt,
                        DispositionHistory::Source,
                    ])
                    .values_panic([
                        workload.as_str().into(),
                        environment.as_str().into(),
                        default.into(),
                        jiff::Timestamp::now().to_string().into(),
                        DispositionSource::Default.as_str().into(),
                    ]))
                .bound()
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
            .map(|p| &p.workload)
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        Ok(GroupStats {
            resources: members.len(),
            unassigned,
            workloads,
            pairs: pairs.len(),
        })
    }

    /// Workload names that map onto another workload.
    pub async fn aliases(
        &self,
    ) -> Result<BTreeMap<WorkloadName, WorkloadName>, DbError> {
        let rows = (Query::select()
            .columns([WorkloadAliases::Alias, WorkloadAliases::Workload])
            .from(WorkloadAliases::Table))
        .bound()
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok((
                    WorkloadName::from(row.try_get::<String, _>("alias")?),
                    WorkloadName::from(row.try_get::<String, _>("workload")?),
                ))
            })
            .collect()
    }

    /// Record that `alias` is the same workload as `workload`. Takes effect
    /// on the next grouping.
    pub(crate) async fn project_merge(
        conn: &mut SqliteConnection,
        alias: &str,
        workload: &str,
    ) -> Result<(), DbError> {
        (Query::insert()
            .into_table(WorkloadAliases::Table)
            .columns([WorkloadAliases::Alias, WorkloadAliases::Workload])
            .values_panic([alias.into(), workload.into()])
            .on_conflict(
                OnConflict::column(WorkloadAliases::Alias)
                    .update_column(WorkloadAliases::Workload)
                    .to_owned(),
            ))
        .bound()
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    /// Every placed resource of a scan.
    pub async fn members(
        &self,
        scan_id: &str,
    ) -> Result<Vec<WorkloadMember>, DbError> {
        let rows = (Query::select()
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
            .order_by(WorkloadMembers::Arn, Order::Asc))
        .bound()
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                let signal: String = row.try_get("signal")?;
                Ok(WorkloadMember {
                    arn: row.parsed("arn")?,
                    pair: PairKey::new(
                        row.try_get::<String, _>("workload")?,
                        row.try_get::<String, _>("environment")?,
                    ),
                    signal: signal.parse()?,
                })
            })
            .collect()
    }

    /// Set the disposition of one workload and environment pair. Every
    /// resource in the pair takes this disposition.
    /// What a pair is set to now, if the pair exists.
    pub(crate) async fn disposition_of(
        &self,
        workload: &str,
        environment: &str,
    ) -> Result<Option<Disposition>, DbError> {
        let row = (Query::select()
            .column(WorkloadEnvironments::Disposition)
            .from(WorkloadEnvironments::Table)
            .and_where(Expr::col(WorkloadEnvironments::Workload).eq(workload))
            .and_where(
                Expr::col(WorkloadEnvironments::Environment).eq(environment),
            ))
        .bound()
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| r.parsed::<Disposition>("disposition")).transpose()
    }

    /// Change one pair's disposition and append the change to its history.
    /// A change to the value it already has is not recorded.
    pub(crate) async fn project_disposition(
        conn: &mut SqliteConnection,
        workload: &str,
        environment: &str,
        disposition: Disposition,
        source: DispositionSource,
        note: Option<&str>,
        at: jiff::Timestamp,
    ) -> Result<(), DbError> {
        let previous: Option<String> = Query::select()
            .column(WorkloadEnvironments::Disposition)
            .from(WorkloadEnvironments::Table)
            .and_where(Expr::col(WorkloadEnvironments::Workload).eq(workload))
            .and_where(
                Expr::col(WorkloadEnvironments::Environment).eq(environment),
            )
            .bound()
            .fetch_optional(&mut *conn)
            .await?
            .map(|row| row.try_get("disposition"))
            .transpose()?;
        let name = disposition.as_str();
        if previous.as_deref() == Some(name) {
            return Ok(());
        }
        (Query::update()
            .table(WorkloadEnvironments::Table)
            .value(WorkloadEnvironments::Disposition, name)
            .and_where(Expr::col(WorkloadEnvironments::Workload).eq(workload))
            .and_where(
                Expr::col(WorkloadEnvironments::Environment).eq(environment),
            ))
        .bound()
        .execute(&mut *conn)
        .await?;
        (Query::insert()
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
                source.as_str().into(),
                note.into(),
            ]))
        .bound()
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    /// A pair's disposition changes, oldest first.
    pub async fn disposition_history(
        &self,
        workload: &str,
        environment: &str,
    ) -> Result<Vec<DispositionChange>, DbError> {
        let rows = (Query::select()
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
            .order_by(DispositionHistory::Id, Order::Asc))
        .bound()
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(DispositionChange {
                    disposition: row.parsed("disposition")?,
                    previous: row.parsed_opt::<Disposition>("previous")?,
                    changed_at: row
                        .try_get::<String, _>("changed_at")?
                        .parse()?,
                    source: row.parsed("source")?,
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
        let rows = (Query::select()
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
                group_concat((
                    ResourceVersions::Table,
                    ResourceVersions::AccountId,
                )),
                sea_query::Alias::new("accounts"),
            )
            .expr_as(
                group_concat((
                    ResourceVersions::Table,
                    ResourceVersions::Region,
                )),
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
            ))
        .bound()
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                let split = |v: Option<String>| -> Vec<String> {
                    let mut out: Vec<String> = v
                        .unwrap_or_default()
                        .split(',')
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect();
                    out.sort();
                    out.dedup();
                    out
                };
                Ok(WorkloadSummary {
                    workload: row.try_get("workload")?,
                    environment: row.try_get("environment")?,
                    disposition: row.parsed("disposition")?,
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
        let rows = (Query::select()
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
            ))
        .bound()
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
                        .get("product")
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
    pub(crate) async fn pair_dispositions(
        &self,
    ) -> Result<BTreeMap<PairKey, Disposition>, DbError> {
        let mut conn = self.pool.acquire().await?;
        Self::pair_dispositions_in(&mut conn).await
    }

    pub(crate) async fn pair_dispositions_in(
        conn: &mut SqliteConnection,
    ) -> Result<BTreeMap<PairKey, Disposition>, DbError> {
        let rows = (Query::select()
            .columns([
                WorkloadEnvironments::Workload,
                WorkloadEnvironments::Environment,
                WorkloadEnvironments::Disposition,
            ])
            .from(WorkloadEnvironments::Table))
        .bound()
        .fetch_all(&mut *conn)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok((
                    PairKey::new(
                        row.try_get::<String, _>("workload")?,
                        row.try_get::<String, _>("environment")?,
                    ),
                    row.parsed("disposition")?,
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
    ) -> Result<Vec<SizedInstance>, DbError> {
        let seq = self.seq_of(scan_id).await?;
        let resources = self.live_resources(seq).await?;
        let relationships = self.live_relationships(seq).await?;
        let members = self.members(scan_id).await?;
        let dispositions = self.pair_dispositions().await?;
        Ok(inputs::sized_instances(
            &resources,
            &relationships,
            &members,
            &dispositions,
            profile,
        ))
    }

    /// Oxide Terraform for one pair's resources at a scan.
    pub async fn pair_terraform(
        &self,
        scan_id: &str,
        pair: &PairKey,
    ) -> Result<String, DbError> {
        let seq = self.seq_of(scan_id).await?;
        let resources = self.live_resources(seq).await?;
        let relationships = self.live_relationships(seq).await?;
        let members = self.members(scan_id).await?;
        Ok(terraform::render(&terraform::Input {
            resources: &resources,
            relationships: &relationships,
            members: &members,
            pair,
        }))
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
        let live: std::collections::HashSet<PairKey> =
            self.members(scan_id).await?.into_iter().map(|m| m.pair).collect();
        let pairs = self.pair_dispositions().await?;

        let mut user_changed: std::collections::HashSet<PairKey> =
            std::collections::HashSet::new();
        if keep_user_changed {
            for row in (Query::select()
                .columns([
                    DispositionHistory::Workload,
                    DispositionHistory::Environment,
                ])
                .from(DispositionHistory::Table)
                .and_where(
                    Expr::col(DispositionHistory::Source)
                        .ne(DispositionSource::Default.as_str()),
                ))
            .bound()
            .fetch_all(&self.pool)
            .await?
            {
                user_changed.insert(PairKey::new(
                    row.try_get::<String, _>("workload")?,
                    row.try_get::<String, _>("environment")?,
                ));
            }
        }

        let mut tx = self.pool.begin().await?;
        let mut removed = 0usize;
        for (pair, _) in pairs {
            if live.contains(&pair) || user_changed.contains(&pair) {
                continue;
            }
            (Query::delete()
                .from_table(WorkloadEnvironments::Table)
                .and_where(
                    Expr::col(WorkloadEnvironments::Workload)
                        .eq(pair.workload.as_str()),
                )
                .and_where(
                    Expr::col(WorkloadEnvironments::Environment)
                        .eq(pair.environment.as_str()),
                ))
            .bound()
            .execute(&mut *tx)
            .await?;
            removed += 1;
        }
        // Workloads with no environments left.
        (Query::delete().from_table(Workloads::Table).and_where(
            Expr::col(Workloads::Name).not_in_subquery(
                Query::select()
                    .column(WorkloadEnvironments::Workload)
                    .from(WorkloadEnvironments::Table)
                    .take(),
            ),
        ))
        .bound()
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(removed)
    }

    /// Manual placements by ARN.
    pub(crate) async fn overrides(
        &self,
    ) -> Result<BTreeMap<Arn, PairKey>, DbError> {
        let rows = (Query::select()
            .columns([
                WorkloadOverrides::Arn,
                WorkloadOverrides::Workload,
                WorkloadOverrides::Environment,
            ])
            .from(WorkloadOverrides::Table))
        .bound()
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok((
                    row.parsed("arn")?,
                    PairKey::new(
                        row.try_get::<String, _>("workload")?,
                        row.try_get::<String, _>("environment")?,
                    ),
                ))
            })
            .collect()
    }
}
