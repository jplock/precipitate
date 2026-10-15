//! The shape every scanner's collectors share. A collector is one
//! service's list calls for one scope: an AWS account and region, a
//! batch of Azure subscriptions, or a Google Cloud project, folder or
//! organization. The runner runs each collector, records
//! failures by API name, logs what came back, and merges it.

pub mod http;

use precip_types::scan::{
    Edge, Relationship, Resource, ResourceKind, ScanTarget, TargetStatus,
};
use precip_types::{AccountId, Arn, Body, Region, ScanEvent};
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tracing::Instrument;

/// Receives progress events. Dropped receivers are ignored, so a headless
/// scan can pass `None`.
pub type ProgressSender = tokio::sync::mpsc::UnboundedSender<ScanEvent>;

/// Send a progress event if anyone is listening.
pub fn emit(progress: &Option<ProgressSender>, event: ScanEvent) {
    if let Some(tx) = progress {
        // A closed receiver means nobody is watching; the scan goes on.
        let _ = tx.send(event);
    }
}

/// Whether an account passes an include list (empty means every account)
/// and an exclude list, matched on any of its keys (an id, a number, a
/// name) without regard to case.
pub fn selected<S: AsRef<str>>(
    keys: &[&str],
    only: &[S],
    except: &[S],
) -> bool {
    let named = |list: &[S]| {
        list.iter()
            .any(|x| keys.iter().any(|k| k.eq_ignore_ascii_case(x.as_ref())))
    };
    (only.is_empty() || named(only)) && !named(except)
}

/// Report accounts scanned together in one pass over every region, as
/// Azure and Google Cloud scan them: each gets one target with the
/// batch's status and the progress events an account and region scan
/// would send.
pub fn finish_batch(
    progress: &Option<ProgressSender>,
    accounts: &[AccountId],
    got: &Collected,
    status: &TargetStatus,
) -> Vec<ScanTarget> {
    let mut per_account: HashMap<&str, usize> = HashMap::new();
    for r in &got.resources {
        *per_account.entry(r.account_id.as_str()).or_default() += 1;
    }
    let mut targets = Vec::new();
    for id in accounts {
        let resources = per_account.get(id.as_str()).copied().unwrap_or(0);
        emit(
            progress,
            ScanEvent::RegionsPlanned {
                account_id: id.clone(),
                regions: 1,
                skipped: 0,
            },
        );
        emit(
            progress,
            ScanEvent::RegionFinished {
                account_id: id.clone(),
                region: Region::every(),
                status: status.clone(),
                resources,
            },
        );
        emit(
            progress,
            ScanEvent::AccountFinished { account_id: id.clone(), resources },
        );
        targets.push(ScanTarget {
            account_id: id.clone(),
            region: Region::every(),
            status: status.clone(),
        });
    }
    targets
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Default)]
pub struct Collected {
    pub resources: Vec<Resource>,
    pub relationships: Vec<Relationship>,
    /// Tags to merge into resources collected earlier, by ARN. Existing
    /// keys are kept; missing ones are added.
    pub tag_updates: Vec<(Arn, BTreeMap<String, String>)>,
}

impl Collected {
    /// Fold another collector's output into this one.
    pub fn absorb(&mut self, other: Collected) {
        if !other.tag_updates.is_empty() {
            let by_arn: HashMap<Arn, usize> = self
                .resources
                .iter()
                .enumerate()
                .map(|(i, r)| (r.arn.clone(), i))
                .collect();
            for (arn, tags) in other.tag_updates {
                if let Some(&i) = by_arn.get(&arn) {
                    for (k, v) in tags {
                        self.resources[i].tags.entry(k).or_insert(v);
                    }
                }
            }
        }
        self.resources.extend(other.resources);
        self.relationships.extend(other.relationships);
    }

    /// Resources of one kind collected so far.
    pub fn of_kind(
        &self,
        kind: ResourceKind,
    ) -> impl Iterator<Item = &Resource> {
        self.resources.iter().filter(move |r| r.kind() == kind)
    }

    /// The distinct values `field` picks out of the bodies collected so
    /// far, sorted.
    pub fn distinct<'a, I>(
        &'a self,
        field: impl Fn(&'a Body) -> I,
    ) -> Vec<String>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut v: Vec<String> = self
            .resources
            .iter()
            .flat_map(|r| field(&r.body))
            .map(str::to_string)
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// Add an edge between two resources.
    pub fn relate(&mut self, from: Arn, edge: Edge, to: Arn) {
        self.relationships.push(Relationship {
            from_arn: from,
            edge,
            to_arn: to,
        });
    }
}

/// An API string as an owned body field.
pub fn own(s: Option<&str>) -> Option<String> {
    s.map(str::to_string)
}

/// API strings as an owned body list.
pub fn owned<'a>(items: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    items.into_iter().map(str::to_string).collect()
}

/// One failed API call, named so the report can say which permission or
/// service was the problem.
#[derive(Debug, thiserror::Error)]
#[error("{api}")]
pub struct CollectError {
    pub api: &'static str,
    #[source]
    pub source: Box<dyn std::error::Error + Send + Sync>,
}

impl CollectError {
    pub fn new<E>(api: &'static str) -> impl FnOnce(E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        move |e| Self { api, source: Box::new(e) }
    }
}

/// The error and every source below it, joined, so a stored failure says
/// why (throttled, access denied, timeout) and not only which API.
pub fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![e.to_string()];
    let mut cur = e.source();
    while let Some(s) = cur {
        parts.push(s.to_string());
        cur = s.source();
    }
    parts.dedup();
    parts.join(": ")
}

/// When a collector runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Concurrently with the other first-stage collectors.
    First,
    /// After the first stage, one at a time in registry order, reading
    /// everything collected before it.
    Second,
}

/// A collector as it is written: one service's calls for one scope.
pub trait Collect: Send + Sync + 'static {
    /// What the collector reads from: credentials and scope.
    type Context: Send + Sync + 'static;
    /// The API the collector calls, used in logs and partial-scan reports.
    const API: &'static str;
    const STAGE: Stage = Stage::First;

    /// `prior` holds everything collectors earlier in the registry
    /// produced for this scope, so a second-stage collector can describe
    /// only what is referenced (images in use, machine sizes in use).
    fn collect(
        &self,
        ctx: &Self::Context,
        prior: &Collected,
    ) -> impl Future<Output = Result<Collected, CollectError>> + Send;
}

/// A collector as the runner holds it.
pub trait Collector<C>: Send + Sync {
    fn name(&self) -> &'static str;
    fn stage(&self) -> Stage;
    fn collect<'a>(
        &'a self,
        ctx: &'a C,
        prior: &'a Collected,
    ) -> BoxFuture<'a, Result<Collected, CollectError>>;
}

impl<T: Collect> Collector<T::Context> for T {
    fn name(&self) -> &'static str {
        T::API
    }

    fn stage(&self) -> Stage {
        T::STAGE
    }

    fn collect<'a>(
        &'a self,
        ctx: &'a T::Context,
        prior: &'a Collected,
    ) -> BoxFuture<'a, Result<Collected, CollectError>> {
        Box::pin(Collect::collect(self, ctx, prior))
    }
}

/// Run every collector for one scope. Collectors that do not depend on
/// earlier results run concurrently; the second stage runs once those
/// finish, one at a time in registry order. A failed collector marks the
/// scope partial; all failing marks it failed. Nothing is dropped silently.
pub async fn run<C: Send + Sync + 'static>(
    collectors: Vec<Arc<dyn Collector<C>>>,
    ctx: Arc<C>,
) -> (Collected, TargetStatus) {
    let total = collectors.len();
    let (second, first): (Vec<_>, Vec<_>) =
        collectors.into_iter().partition(|c| c.stage() == Stage::Second);

    let mut failures: Vec<String> = Vec::new();
    let mut so_far = run_stage(first, &ctx, &mut failures).await;

    // Second-stage collectors may depend on each other (instance types
    // come from launch templates), so they run one at a time in order,
    // each reading everything collected before it.
    for c in second {
        match c.collect(&ctx, &so_far).await {
            Ok(got) => {
                log_collected(c.name(), &got);
                so_far.absorb(got);
            }
            Err(e) => record_failure(&mut failures, c.name(), &e),
        }
    }

    let status = if failures.is_empty() {
        TargetStatus::Scanned
    } else if failures.len() == total {
        TargetStatus::Failed(failures.join(", "))
    } else {
        TargetStatus::Partial(failures.join(", "))
    };
    (so_far, status)
}

/// Run a set of collectors concurrently and merge their output in a fixed
/// order so results are deterministic.
async fn run_stage<C: Send + Sync + 'static>(
    collectors: Vec<Arc<dyn Collector<C>>>,
    ctx: &Arc<C>,
    failures: &mut Vec<String>,
) -> Collected {
    let prior = Arc::new(Collected::default());
    let mut tasks = tokio::task::JoinSet::new();
    let mut names = HashMap::new();
    for (i, c) in collectors.into_iter().enumerate() {
        let ctx = ctx.clone();
        let prior = prior.clone();
        let span = tracing::Span::current();
        let name = c.name();
        let handle = tasks.spawn(
            async move {
                let result = c.collect(&ctx, &prior).await;
                (i, c.name(), result)
            }
            .instrument(span),
        );
        names.insert(handle.id(), name);
    }
    let mut results = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(r) => results.push(r),
            Err(e) => {
                let name = names.get(&e.id()).copied().unwrap_or("collector");
                tracing::error!(api = name, error = %e, "collector task panicked");
                failures.push(format!("{name}: collector panicked: {e}"));
            }
        }
    }
    results.sort_by_key(|(i, _, _)| *i);

    let mut out = Collected::default();
    for (_, name, result) in results {
        match result {
            Ok(got) => {
                log_collected(name, &got);
                out.absorb(got);
            }
            Err(e) => record_failure(failures, name, &e),
        }
    }
    out
}

/// Log what one collector brought back.
fn log_collected(api: &'static str, got: &Collected) {
    tracing::info!(
        api,
        resources = got.resources.len(),
        relationships = got.relationships.len(),
        tag_updates = got.tag_updates.len(),
        "collected"
    );
}

/// Note a failed collector for the scope's report.
fn record_failure(
    failures: &mut Vec<String>,
    name: &'static str,
    e: &CollectError,
) {
    tracing::warn!(api = name, error = %e.source, "collector failed");
    failures.push(format!("{}: {}", e.api, error_chain(&*e.source)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use precip_types::Region;
    use precip_types::body::Instance;

    struct Ctx;

    struct Panics;

    impl Collect for Panics {
        type Context = Ctx;
        const API: &'static str = "test:Panics";

        async fn collect(
            &self,
            _ctx: &Ctx,
            _prior: &Collected,
        ) -> Result<Collected, CollectError> {
            panic!("collector bug")
        }
    }

    fn instance(arn: &str) -> Resource {
        Resource {
            arn: Arn::from(arn),
            account_id: "1".into(),
            region: Region::from("r"),
            tags: BTreeMap::new(),
            body: Body::Instance(Instance::default()),
        }
    }

    struct Works;

    impl Collect for Works {
        type Context = Ctx;
        const API: &'static str = "test:Works";

        async fn collect(
            &self,
            _ctx: &Ctx,
            _prior: &Collected,
        ) -> Result<Collected, CollectError> {
            let mut out = Collected::default();
            out.resources.push(instance("i-1"));
            Ok(out)
        }
    }

    #[derive(Debug, thiserror::Error)]
    #[error("denied")]
    struct Denied;

    struct Fails;

    impl Collect for Fails {
        type Context = Ctx;
        const API: &'static str = "test:Fails";

        async fn collect(
            &self,
            _ctx: &Ctx,
            _prior: &Collected,
        ) -> Result<Collected, CollectError> {
            Err(CollectError::new(Self::API)(Denied))
        }
    }

    /// Second stage: counts what the first stage collected and tags it.
    struct Counts;

    impl Collect for Counts {
        type Context = Ctx;
        const API: &'static str = "test:Counts";
        const STAGE: Stage = Stage::Second;

        async fn collect(
            &self,
            _ctx: &Ctx,
            prior: &Collected,
        ) -> Result<Collected, CollectError> {
            let mut out = Collected::default();
            out.tag_updates.push((
                Arn::from("i-1"),
                BTreeMap::from([(
                    "seen".to_string(),
                    prior.resources.len().to_string(),
                )]),
            ));
            Ok(out)
        }
    }

    fn block_on<F: Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(f)
    }

    #[test]
    fn a_panicking_collector_is_recorded_as_a_failure() {
        let (got, status) = block_on(run::<Ctx>(
            vec![Arc::new(Panics), Arc::new(Works)],
            Arc::new(Ctx),
        ));
        assert_eq!(got.resources.len(), 1);
        let TargetStatus::Partial(e) = status else { panic!("{status:?}") };
        assert!(e.starts_with("test:Panics: collector panicked"));
    }

    #[test]
    fn second_stage_reads_the_first_and_failures_name_their_api() {
        let (got, status) = block_on(run::<Ctx>(
            vec![Arc::new(Counts), Arc::new(Works), Arc::new(Fails)],
            Arc::new(Ctx),
        ));
        assert_eq!(
            got.resources[0].tags.get("seen").map(String::as_str),
            Some("1")
        );
        assert_eq!(status, TargetStatus::Partial("test:Fails: denied".into()));
    }

    #[test]
    fn every_collector_failing_fails_the_scope() {
        let (_, status) =
            block_on(run::<Ctx>(vec![Arc::new(Fails)], Arc::new(Ctx)));
        assert_eq!(status, TargetStatus::Failed("test:Fails: denied".into()));
        let (_, status) = block_on(run::<Ctx>(vec![], Arc::new(Ctx)));
        assert_eq!(
            status,
            TargetStatus::Scanned,
            "nothing to run is not a failure"
        );
    }

    #[test]
    fn tag_updates_keep_existing_keys() {
        let mut a = Collected::default();
        let mut r = instance("i-1");
        r.tags.insert("Name".into(), "web".into());
        a.resources.push(r);
        let mut b = Collected::default();
        b.tag_updates.push((
            Arn::from("i-1"),
            BTreeMap::from([
                ("Name".to_string(), "other".to_string()),
                ("App".to_string(), "shop".to_string()),
            ]),
        ));
        b.tag_updates.push((Arn::from("missing"), BTreeMap::new()));
        a.absorb(b);
        assert_eq!(a.resources[0].tags["Name"], "web");
        assert_eq!(a.resources[0].tags["App"], "shop");
    }

    #[test]
    fn selection_matches_any_key_ignoring_case() {
        let none: [&str; 0] = [];
        assert!(selected(&["a", "1"], &none, &none));
        assert!(selected(&["shop", "123"], &["123"], &none));
        assert!(selected(&["Shop"], &["SHOP"], &none));
        assert!(!selected(&["shop"], &["other"], &none));
        assert!(!selected(&["shop", "123"], &none, &["123"]));
        assert!(!selected(&["shop"], &["shop"], &["shop"]), "exclusion wins");
    }

    #[test]
    fn a_batch_reports_each_account_with_its_own_count() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut got = Collected::default();
        got.resources.push(instance("i-1"));
        let accounts = [AccountId::from("1"), AccountId::from("2")];
        let targets =
            finish_batch(&Some(tx), &accounts, &got, &TargetStatus::Scanned);
        assert_eq!(targets.len(), 2);
        assert!(targets.iter().all(|t| t.region == Region::every()));
        let mut finished = Vec::new();
        while let Ok(e) = rx.try_recv() {
            if let ScanEvent::AccountFinished { account_id, resources } = e {
                finished.push((account_id.to_string(), resources));
            }
        }
        assert_eq!(finished, [("1".to_string(), 1), ("2".to_string(), 0)]);
    }
}
