//! Table and column identifiers for sea-query. These must match the SQL in
//! `migrations/`.

use sea_query::Iden;

#[derive(Iden)]
pub(crate) enum Scans {
    Table,
    Id,
    Seq,
    Started,
    Finished,
    Provider,
    Caller,
}

#[derive(Iden)]
pub(crate) enum ScanTargets {
    Table,
    ScanId,
    AccountId,
    Region,
    Status,
    Error,
}

#[derive(Iden)]
pub(crate) enum ResourceVersions {
    Table,
    Arn,
    Version,
    Provider,
    Kind,
    AccountId,
    Region,
    Tags,
    Body,
    ContentHash,
    FirstSeenSeq,
    LastSeenSeq,
}

#[derive(Iden)]
pub(crate) enum RelationshipVersions {
    Table,
    FromArn,
    Edge,
    ToArn,
    Provider,
    FirstSeenSeq,
    LastSeenSeq,
}

#[derive(Iden)]
pub(crate) enum Workloads {
    Table,
    Name,
}

#[derive(Iden)]
pub(crate) enum WorkloadEnvironments {
    Table,
    Workload,
    Environment,
    Disposition,
}

#[derive(Iden)]
pub(crate) enum WorkloadMembers {
    Table,
    ScanId,
    Arn,
    Workload,
    Environment,
    Signal,
}

#[derive(Iden)]
pub(crate) enum WorkloadOverrides {
    Table,
    Arn,
    Workload,
    Environment,
}

#[derive(Iden)]
pub(crate) enum Settings {
    Table,
    Key,
    Value,
}

#[derive(Iden)]
pub(crate) enum WorkloadAliases {
    Table,
    Alias,
    Workload,
}

#[derive(Iden)]
pub(crate) enum DispositionHistory {
    Table,
    Id,
    Workload,
    Environment,
    Disposition,
    Previous,
    ChangedAt,
    Source,
    Note,
}

#[derive(Iden)]
pub(crate) enum ScheduleBlackouts {
    Table,
    Name,
    StartDate,
    EndDate,
    Workload,
    Environment,
}

#[derive(Iden)]
pub(crate) enum ScheduleRules {
    Table,
    Workload,
    Environment,
    Weekdays,
    Earliest,
    Priority,
    Wave,
    DurationDays,
    Weight,
}

#[derive(Iden)]
pub(crate) enum ScheduleDependencies {
    Table,
    Workload,
    Environment,
    OnWorkload,
    OnEnvironment,
}

#[derive(Iden)]
pub(crate) enum ScheduleWaves {
    Table,
    Wave,
    Name,
    TargetEnd,
}

#[derive(Iden)]
pub(crate) enum MigrationStatus {
    Table,
    Workload,
    Environment,
    Status,
    ConfirmedDate,
    MigratedDate,
}

#[derive(Iden)]
pub(crate) enum MigrationEvents {
    Table,
    Id,
    Workload,
    Environment,
    Event,
    Date,
    At,
    Note,
}

#[derive(Iden)]
pub(crate) enum ScheduleVersions {
    Table,
    Id,
    ComputedAt,
    AsOf,
    TargetEnd,
    Ends,
    Late,
    Trigger,
    Schedule,
    EventId,
}

#[derive(Iden)]
pub(crate) enum ScheduleSlots {
    Table,
    VersionId,
    Workload,
    Environment,
    Date,
    EndDate,
    Wave,
    Locked,
    Reason,
    Alternatives,
}

#[derive(Iden)]
pub(crate) enum PlanEvents {
    Table,
    Id,
    At,
    Kind,
    Body,
    VersionId,
}
