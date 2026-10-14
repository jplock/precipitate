//! Table and column identifiers for sea-query. These must match the SQL in
//! `migrations/`.

use sea_query::Iden;

#[derive(Iden)]
pub enum Scans {
    Table,
    Id,
    Seq,
    Started,
    Finished,
    CallerArn,
}

#[derive(Iden)]
pub enum ScanTargets {
    Table,
    ScanId,
    AccountId,
    Region,
    Status,
    Error,
}

#[derive(Iden)]
pub enum ResourceVersions {
    Table,
    Arn,
    Version,
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
pub enum RelationshipVersions {
    Table,
    FromArn,
    Edge,
    ToArn,
    FirstSeenSeq,
    LastSeenSeq,
}

#[derive(Iden)]
pub enum Workloads {
    Table,
    Name,
}

#[derive(Iden)]
pub enum WorkloadEnvironments {
    Table,
    Workload,
    Environment,
    Disposition,
}

#[derive(Iden)]
pub enum WorkloadMembers {
    Table,
    ScanId,
    Arn,
    Workload,
    Environment,
    Signal,
}

#[derive(Iden)]
pub enum WorkloadOverrides {
    Table,
    Arn,
    Workload,
    Environment,
}

#[derive(Iden)]
pub enum Settings {
    Table,
    Key,
    Value,
}

#[derive(Iden)]
pub enum WorkloadAliases {
    Table,
    Alias,
    Workload,
}

#[derive(Iden)]
pub enum DispositionHistory {
    Table,
    Workload,
    Environment,
    Disposition,
    Previous,
    ChangedAt,
    Source,
    Note,
}

#[derive(Iden)]
pub enum ScheduleBlackouts {
    Table,
    Name,
    StartDate,
    EndDate,
    Workload,
    Environment,
}

#[derive(Iden)]
pub enum ScheduleRules {
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
pub enum ScheduleDependencies {
    Table,
    Workload,
    Environment,
    OnWorkload,
    OnEnvironment,
}

#[derive(Iden)]
pub enum ScheduleWaves {
    Table,
    Wave,
    Name,
    TargetEnd,
}

#[derive(Iden)]
pub enum MigrationStatus {
    Table,
    Workload,
    Environment,
    Status,
    ConfirmedDate,
    MigratedDate,
}

#[derive(Iden)]
pub enum MigrationEvents {
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
pub enum ScheduleVersions {
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
pub enum ScheduleSlots {
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
pub enum PlanEvents {
    Table,
    Id,
    At,
    Kind,
    Body,
    VersionId,
}
