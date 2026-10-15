use super::*;
use crate::kinds::Setting;
use crate::query::Bound;
use crate::schema::{Settings, WorkloadOverrides};
use precip_engine::schedule::{Blackout, Schedule, Slot, Weekdays};
use precip_types::body::Instance;
use precip_types::scan::{
    Edge, Relationship, Resource, ResourceKind, ScanTarget, TargetStatus,
};
use precip_types::{
    Arn, Disposition, GroupingConfig, PairKey, Provider, Scan, WorkloadMember,
};
use sea_query::{Expr, ExprTrait, Query};
use std::collections::BTreeMap;

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
        account_id: "123456789012".into(),
        region: "us-east-1".into(),
        tags: BTreeMap::from([("Name".into(), "web".into())]),
        body: precip_types::Body::Instance(Instance {
            instance_type: Some(instance_type.into()),
            ..Default::default()
        }),
    }
}

fn scan(id: &str, day: u8, resources: Vec<Resource>) -> Scan {
    Scan {
        id: id.into(),
        started: format!("2026-10-{day:02}T00:00:00Z").parse().unwrap(),
        finished: None,
        provider: Provider::Aws,
        caller: "arn:aws:iam::123456789012:role/precip".into(),
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
    assert_eq!(s2, ScanStats { seq: 2, unchanged: 1, ..Default::default() });

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
        account_id: ACCT.into(),
        region: "us-east-1".into(),
        tags: tags
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        body: precip_types::Body::from_value(kind, body)
            .expect("fixture body matches its kind"),
    }
}

fn inst(id: &str, tags: &[(&str, &str)]) -> Resource {
    res(
        &format!("arn:aws:ec2:us-east-1:{ACCT}:instance/{id}"),
        ResourceKind::Instance,
        tags,
        serde_json::json!({
            "instance_type": "t3.micro",
            "network_id": format!("arn:aws:ec2:us-east-1:{ACCT}:vpc/vpc-1"),
        }),
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
    store.group_scan(&s.id, &GroupingConfig::default()).await.expect("group");
    let members = store.members(&s.id).await.expect("members");
    (store, members)
}

fn find<'a>(members: &'a [WorkloadMember], arn: &str) -> &'a WorkloadMember {
    members
        .iter()
        .find(|m| m.arn == arn)
        .unwrap_or_else(|| panic!("{arn} not placed"))
}

#[tokio::test]
async fn summaries_count_each_account_and_region_once() {
    let mut west = inst("i-3", &[("app", "shop-prod")]);
    west.region = "us-west-2".into();
    west.arn = Arn::from(format!("arn:aws:ec2:us-west-2:{ACCT}:instance/i-3"));
    let (store, _) = grouped(fixture(
        "s",
        vec![
            inst("i-1", &[("app", "shop-prod")]),
            inst("i-2", &[("app", "shop-prod")]),
            west,
        ],
        vec![],
    ))
    .await;
    let summary = store.workload_summary("s").await.unwrap();
    let shop = summary
        .iter()
        .find(|w| w.workload == "shop" && w.environment == "prod")
        .unwrap();
    assert_eq!(shop.resources, 3);
    assert_eq!(shop.accounts, vec![ACCT.to_string()]);
    assert_eq!(shop.regions, vec!["us-east-1", "us-west-2"]);
    let overview = store.overview(1).await.unwrap();
    assert_eq!((overview.accounts, overview.regions), (1, 2));
    // Each new scan takes the next sequence number.
    let next = store.record_scan(&scan("t", 2, vec![])).await.unwrap();
    assert_eq!(next.seq, 2);
}

#[tokio::test]
async fn pair_members_lists_each_resource_with_kind_name_and_signal() {
    let (store, _) = grouped(fixture(
        "s",
        vec![
            inst("i-1", &[("app", "billing-prod"), ("Name", "web-1")]),
            res(
                V1,
                ResourceKind::Volume,
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
            ("instance".into(), "web-1".into(), "tag".into()),
            ("volume".into(), "vol-1".into(), "attached".into()),
        ]
    );
    assert!(
        store.pair_members("s", "billing", "dev").await.unwrap().is_empty()
    );
}

#[tokio::test]
async fn a_pairs_terraform_holds_its_instances_and_attached_disks() {
    let (store, _) = grouped(fixture(
        "s",
        vec![
            inst("i-1", &[("app", "billing-prod"), ("Name", "web-1")]),
            inst("i-2", &[("app", "shop-prod")]),
            res(
                V1,
                ResourceKind::Volume,
                &[],
                serde_json::json!({ "size_gib": 8, "attached_to": [I1] }),
            ),
            res(
                "arn:aws:ec2:us-east-1::instance-type/t3.micro",
                ResourceKind::InstanceType,
                &[],
                serde_json::json!({ "name": "t3.micro", "vcpus": 2, "memory_mib": 1024 }),
            ),
        ],
        vec![rel(V1, Edge::AttachedTo, I1)],
    ))
    .await;
    let pair = PairKey::new("billing", "prod");
    let tf = store.pair_terraform("s", &pair).await.unwrap();
    assert!(tf.contains("resource \"oxide_project\" \"billing-prod\""), "{tf}");
    assert!(tf.contains("resource \"oxide_instance\" \"web-1\""), "{tf}");
    assert!(tf.contains("oxide_disk.vol-1.id"), "{tf}");
    assert!(!tf.contains("i-2"), "the other pair's instance is left out");
}

#[tokio::test]
async fn folding_a_workload_into_itself_records_nothing() {
    let store = memory_store().await;
    let before = store.events().await.unwrap().len();
    store.merge_workload("shop", " shop ").await.unwrap();
    assert_eq!(store.events().await.unwrap().len(), before);
    assert!(store.aliases().await.unwrap().is_empty());
    store.merge_workload("shop-eu", "shop").await.unwrap();
    assert_eq!(store.events().await.unwrap().len(), before + 1);
    assert_eq!(store.aliases().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_fold_that_would_loop_is_refused() {
    let store = memory_store().await;
    store.merge_workload("a", "b").await.unwrap();
    store.merge_workload("b", "c").await.unwrap();
    let before = store.events().await.unwrap().len();
    let err = store.merge_workload("c", "a").await.unwrap_err();
    assert!(err.to_string().contains("a → b → c"), "{err}");
    assert_eq!(store.events().await.unwrap().len(), before, "nothing recorded");
    store.merge_workload("a", "c").await.unwrap();
    assert_eq!(
        store.aliases().await.unwrap().len(),
        2,
        "refolding a name replaces its fold"
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
            DispositionSource::User,
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
    store.set_disposition("kept", "prod", Disposition::Retain).await.unwrap();
    // Next scan: only i-1 remains, regrouped under a new name.
    let mut s2 =
        fixture("s2", vec![inst("i-1", &[("app", "new-prod")])], vec![]);
    s2.started = "2026-10-02T00:00:00Z".parse().unwrap();
    store.record_scan(&s2).await.unwrap();
    store.group_scan("s2", &GroupingConfig::default()).await.unwrap();

    let removed = store.prune_stale_pairs("s2", true).await.unwrap();
    assert_eq!(removed, 2, "old and gone go; kept stays because a user set it");
    let names: Vec<String> = store
        .pair_dispositions()
        .await
        .unwrap()
        .into_keys()
        .map(|p| p.workload.into_string())
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
    let (_, s) = store.compute_schedule(today, Trigger::Command).await.unwrap();
    let erp = s.slots.iter().find(|x| x.pair.workload == "erp").unwrap();
    assert_eq!(erp.wave, 2);
    assert_eq!((erp.date, erp.end), (date(2026, 10, 15), date(2026, 10, 17)));
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
    let (_, s) = store.compute_schedule(today, Trigger::Command).await.unwrap();
    let dev = s.slots.iter().find(|x| x.pair.environment == "dev").unwrap();
    assert!(dev.reason().starts_with("rolled back"));
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
    if let precip_types::Body::Instance(i) = &mut gone.body {
        i.state = Some("terminated".into());
    }
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
async fn two_pairs_cannot_be_agreed_on_one_day_at_one_per_day() {
    use jiff::civil::date;
    use precip_engine::schedule::Status;
    let (store, _) = grouped(fixture(
        "s",
        vec![
            inst("i-1", &[("app", "shop-dev")]),
            inst("i-2", &[("app", "blog-dev")]),
        ],
        vec![],
    ))
    .await;
    let today = date(2026, 10, 14);
    let day = Some(date(2026, 10, 20));
    store
        .set_pair_status("shop", "dev", Status::Confirmed, day, today, None)
        .await
        .unwrap();
    assert!(matches!(
        store.set_pair_status("blog", "dev", Status::Confirmed, day, today, None).await,
        Err(DbError::Invalid(msg)) if msg.contains("limit is 1 per day")
    ));
    store
        .set_pair_status(
            "blog",
            "dev",
            Status::Confirmed,
            Some(date(2026, 10, 21)),
            today,
            None,
        )
        .await
        .unwrap();
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
    assert!(matches!(
        status(Status::RolledBack, None).await,
        Err(DbError::Invalid(msg)) if msg == "only a migrated pair can be rolled back"
    ));
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
    assert!(refused(status(Status::Confirmed, Some(date(2026, 11, 1))).await));
    status(Status::RolledBack, None).await.unwrap();
    assert!(refused(status(Status::RolledBack, None).await));

    let b = Blackout {
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
    let (_, before) =
        store.compute_schedule(today, Trigger::Command).await.unwrap();
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
            DispositionSource::User,
            Some("unused"),
        )
        .await
        .unwrap();
    store
        .set_default_disposition(Disposition::Replatform, false)
        .await
        .unwrap();
    store.merge_workload("shop-eu", "shop").await.unwrap();
    let (_, before) =
        store.compute_schedule(today, Trigger::Command).await.unwrap();
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
async fn a_change_whose_schedule_fails_leaves_no_trace() {
    use precip_engine::schedule::Blackout;
    let store = memory_store().await;
    let events = store.events().await.unwrap().len();
    // A stored target the scheduler cannot read makes computing the
    // version fail after the event was projected and logged.
    (Query::update()
        .table(Settings::Table)
        .value(Settings::Value, "not a date")
        .and_where(Expr::col(Settings::Key).eq(Setting::TargetEnd.as_str())))
    .bound()
    .execute(&store.pool)
    .await
    .unwrap();
    let freeze = Blackout {
        name: "freeze".into(),
        start: jiff::civil::date(2026, 12, 24),
        end: jiff::civil::date(2026, 12, 26),
        pair: None,
    };
    assert!(store.add_blackout(&freeze).await.is_err());
    assert!(store.blackouts().await.unwrap().is_empty());
    assert_eq!(store.events().await.unwrap().len(), events);
}

#[tokio::test]
async fn a_replay_that_fails_keeps_the_plan_it_had() {
    use precip_engine::schedule::Blackout;
    let store = memory_store().await;
    let freeze = Blackout {
        name: "freeze".into(),
        start: jiff::civil::date(2026, 12, 24),
        end: jiff::civil::date(2026, 12, 26),
        pair: None,
    };
    // An event no projection accepts, written past the checks, ahead
    // of a change the replay would otherwise wipe.
    let bad = PlanEvent::PairPlanSet {
        workload: "shop".into(),
        environment: "prod".into(),
        wave: 0,
        duration_days: 1,
        weight: 1,
    };
    (Query::insert()
        .into_table(schema::PlanEvents::Table)
        .columns([
            schema::PlanEvents::At,
            schema::PlanEvents::Kind,
            schema::PlanEvents::Body,
        ])
        .values_panic([
            jiff::Timestamp::now().to_string().into(),
            bad.kind().as_str().into(),
            serde_json::to_string(&bad).unwrap().into(),
        ]))
    .bound()
    .execute(&store.pool)
    .await
    .unwrap();
    store.add_blackout(&freeze).await.unwrap();
    assert!(store.replay().await.is_err());
    assert_eq!(store.blackouts().await.unwrap(), vec![freeze]);
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
    let dev_date = |s: &Schedule| {
        s.slots.iter().find(|x| x.pair.environment == "dev").unwrap().date
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
    let (_, s1) =
        store.compute_schedule(today, Trigger::Command).await.unwrap();
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
    let (_, s2) =
        store.compute_schedule(today, Trigger::Command).await.unwrap();
    assert_eq!(dev_date(&s2), today, "prod moved on, so today is open again");
    assert_eq!(
        s2.slots.iter().filter(|x| x.agreed).count(),
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
            Some(&Weekdays::only([jiff::civil::Weekday::Saturday])),
            None,
            0,
        )
        .await
        .unwrap();

    let today = date(2026, 10, 14);
    let (v1, s1) =
        store.compute_schedule(today, Trigger::Command).await.unwrap();
    let by: BTreeMap<&str, &Slot> =
        s1.slots.iter().map(|x| (x.pair.environment.as_str(), x)).collect();
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
        store.compute_schedule(today, Trigger::StatusChange).await.unwrap();
    let prod = s2.slots.iter().find(|x| x.pair.environment == "prod").unwrap();
    assert!(prod.agreed);
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
    let (_, s3) = store
        .compute_schedule(date(2026, 10, 26), Trigger::Open)
        .await
        .unwrap();
    assert_eq!(s3.expired, vec![precip_types::PairKey::new("shop", "prod")]);
    assert_eq!(
        store.pair_status("shop", "prod").await.unwrap().status,
        Status::Missed
    );
    let events: Vec<&str> = store
        .migration_events("shop", "prod")
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.event.as_str())
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
    let events: Vec<&str> = store
        .migration_events("shop", "prod")
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.event.as_str())
        .collect();
    assert_eq!(events, vec!["confirmed", "missed", "confirmed", "reconfirmed"]);
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
                ResourceKind::ScaleGroup,
                &[],
                serde_json::json!({ "name": "vouch-us-prod" }),
            ),
            inst("i-1", &[]),
            inst("i-2", &[("Service", "vouch"), ("env", "prod")]),
        ],
        vec![rel(I1, Edge::MemberOfScaleGroup, asg)],
    ))
    .await;
    assert_eq!(find(&m, I1).pair.workload, "vouch-us");
    store.merge_workload("vouch-us", "vouch").await.unwrap();
    store.group_scan("s", &GroupingConfig::default()).await.unwrap();
    let m = store.members("s").await.unwrap();
    assert!(m.iter().all(|x| x.pair.workload == "vouch"), "{m:?}");
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
    assert_eq!(find(&m, I1).pair.workload, "x");
    (Query::insert()
        .into_table(WorkloadOverrides::Table)
        .columns([
            WorkloadOverrides::Arn,
            WorkloadOverrides::Workload,
            WorkloadOverrides::Environment,
        ])
        .values_panic([I1.into(), "legacy".into(), "prod".into()]))
    .bound()
    .execute(&store.pool)
    .await
    .unwrap();
    store.group_scan("s", &GroupingConfig::default()).await.unwrap();
    let m = store.members("s").await.unwrap();
    assert_eq!(m.len(), 1, "regrouping replaces, never duplicates");
    assert_eq!(
        (find(&m, I1).pair.workload.as_str(), find(&m, I1).signal),
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

const AZ_VM: &str = "/subscriptions/00000000-0000-0000-0000-000000000001/resourcegroups/shop/providers/microsoft.compute/virtualmachines/web-1";
const AZ_DISK: &str = "/subscriptions/00000000-0000-0000-0000-000000000001/resourcegroups/shop/providers/microsoft.compute/disks/web-1-os";

fn azure_scan(id: &str, day: u8, resources: Vec<Resource>) -> Scan {
    Scan {
        provider: Provider::Azure,
        caller: "11111111-1111-1111-1111-111111111111".into(),
        relationships: vec![Relationship {
            from_arn: AZ_DISK.into(),
            edge: Edge::AttachedTo,
            to_arn: AZ_VM.into(),
        }],
        ..scan(id, day, resources)
    }
}

#[tokio::test]
async fn a_scan_of_one_provider_keeps_the_other_providers_estate() {
    let store = memory_store().await;
    store
        .record_scan(&scan("s1", 1, vec![instance(I1, "m6i.large")]))
        .await
        .unwrap();
    let s2 = store
        .record_scan(&azure_scan(
            "s2",
            2,
            vec![instance(AZ_VM, "Standard_D2s_v5")],
        ))
        .await
        .unwrap();
    assert_eq!(s2, ScanStats { seq: 2, added: 1, ..Default::default() });
    assert_eq!(store.resource_count(2).await.unwrap(), 2, "both estates");
    let history = store.history(I1).await.unwrap();
    assert_eq!(history.len(), 1, "the AWS row is extended, not copied");
    assert_eq!((history[0].1, history[0].2), (1, 2));
    let d = store.diff_scans(1, 2).await.unwrap();
    assert_eq!((d.added, d.removed), (vec![AZ_VM.to_string()], vec![]));
    let rels = store.live_relationships(2).await.unwrap();
    assert_eq!(rels.len(), 2, "relationships of both providers are live");
}

#[tokio::test]
async fn a_resource_leaves_only_when_its_own_provider_no_longer_sees_it() {
    let store = memory_store().await;
    store
        .record_scan(&scan(
            "s1",
            1,
            vec![instance(I1, "m6i.large"), instance(I2, "t3.micro")],
        ))
        .await
        .unwrap();
    store
        .record_scan(&azure_scan(
            "s2",
            2,
            vec![instance(AZ_VM, "Standard_D2s_v5")],
        ))
        .await
        .unwrap();
    let s3 = store
        .record_scan(&azure_scan(
            "s3",
            3,
            vec![instance(AZ_VM, "Standard_D2s_v5")],
        ))
        .await
        .unwrap();
    assert_eq!(s3, ScanStats { seq: 3, unchanged: 1, ..Default::default() });
    assert_eq!(store.resource_count(3).await.unwrap(), 3);

    let s4 = store
        .record_scan(&scan("s4", 4, vec![instance(I1, "m6i.large")]))
        .await
        .unwrap();
    assert_eq!(s4, ScanStats { seq: 4, unchanged: 1, ..Default::default() });
    let d = store.diff_scans(3, 4).await.unwrap();
    assert_eq!(d.removed, vec![I2.to_string()], "only the AWS host left");
    assert_eq!(store.resource_count(4).await.unwrap(), 2);
    let scans = store.list_scans().await.unwrap();
    assert_eq!(
        scans.iter().map(|s| s.provider).collect::<Vec<_>>(),
        [Provider::Aws, Provider::Azure, Provider::Azure, Provider::Aws]
    );
}

#[tokio::test]
async fn scan_selections_are_saved_per_provider() {
    let store = memory_store().await;
    store.save_scan_options(Provider::Aws, r#"{"a":1}"#).await.unwrap();
    store.save_scan_options(Provider::Azure, r#"{"z":1}"#).await.unwrap();
    assert_eq!(
        store.saved_scan_options(Provider::Aws).await.unwrap().as_deref(),
        Some(r#"{"a":1}"#)
    );
    assert_eq!(
        store.saved_scan_options(Provider::Azure).await.unwrap().as_deref(),
        Some(r#"{"z":1}"#)
    );
}
