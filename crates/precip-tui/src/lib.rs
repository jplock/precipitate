//! ratatui application: header with context, tabs for overview, scans and
//! accounts, an activity feed, and a progress gauge while a scan runs. The
//! scan itself runs on the tokio runtime; the interface polls its progress
//! channel between frames.

mod logo;
mod ui;
pub mod workspace;

use camino::Utf8PathBuf;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode,
    KeyEventKind, MouseButton, MouseEventKind,
};
use precip_db::{
    DispositionChange, MemberDetail, Overview, ScanStats, ScanSummary, Store,
    WorkloadSummary,
};
use precip_types::scan::TargetStatus;
use precip_types::{Disposition, ScanEvent};
use ratatui::DefaultTerminal;
use ratatui::widgets::ListState;
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};
use workspace::{WorkspaceDir, WorkspaceEntry};

#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    #[error("terminal I/O")]
    Io(#[from] std::io::Error),
    #[error("database")]
    Db(#[from] precip_db::DbError),
}

/// A running scan: its progress channel and the task that completes it.
pub struct ScanHandle {
    pub events: tokio::sync::mpsc::UnboundedReceiver<ScanEvent>,
    pub done: tokio::task::JoinHandle<Result<ScanStats, String>>,
}

/// Starts a scan into the given store. Built by the binary so this crate
/// does not depend on the AWS crates.
pub type Launcher = Box<dyn FnMut(Store) -> ScanHandle + Send>;

/// What the header shows about where we are.
#[derive(Debug, Clone, Default)]
pub struct Context {
    pub profile: Option<String>,
    pub workspace: String,
}

/// The first screen: pick a workspace or create one.
pub struct Picker {
    pub dir: WorkspaceDir,
    pub entries: Vec<WorkspaceEntry>,
    pub state: ListState,
    /// Name being typed for a new workspace, when creating.
    pub new_name: Option<String>,
    pub error: Option<String>,
}

impl Picker {
    fn new(dir: WorkspaceDir) -> Result<Self, TuiError> {
        let entries = dir.list()?;
        let mut state = ListState::default();
        state.select(Some(0));
        Ok(Self { dir, entries, state, new_name: None, error: None })
    }

    /// Rows are the existing workspaces followed by "create new".
    pub fn row_count(&self) -> usize {
        self.entries.len() + 1
    }
}

pub enum Screen {
    Picker(Picker),
    Main,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Workloads,
    Sizing,
    Schedule,
    Scans,
    Accounts,
}

impl Tab {
    pub const ALL: [Tab; 6] = [
        Tab::Overview,
        Tab::Workloads,
        Tab::Sizing,
        Tab::Schedule,
        Tab::Scans,
        Tab::Accounts,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Workloads => "Workloads",
            Tab::Sizing => "Sizing",
            Tab::Schedule => "Schedule",
            Tab::Scans => "Scans",
            Tab::Accounts => "Accounts",
        }
    }

    fn index(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }

    pub fn next(self) -> Tab {
        Tab::ALL[(self.index() + 1) % Tab::ALL.len()]
    }

    pub fn previous(self) -> Tab {
        Tab::ALL[(self.index() + Tab::ALL.len() - 1) % Tab::ALL.len()]
    }
}

#[derive(Debug, Default, Clone)]
pub struct AccountProgress {
    pub name: Option<String>,
    pub planned: usize,
    pub skipped: usize,
    pub done: usize,
    pub resources: usize,
    pub failed: usize,
    pub note: Option<String>,
}

impl AccountProgress {
    /// Whether this account has nothing left to do.
    pub fn finished(&self) -> bool {
        self.note.is_some()
            || (self.planned + self.skipped > 0 && self.done >= self.planned)
    }

    pub fn state(&self) -> String {
        match &self.note {
            Some(n) => n.clone(),
            None if self.planned == 0 && self.skipped > 0 => {
                "no workload resources".to_string()
            }
            None if self.planned == 0 => "listing regions".to_string(),
            None if self.done < self.planned => "scanning".to_string(),
            None => "done".to_string(),
        }
    }
}

pub struct Progress {
    handle: ScanHandle,
    pub started: Instant,
    pub accounts_total: usize,
    pub accounts: BTreeMap<String, AccountProgress>,
    pub outcome: Option<Result<ScanStats, String>>,
}

impl Progress {
    pub fn regions_planned(&self) -> usize {
        self.accounts.values().map(|a| a.planned).sum()
    }

    pub fn regions_done(&self) -> usize {
        self.accounts.values().map(|a| a.done).sum()
    }

    pub fn accounts_done(&self) -> usize {
        self.accounts.values().filter(|a| a.finished()).count()
    }

    pub fn running(&self) -> bool {
        self.outcome.is_none()
    }
}

/// Sizing state: the profile, the assumptions the user can turn, the
/// assessed instances of the latest scan, and the result.
pub struct SizingView {
    pub profile: precip_types::Profile,
    pub assumptions: precip_engine::sizing::Assumptions,
    pub instances: Vec<precip_engine::sizing::inputs::SizedInstance>,
    pub result: precip_engine::sizing::Sizing,
    pub demand: Vec<precip_engine::sizing::WorkloadDemand>,
}

impl SizingView {
    fn compute(&mut self) {
        let inputs: Vec<precip_engine::sizing::Instance> =
            self.instances.iter().map(|i| i.to_instance()).collect();
        self.result = precip_engine::sizing::size(
            &self.profile,
            &inputs,
            &self.assumptions,
        );
        self.demand =
            precip_engine::sizing::demand_by_workload(&self.instances);
    }
}

/// The computed migration calendar and the controls over it.
pub struct ScheduleView {
    pub rules: precip_engine::schedule::GlobalRules,
    pub schedule: precip_engine::schedule::Schedule,
    pub metrics: precip_db::Metrics,
    /// Migrated pairs whose compute the latest scan still sees.
    pub findings: Vec<precip_db::Verification>,
    pub version: i64,
    pub selected: ratatui::widgets::TableState,
    pub today: jiff::civil::Date,
}

/// Picker for the global weekday rule: three presets, then each day of
/// the week as a toggle.
pub struct WeekdayPicker {
    pub days: Vec<jiff::civil::Weekday>,
    pub state: ListState,
}

/// Rows of the weekday picker, presets first.
pub const WEEKDAY_ROWS: [&str; 10] = [
    "any day", "weekdays", "weekends", "mon", "tue", "wed", "thu", "fri",
    "sat", "sun",
];

pub const ALL_WEEKDAYS: [jiff::civil::Weekday; 7] = [
    jiff::civil::Weekday::Monday,
    jiff::civil::Weekday::Tuesday,
    jiff::civil::Weekday::Wednesday,
    jiff::civil::Weekday::Thursday,
    jiff::civil::Weekday::Friday,
    jiff::civil::Weekday::Saturday,
    jiff::civil::Weekday::Sunday,
];

/// One thing the user can do from a row menu. Only the actions that make
/// sense for the row are offered, and the ones that change more than a
/// single pair, or record an outcome, ask once more before they act.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    AgreeDate,
    ChangeDate,
    Done,
    DidNotHappen,
    RolledBack,
    ShowResources,
    ChangeDisposition,
    PairSettings,
    MergeInto,
    DefaultDisposition,
    DefaultDispositionApply,
    Regroup,
    Prune,
    PruneAll,
}

impl Action {
    pub fn label(self) -> &'static str {
        match self {
            Action::AgreeDate => "Agree a date with the owners",
            Action::ChangeDate => "Agree a different date",
            Action::Done => "It's done",
            Action::DidNotHappen => "It didn't happen",
            Action::RolledBack => "It was rolled back to AWS",
            Action::ShowResources => "Show its resources",
            Action::ChangeDisposition => "Change its disposition",
            Action::PairSettings => "Its migration settings",
            Action::MergeInto => "Fold it into another workload",
            Action::DefaultDisposition => "Change the default disposition",
            Action::DefaultDispositionApply => {
                "Change the default and apply it to untouched pairs"
            }
            Action::Regroup => "Regroup the latest scan",
            Action::Prune => "Remove pairs the latest scan no longer has",
            Action::PruneAll => "Remove stale pairs, even ones set by hand",
        }
    }

    pub fn help(self) -> &'static str {
        match self {
            Action::AgreeDate => {
                "Pick one of the proposed dates. Once agreed, the tool never moves it."
            }
            Action::ChangeDate => {
                "Replace the agreed date with another. The old day is freed for someone else."
            }
            Action::Done => {
                "The migration finished today. The pair leaves the calendar."
            }
            Action::DidNotHappen => {
                "The agreed day passed without the move. A new date is proposed."
            }
            Action::RolledBack => {
                "The migration was undone. The pair goes back on the calendar."
            }
            Action::ShowResources => {
                "The instances, volumes, groups and services placed in this pair."
            }
            Action::ChangeDisposition => {
                "Rehost, replatform, retire and so on. Decides whether it is sized and scheduled."
            }
            Action::PairSettings => {
                "Wave, duration, weight, allowed days, earliest date, dependencies and freezes."
            }
            Action::MergeInto => {
                "This name is really another workload. Resources move there on regroup."
            }
            Action::DefaultDisposition => {
                "What newly discovered pairs start as. Existing pairs keep theirs."
            }
            Action::DefaultDispositionApply => {
                "Also changes every pair whose disposition nobody set by hand."
            }
            Action::Regroup => {
                "Run the grouping again on the latest scan, for example after a merge."
            }
            Action::Prune => {
                "Drops pairs with no resources any more, except ones someone set a disposition on."
            }
            Action::PruneAll => {
                "Drops every pair with no resources any more, including ones set by hand."
            }
        }
    }

    /// Actions that record an outcome or touch many pairs ask once more.
    pub fn needs_confirmation(self) -> bool {
        match self {
            Action::AgreeDate
            | Action::ChangeDate
            | Action::ShowResources
            | Action::ChangeDisposition
            | Action::PairSettings
            | Action::MergeInto
            | Action::DefaultDisposition => false,
            Action::Done
            | Action::DidNotHappen
            | Action::RolledBack
            | Action::DefaultDispositionApply
            | Action::Regroup
            | Action::Prune
            | Action::PruneAll => true,
        }
    }
}

pub struct ActionMenu {
    pub title: String,
    pub workload: String,
    pub environment: String,
    pub actions: Vec<Action>,
    pub state: ListState,
    /// The action waiting for a second Enter.
    pub confirming: Option<Action>,
}

/// A small form inside an editor row: one box per field, Tab between
/// them, so the shape of what to type is visible before typing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fields {
    pub labels: Vec<&'static str>,
    pub values: Vec<String>,
    pub active: usize,
}

impl Fields {
    pub fn new(labels: &[&'static str], values: &[&str]) -> Self {
        let mut values: Vec<String> =
            values.iter().map(|v| (*v).to_string()).collect();
        values.resize(labels.len(), String::new());
        Self { labels: labels.to_vec(), values, active: 0 }
    }

    pub fn get(&self, i: usize) -> &str {
        self.values.get(i).map(String::as_str).unwrap_or_default().trim()
    }

    pub fn next(&mut self) {
        self.active = (self.active + 1) % self.labels.len();
    }

    pub fn prev(&mut self) {
        self.active = (self.active + self.labels.len() - 1) % self.labels.len();
    }

    pub fn push(&mut self, c: char) {
        if let Some(v) = self.values.get_mut(self.active) {
            v.push(c);
        }
    }

    pub fn pop(&mut self) {
        if let Some(v) = self.values.get_mut(self.active) {
            v.pop();
        }
    }
}

/// Rows of the rules editor, rebuilt after every change. Headers are
/// skipped when moving the selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleRow {
    Header(&'static str),
    PerDay,
    PerWeek,
    PerMonth,
    Weekdays,
    TargetEnd,
    NonprodFirst,
    /// A freeze, by index into `blackouts`.
    Blackout(usize),
    AddBlackout,
    Holidays,
    Wave(usize),
    AddWave,
}

impl RuleRow {
    pub fn is_header(self) -> bool {
        matches!(self, RuleRow::Header(_))
    }
}

/// The global rules, freezes and waves, editable in words.
pub struct RulesEditor {
    pub rules: precip_engine::schedule::GlobalRules,
    pub blackouts: Vec<precip_engine::schedule::Blackout>,
    pub waves: Vec<precip_engine::schedule::Wave>,
    pub rows: Vec<RuleRow>,
    pub today: jiff::civil::Date,
    pub state: ListState,
    /// The form being filled in for the selected row.
    pub input: Option<Fields>,
    /// A row waiting for a second Enter to delete it.
    pub confirming_delete: bool,
}

impl RulesEditor {
    pub fn rebuild_rows(&mut self) {
        let mut rows = vec![
            RuleRow::Header("PACE"),
            RuleRow::PerDay,
            RuleRow::PerWeek,
            RuleRow::PerMonth,
            RuleRow::Weekdays,
            RuleRow::TargetEnd,
            RuleRow::NonprodFirst,
            RuleRow::Header("FREEZES"),
        ];
        // Freezes that already passed are over; nothing to decide.
        rows.extend(
            self.blackouts
                .iter()
                .enumerate()
                .filter(|(_, b)| b.end >= self.today)
                .map(|(i, _)| RuleRow::Blackout(i)),
        );
        rows.push(RuleRow::AddBlackout);
        rows.push(RuleRow::Holidays);
        rows.push(RuleRow::Header("WAVES"));
        rows.extend((0..self.waves.len()).map(RuleRow::Wave));
        rows.push(RuleRow::AddWave);
        self.rows = rows;
        let i = self.state.selected().unwrap_or(1).min(self.rows.len() - 1);
        self.state.select(Some(if self.rows[i].is_header() {
            i + 1
        } else {
            i
        }));
    }

    pub fn row(&self) -> RuleRow {
        self.rows[self.state.selected().unwrap_or(1)]
    }

    pub fn step(&mut self, delta: isize) {
        let i = self.state.selected().unwrap_or(1);
        self.state.select(Some(step_over_headers(
            i,
            delta,
            self.rows.len(),
            |j| self.rows[j].is_header(),
        )));
    }
}

/// Move a selection by one, skipping header rows and staying in range.
fn step_over_headers(
    from: usize,
    delta: isize,
    len: usize,
    is_header: impl Fn(usize) -> bool,
) -> usize {
    let mut i = from;
    loop {
        let Some(next) = i.checked_add_signed(delta).filter(|n| *n < len)
        else {
            return from;
        };
        i = next;
        if !is_header(i) {
            return i;
        }
    }
}

/// Rows of the per-pair editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairRow {
    Header(&'static str),
    Wave,
    Weekdays,
    Earliest,
    Duration,
    Weight,
    Priority,
    Dependency(usize),
    AddDependency,
    Freeze(usize),
    AddFreeze,
}

impl PairRow {
    pub fn is_header(self) -> bool {
        matches!(self, PairRow::Header(_))
    }
}

/// One pair's migration settings, editable in words.
pub struct PairEditor {
    pub pair: precip_engine::schedule::PairInput,
    /// Freezes that apply only to this pair.
    pub freezes: Vec<precip_engine::schedule::Blackout>,
    pub rows: Vec<PairRow>,
    pub state: ListState,
    pub input: Option<Fields>,
    pub confirming_delete: bool,
}

impl PairEditor {
    pub fn rebuild_rows(&mut self) {
        let mut rows = vec![
            PairRow::Header("WHEN"),
            PairRow::Wave,
            PairRow::Weekdays,
            PairRow::Earliest,
            PairRow::Header("HOW BIG"),
            PairRow::Duration,
            PairRow::Weight,
            PairRow::Priority,
            PairRow::Header("WAITS FOR"),
        ];
        rows.extend((0..self.pair.depends_on.len()).map(PairRow::Dependency));
        rows.push(PairRow::AddDependency);
        rows.push(PairRow::Header("FREEZES FOR THIS PAIR"));
        rows.extend((0..self.freezes.len()).map(PairRow::Freeze));
        rows.push(PairRow::AddFreeze);
        self.rows = rows;
        let i = self.state.selected().unwrap_or(1).min(self.rows.len() - 1);
        self.state.select(Some(if self.rows[i].is_header() {
            i + 1
        } else {
            i
        }));
    }

    pub fn row(&self) -> PairRow {
        self.rows[self.state.selected().unwrap_or(1)]
    }

    pub fn step(&mut self, delta: isize) {
        let i = self.state.selected().unwrap_or(1);
        self.state.select(Some(step_over_headers(
            i,
            delta,
            self.rows.len(),
            |j| self.rows[j].is_header(),
        )));
    }
}

/// Which editor the weekday picker reports back to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WeekdayTarget {
    Global,
    Pair,
}

/// A year's holidays with a tick each; only the ticked ones become
/// freezes, since not every company honours every one.
pub struct HolidayPicker {
    pub year: i16,
    pub holidays: Vec<precip_engine::schedule::holidays::Holiday>,
    pub chosen: Vec<bool>,
    pub state: ListState,
}

/// Pick another pair for the open pair editor to depend on.
pub struct DependsPicker {
    pub candidates: Vec<(String, String)>,
    pub state: ListState,
}

/// Typing the workload a name folds into.
pub struct MergePrompt {
    pub alias: String,
    pub text: String,
}

/// Every computed schedule, oldest first.
pub struct HistoryView {
    pub events: Vec<precip_db::PlanEventRecord>,
    pub state: ListState,
}

/// What the disposition picker sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerMode {
    Pair,
    /// The default for new pairs; `apply` also changes untouched pairs.
    Default {
        apply: bool,
    },
}

/// Picker for confirming a date: the proposed date and its alternatives.
pub struct DatePicker {
    pub workload: String,
    pub environment: String,
    pub dates: Vec<jiff::civil::Date>,
    /// True when a confirmed date is being replaced by another.
    pub replacing: bool,
    pub state: ListState,
}

/// A workload and environment pair opened to show its resources.
pub struct Expanded {
    pub workload: String,
    pub environment: String,
    pub members: Vec<MemberDetail>,
    pub selected: ratatui::widgets::TableState,
    pub history: Vec<DispositionChange>,
}

/// The disposition picker, open over the Workloads tab.
pub struct DispositionPicker {
    pub workload: String,
    pub environment: String,
    pub mode: PickerMode,
    pub state: ListState,
}

pub const DISPOSITIONS: [Disposition; 7] = [
    Disposition::Rehost,
    Disposition::Relocate,
    Disposition::Replatform,
    Disposition::Refactor,
    Disposition::Repurchase,
    Disposition::Retain,
    Disposition::Retire,
];

/// What a disposition means, for the picker.
pub fn disposition_help(d: Disposition) -> &'static str {
    match d {
        Disposition::Rehost => "move the instances to Oxide as they are",
        Disposition::Relocate => "move by re-importing images, no app change",
        Disposition::Replatform => {
            "move with a platform change, same architecture"
        }
        Disposition::Refactor => "re-architect before or during the move",
        Disposition::Repurchase => "replace with a different product",
        Disposition::Retain => "leave in AWS for now",
        Disposition::Retire => "decommission",
    }
}

pub fn disposition_name(d: Disposition) -> String {
    serde_json::to_value(d)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// One line in the activity feed.
#[derive(Debug, Clone)]
pub struct Activity {
    pub at: Instant,
    pub text: String,
}

pub struct App {
    pub screen: Screen,
    store: Option<Store>,
    pub context: Context,
    pub scans: Vec<ScanSummary>,
    pub overview: Option<Overview>,
    pub workloads: Vec<WorkloadSummary>,
    /// Account id to name, from the latest scan.
    pub account_names: BTreeMap<String, String>,
    pub workload_selected: ratatui::widgets::TableState,
    /// The expanded pair's resources, when a workload is open.
    pub expanded: Option<Expanded>,
    pub picker: Option<DispositionPicker>,
    pub sizing: Option<SizingView>,
    pub schedule: Option<ScheduleView>,
    pub date_picker: Option<DatePicker>,
    pub weekday_picker: Option<WeekdayPicker>,
    pub action_menu: Option<ActionMenu>,
    pub rules_editor: Option<RulesEditor>,
    pub pair_editor: Option<PairEditor>,
    pub depends_picker: Option<DependsPicker>,
    pub holiday_picker: Option<HolidayPicker>,
    pub merge_prompt: Option<MergePrompt>,
    pub history: Option<HistoryView>,
    pub weekday_target: WeekdayTarget,
    /// Screen area of the workload table's rows, for mouse clicks.
    pub workload_rows_area: ratatui::layout::Rect,
    pub selected: ListState,
    pub tab: Tab,
    launcher: Launcher,
    pub progress: Option<Progress>,
    pub activity: VecDeque<Activity>,
    pub opened: Instant,
}

const ACTIVITY_LIMIT: usize = 200;

impl App {
    /// Start at the workspace picker, or straight in a workspace when
    /// `preselected` names a database file.
    pub fn new(
        dir: WorkspaceDir,
        preselected: Option<Utf8PathBuf>,
        context: Context,
        launcher: Launcher,
    ) -> Result<Self, TuiError> {
        let mut app = Self {
            screen: Screen::Picker(Picker::new(dir)?),
            store: None,
            context,
            scans: Vec::new(),
            overview: None,
            workloads: Vec::new(),
            account_names: BTreeMap::new(),
            workload_selected: ratatui::widgets::TableState::default(),
            expanded: None,
            picker: None,
            sizing: None,
            schedule: None,
            date_picker: None,
            weekday_picker: None,
            action_menu: None,
            rules_editor: None,
            pair_editor: None,
            depends_picker: None,
            holiday_picker: None,
            merge_prompt: None,
            history: None,
            weekday_target: WeekdayTarget::Global,
            workload_rows_area: ratatui::layout::Rect::default(),
            selected: ListState::default(),
            tab: Tab::Overview,
            launcher,
            progress: None,
            activity: VecDeque::new(),
            opened: Instant::now(),
        };
        if let Some(path) = preselected {
            let name = path.file_stem().unwrap_or("workspace").to_string();
            app.open_workspace(&name, &path)?;
        }
        Ok(app)
    }

    fn open_workspace(
        &mut self,
        name: &str,
        path: &Utf8PathBuf,
    ) -> Result<(), TuiError> {
        let store =
            tokio::runtime::Handle::current().block_on(Store::open(path))?;
        self.store = Some(store);
        self.context.workspace = name.to_string();
        self.screen = Screen::Main;
        self.refresh()?;
        match self.scans.first() {
            Some(s) => self.note(format!(
                "opened {name}; latest scan #{} from {}",
                s.seq,
                s.started.strftime("%Y-%m-%d %H:%M")
            )),
            None => self.note(format!(
                "opened {name}; no scans yet, press s to start one"
            )),
        }
        Ok(())
    }

    pub fn note(&mut self, text: impl Into<String>) {
        self.activity
            .push_front(Activity { at: Instant::now(), text: text.into() });
        self.activity.truncate(ACTIVITY_LIMIT);
    }

    /// Runs the interface. Must be called from a thread that may block,
    /// with a tokio runtime available for the scan tasks.
    pub fn run(mut self) -> Result<(), TuiError> {
        let mut terminal = ratatui::init();
        let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);
        let result = self.event_loop(&mut terminal);
        let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
        ratatui::restore();
        result
    }

    /// Open the selected workload, or close it if already open.
    fn toggle_expand(&mut self) -> Result<(), TuiError> {
        if self.expanded.is_some() {
            self.expanded = None;
            return Ok(());
        }
        let (Some(i), Some(scan), Some(store)) = (
            self.workload_selected.selected(),
            self.scans.first(),
            &self.store,
        ) else {
            return Ok(());
        };
        let Some(w) = self.workloads.get(i) else { return Ok(()) };
        let rt = tokio::runtime::Handle::current();
        let members = rt.block_on(store.pair_members(
            &scan.id,
            &w.workload,
            &w.environment,
        ))?;
        let history = rt
            .block_on(store.disposition_history(&w.workload, &w.environment))?;
        let mut selected = ratatui::widgets::TableState::default();
        if !members.is_empty() {
            selected.select(Some(0));
        }
        self.expanded = Some(Expanded {
            workload: w.workload.clone(),
            environment: w.environment.clone(),
            members,
            selected,
            history,
        });
        Ok(())
    }

    fn adjust_sizing(
        &mut self,
        change: impl FnOnce(&mut precip_engine::sizing::Assumptions),
    ) {
        if let Some(v) = self.sizing.as_mut() {
            change(&mut v.assumptions);
            v.compute();
            let a = v.assumptions;
            self.note(format!(
                "sizing assumptions: growth x{:.1}, threshold {:.0}%, N+{}",
                a.growth_factor,
                a.threshold * 100.0,
                a.headroom
            ));
        }
    }

    /// The pair the user is acting on: the open one, else the selected row.
    fn current_pair(&self) -> Option<(String, String, Disposition)> {
        if let Some(x) = &self.expanded {
            let d = self
                .workloads
                .iter()
                .find(|w| {
                    w.workload == x.workload && w.environment == x.environment
                })
                .map(|w| w.disposition)?;
            return Some((x.workload.clone(), x.environment.clone(), d));
        }
        let w = self.workloads.get(self.workload_selected.selected()?)?;
        Some((w.workload.clone(), w.environment.clone(), w.disposition))
    }

    fn open_picker(&mut self, mode: PickerMode) {
        let (workload, environment, current) = match mode {
            PickerMode::Pair => {
                let Some(pair) = self.current_pair() else { return };
                pair
            }
            PickerMode::Default { .. } => {
                let Some(store) = &self.store else { return };
                let current = tokio::runtime::Handle::current()
                    .block_on(store.default_disposition())
                    .unwrap_or(Disposition::Replatform);
                (String::new(), String::new(), current)
            }
        };
        let mut state = ListState::default();
        state.select(DISPOSITIONS.iter().position(|d| *d == current));
        self.picker =
            Some(DispositionPicker { workload, environment, mode, state });
    }

    fn apply_picker(&mut self) -> Result<(), TuiError> {
        let Some(p) = self.picker.take() else { return Ok(()) };
        let Some(i) = p.state.selected() else { return Ok(()) };
        let Some(store) = &self.store else { return Ok(()) };
        let chosen = DISPOSITIONS[i];
        let rt = tokio::runtime::Handle::current();
        match p.mode {
            PickerMode::Pair => {
                rt.block_on(store.set_disposition(
                    &p.workload,
                    &p.environment,
                    chosen,
                ))?;
                self.note(format!(
                    "{} / {}: disposition set to {}",
                    p.workload,
                    p.environment,
                    disposition_name(chosen)
                ));
            }
            PickerMode::Default { apply } => {
                rt.block_on(store.set_default_disposition(chosen, apply))?;
                self.note(format!(
                    "new pairs start as {}{}",
                    disposition_name(chosen),
                    if apply {
                        "; pairs nobody set by hand changed too"
                    } else {
                        ""
                    }
                ));
            }
        }
        self.refresh()?;
        if self.expanded.is_some() {
            // Reload so the history pane shows the change.
            self.expanded = None;
            self.workload_selected.select(self.workloads.iter().position(
                |w| w.workload == p.workload && w.environment == p.environment,
            ));
            self.toggle_expand()?;
        }
        Ok(())
    }

    fn click_workload_row(
        &mut self,
        column: u16,
        row: u16,
    ) -> Result<(), TuiError> {
        let area = self.workload_rows_area;
        let inside = column >= area.x
            && column < area.x + area.width
            && row >= area.y
            && row < area.y + area.height;
        if !inside || self.tab != Tab::Workloads {
            return Ok(());
        }
        let offset = self.workload_selected.offset();
        let index = usize::from(row - area.y) + offset;
        if index < self.workloads.len() {
            self.workload_selected.select(Some(index));
            self.expanded = None;
            self.toggle_expand()?;
        }
        Ok(())
    }

    fn start_scan(&mut self) {
        if self.progress.as_ref().is_some_and(Progress::running) {
            self.note("a scan is already running");
            return;
        }
        let Some(store) = self.store.clone() else { return };
        let handle = (self.launcher)(store);
        self.progress = Some(Progress {
            handle,
            started: Instant::now(),
            accounts_total: 0,
            accounts: BTreeMap::new(),
            outcome: None,
        });
        self.tab = Tab::Accounts;
        self.note("scan started: resolving credentials and organization");
    }

    fn refresh(&mut self) -> Result<(), TuiError> {
        let Some(store) = &self.store else { return Ok(()) };
        let rt = tokio::runtime::Handle::current();
        self.scans = rt.block_on(store.list_scans())?;
        self.overview = match self.scans.first() {
            Some(s) => Some(rt.block_on(store.overview(s.seq))?),
            None => None,
        };
        self.workloads = match self.scans.first() {
            Some(s) => rt.block_on(store.workload_summary(&s.id))?,
            None => Vec::new(),
        };
        self.account_names = match self.scans.first() {
            Some(s) => rt.block_on(store.account_names(s.seq))?,
            None => BTreeMap::new(),
        };
        // Keep the assumptions the user has set across refreshes.
        let assumptions =
            self.sizing.as_ref().map(|v| v.assumptions).unwrap_or_default();
        self.sizing = match self.scans.first() {
            Some(s) => {
                let profile =
                    precip_engine::sizing::profiles::builtin("gen2-1152")
                        .and_then(Result::ok);
                match profile {
                    Some(profile) => {
                        let instances = rt
                            .block_on(store.sized_instances(&s.id, &profile))?;
                        let result = precip_engine::sizing::size(
                            &profile,
                            &[],
                            &assumptions,
                        );
                        let mut view = SizingView {
                            profile,
                            assumptions,
                            instances,
                            result,
                            demand: Vec::new(),
                        };
                        view.compute();
                        Some(view)
                    }
                    None => None,
                }
            }
            None => None,
        };
        if self.workload_selected.selected().is_none()
            && !self.workloads.is_empty()
        {
            self.workload_selected.select(Some(0));
        }
        if self.selected.selected().is_none() && !self.scans.is_empty() {
            self.selected.select(Some(0));
        }
        self.recompute_schedule("open")?;
        Ok(())
    }

    /// Recompute the calendar from today and keep the selection.
    fn recompute_schedule(&mut self, trigger: &str) -> Result<(), TuiError> {
        let Some(store) = &self.store else { return Ok(()) };
        let rt = tokio::runtime::Handle::current();
        let today = jiff::Zoned::now().date();
        let rules = rt.block_on(store.global_rules())?;
        // Every change already produced a version through the event log;
        // only opening the workspace (a new day) computes afresh.
        let latest = if trigger == "open" {
            None
        } else {
            rt.block_on(store.latest_schedule())?
                .filter(|(_, as_of, _)| *as_of == today)
        };
        let (version, schedule) = match latest {
            Some((id, _, schedule)) => (id, schedule),
            None => rt.block_on(store.compute_schedule(today, trigger))?,
        };
        let metrics = rt.block_on(store.metrics(today))?;
        let findings = match self.scans.first() {
            Some(latest) => rt.block_on(store.verify_migrated(&latest.id))?,
            None => Vec::new(),
        };
        let mut selected =
            self.schedule.as_ref().map(|v| v.selected).unwrap_or_default();
        if selected.selected().is_none() && !schedule.slots.is_empty() {
            selected.select(Some(0));
        }
        let expired = schedule.expired.clone();
        self.schedule = Some(ScheduleView {
            rules,
            schedule,
            metrics,
            findings,
            version,
            selected,
            today,
        });
        for (w, e) in expired {
            self.note(format!(
                "{w} / {e}: confirmed date passed without a migration, rescheduled"
            ));
        }
        let conflicts =
            self.schedule.as_ref().map(|v| v.schedule.conflicts.clone());
        for ((w, e), why) in conflicts.unwrap_or_default() {
            self.note(format!("{w} / {e}: confirmed date {why}"));
        }
        Ok(())
    }

    fn selected_slot(&self) -> Option<precip_engine::schedule::Slot> {
        let v = self.schedule.as_ref()?;
        v.schedule.slots.get(v.selected.selected()?).cloned()
    }

    fn change_status(
        &mut self,
        workload: &str,
        environment: &str,
        status: precip_engine::schedule::Status,
        date: Option<jiff::civil::Date>,
    ) -> Result<(), TuiError> {
        let Some(store) = &self.store else { return Ok(()) };
        let today = jiff::Zoned::now().date();
        let result =
            tokio::runtime::Handle::current().block_on(store.set_pair_status(
                workload,
                environment,
                status,
                date,
                today,
                None,
            ));
        match result {
            Ok(()) => {
                let what = match status {
                    precip_engine::schedule::Status::Proposed => {
                        "back to proposed"
                    }
                    precip_engine::schedule::Status::Confirmed => "date agreed",
                    precip_engine::schedule::Status::Missed => {
                        "didn't happen, a new date is proposed"
                    }
                    precip_engine::schedule::Status::Migrated => "done",
                    precip_engine::schedule::Status::RolledBack => {
                        "rolled back, a new date is proposed"
                    }
                };
                self.note(format!(
                    "{workload} / {environment}: {what}{}",
                    date.map(|d| format!(" ({d})")).unwrap_or_default()
                ));
                self.recompute_schedule("status_change")?;
            }
            Err(precip_db::DbError::Invalid(msg)) => {
                self.note(format!("{workload} / {environment}: {msg}"));
            }
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }

    /// Open the menu for the selected row: a slot, or a finding below
    /// the slots.
    /// Open the menu for the selected schedule row: a slot, or a finding
    /// below the slots.
    fn open_action_menu(&mut self) {
        let Some(v) = &self.schedule else { return };
        let Some(i) = v.selected.selected() else { return };
        let today = v.today;
        let (workload, environment, actions) =
            if let Some(slot) = v.schedule.slots.get(i) {
                let mut actions = Vec::new();
                if slot.locked {
                    actions.push(Action::ChangeDate);
                    actions.push(Action::Done);
                    if slot.date <= today {
                        actions.push(Action::DidNotHappen);
                    }
                } else {
                    actions.push(Action::AgreeDate);
                    actions.push(Action::Done);
                }
                actions.push(Action::PairSettings);
                (slot.workload.clone(), slot.environment.clone(), actions)
            } else if let Some(f) = v.findings.get(i - v.schedule.slots.len()) {
                (
                    f.workload.clone(),
                    f.environment.clone(),
                    vec![Action::RolledBack],
                )
            } else {
                return;
            };
        let title = format!("{workload} / {environment}");
        self.show_menu(title, workload, environment, actions);
    }

    /// The menu for the selected workload row, plus what applies to the
    /// whole tab.
    fn open_workload_menu(&mut self) {
        let pair = self.current_pair();
        let mut actions = Vec::new();
        if pair.is_some() {
            actions.extend([
                Action::ShowResources,
                Action::ChangeDisposition,
                Action::PairSettings,
                Action::MergeInto,
            ]);
        }
        actions.extend([
            Action::DefaultDisposition,
            Action::DefaultDispositionApply,
            Action::Regroup,
            Action::Prune,
            Action::PruneAll,
        ]);
        let (title, workload, environment) = match pair {
            Some((w, e, _)) => (format!("{w} / {e}"), w, e),
            None => ("Workloads".to_string(), String::new(), String::new()),
        };
        self.show_menu(title, workload, environment, actions);
    }

    fn show_menu(
        &mut self,
        title: String,
        workload: String,
        environment: String,
        actions: Vec<Action>,
    ) {
        let mut state = ListState::default();
        state.select(Some(0));
        self.action_menu = Some(ActionMenu {
            title,
            workload,
            environment,
            actions,
            state,
            confirming: None,
        });
    }

    /// Enter in the action menu: run the action, or ask first.
    fn action_menu_enter(&mut self) -> Result<(), TuiError> {
        let Some(m) = self.action_menu.as_mut() else { return Ok(()) };
        let Some(action) = m.state.selected().and_then(|i| m.actions.get(i))
        else {
            return Ok(());
        };
        let action = *action;
        if action.needs_confirmation() && m.confirming != Some(action) {
            m.confirming = Some(action);
            return Ok(());
        }
        let (w, e) = (m.workload.clone(), m.environment.clone());
        self.action_menu = None;
        match action {
            Action::AgreeDate | Action::ChangeDate => self.open_date_picker(),
            Action::Done => self.change_status(
                &w,
                &e,
                precip_engine::schedule::Status::Migrated,
                None,
            )?,
            Action::DidNotHappen => self.change_status(
                &w,
                &e,
                precip_engine::schedule::Status::Missed,
                None,
            )?,
            Action::RolledBack => self.change_status(
                &w,
                &e,
                precip_engine::schedule::Status::RolledBack,
                None,
            )?,
            Action::ShowResources => {
                if self.expanded.is_none() {
                    self.toggle_expand()?;
                }
            }
            Action::ChangeDisposition => self.open_picker(PickerMode::Pair),
            Action::PairSettings => self.open_pair_editor(&w, &e)?,
            Action::MergeInto => {
                self.merge_prompt =
                    Some(MergePrompt { alias: w, text: String::new() });
            }
            Action::DefaultDisposition => {
                self.open_picker(PickerMode::Default { apply: false });
            }
            Action::DefaultDispositionApply => {
                self.open_picker(PickerMode::Default { apply: true });
            }
            Action::Regroup => self.regroup()?,
            Action::Prune => self.prune(true)?,
            Action::PruneAll => self.prune(false)?,
        }
        Ok(())
    }

    fn regroup(&mut self) -> Result<(), TuiError> {
        let (Some(store), Some(scan)) = (&self.store, self.scans.first())
        else {
            return Ok(());
        };
        let stats = tokio::runtime::Handle::current().block_on(
            store
                .group_scan(&scan.id, &precip_types::GroupingConfig::default()),
        )?;
        self.note(format!(
            "regrouped scan #{}: {} workloads, {} resources unassigned",
            scan.seq, stats.workloads, stats.unassigned
        ));
        self.expanded = None;
        self.refresh()
    }

    fn prune(&mut self, keep_user_changed: bool) -> Result<(), TuiError> {
        let (Some(store), Some(scan)) = (&self.store, self.scans.first())
        else {
            return Ok(());
        };
        let removed = tokio::runtime::Handle::current()
            .block_on(store.prune_stale_pairs(&scan.id, keep_user_changed))?;
        self.note(format!("removed {removed} stale pair(s)"));
        self.expanded = None;
        self.refresh()
    }

    fn apply_merge(&mut self) -> Result<(), TuiError> {
        let Some(p) = self.merge_prompt.take() else { return Ok(()) };
        let Some(store) = &self.store else { return Ok(()) };
        let into = p.text.trim().to_string();
        if into.is_empty() || into == p.alias {
            self.note("type the workload this name belongs to");
            return Ok(());
        }
        tokio::runtime::Handle::current()
            .block_on(store.merge_workload(&p.alias, &into))?;
        self.note(format!("{} is now part of {into}", p.alias));
        self.regroup()
    }

    fn open_history(&mut self) -> Result<(), TuiError> {
        let Some(store) = &self.store else { return Ok(()) };
        let events =
            tokio::runtime::Handle::current().block_on(store.events())?;
        let mut state = ListState::default();
        if !events.is_empty() {
            state.select(Some(events.len() - 1));
        }
        self.history = Some(HistoryView { events, state });
        Ok(())
    }

    // ---- the rules editor -------------------------------------------

    fn open_rules_editor(&mut self) -> Result<(), TuiError> {
        let Some(store) = &self.store else { return Ok(()) };
        let rt = tokio::runtime::Handle::current();
        let rules = rt.block_on(store.global_rules())?;
        let blackouts = rt.block_on(store.blackouts())?;
        let waves = rt.block_on(store.waves())?;
        let mut state = ListState::default();
        state.select(Some(1));
        let mut ed = RulesEditor {
            rules,
            blackouts,
            waves,
            rows: Vec::new(),
            today: jiff::Zoned::now().date(),
            state,
            input: None,
            confirming_delete: false,
        };
        ed.rebuild_rows();
        self.rules_editor = Some(ed);
        Ok(())
    }

    fn save_rules(&mut self) -> Result<(), TuiError> {
        let Some(ed) = &self.rules_editor else { return Ok(()) };
        let Some(store) = &self.store else { return Ok(()) };
        let rules = ed.rules.clone();
        tokio::runtime::Handle::current()
            .block_on(store.set_global_rules(&rules))?;
        self.recompute_schedule("rule_change")
    }

    /// Left and right on a rules row: nudge a number or flip a switch.
    fn rules_adjust(&mut self, delta: i64) -> Result<(), TuiError> {
        let Some(ed) = self.rules_editor.as_mut() else { return Ok(()) };
        if ed.input.is_some() {
            return Ok(());
        }
        let bump = |n: u32| -> u32 {
            u32::try_from((i64::from(n) + delta).clamp(1, 1000)).unwrap_or(1)
        };
        match ed.row() {
            RuleRow::PerDay => ed.rules.per_day = bump(ed.rules.per_day),
            RuleRow::PerWeek => ed.rules.per_week = bump(ed.rules.per_week),
            RuleRow::PerMonth => ed.rules.per_month = bump(ed.rules.per_month),
            RuleRow::TargetEnd => {
                // The target end moves a week at a time.
                if let Some(t) = ed.rules.target_end {
                    ed.rules.target_end =
                        t.checked_add(jiff::Span::new().weeks(delta)).ok();
                }
            }
            RuleRow::NonprodFirst => {
                ed.rules.nonprod_before_prod = !ed.rules.nonprod_before_prod;
            }
            RuleRow::Header(_)
            | RuleRow::Weekdays
            | RuleRow::Blackout(_)
            | RuleRow::AddBlackout
            | RuleRow::Wave(_)
            | RuleRow::AddWave
            | RuleRow::Holidays => return Ok(()),
        }
        self.save_rules()
    }

    /// Enter on a rules row: open the day picker, start typing, flip the
    /// switch, or delete a freeze (asked twice).
    fn rules_enter(&mut self) -> Result<(), TuiError> {
        let Some(ed) = self.rules_editor.as_mut() else { return Ok(()) };
        if let Some(fields) = ed.input.take() {
            return self.rules_apply_input(&fields);
        }
        match ed.row() {
            RuleRow::Weekdays => {
                let days = ed.rules.weekdays.clone();
                self.weekday_target = WeekdayTarget::Global;
                self.open_weekday_picker_from(days);
            }
            RuleRow::TargetEnd => {
                let current = ed
                    .rules
                    .target_end
                    .map(|d| d.to_string())
                    .unwrap_or_default();
                ed.input = Some(Fields::new(&["date"], &[&current]));
            }
            RuleRow::NonprodFirst => return self.rules_adjust(1),
            RuleRow::AddBlackout => {
                ed.input = Some(Fields::new(&["name", "from", "to"], &[]));
            }
            RuleRow::AddWave => {
                ed.input = Some(Fields::new(&["number", "name", "due"], &[]));
            }
            RuleRow::Holidays => ed.input = Some(Fields::new(&["year"], &[])),
            RuleRow::Wave(i) => {
                let w = &ed.waves[i];
                let due =
                    w.target_end.map(|d| d.to_string()).unwrap_or_default();
                ed.input =
                    Some(Fields::new(&["name", "due"], &[&w.name, &due]));
            }
            RuleRow::Blackout(i) => {
                if !ed.confirming_delete {
                    ed.confirming_delete = true;
                    return Ok(());
                }
                let b = ed.blackouts.remove(i);
                ed.confirming_delete = false;
                ed.rebuild_rows();
                if let Some(store) = &self.store {
                    tokio::runtime::Handle::current()
                        .block_on(store.remove_blackout(&b))?;
                }
                self.note(format!("removed the {} freeze", b.name));
                return self.recompute_schedule("rule_change");
            }
            RuleRow::Header(_)
            | RuleRow::PerDay
            | RuleRow::PerWeek
            | RuleRow::PerMonth => {}
        }
        Ok(())
    }

    /// Backspace on a wave row: ask once, then remove it.
    fn rules_remove_wave(&mut self) -> Result<(), TuiError> {
        let Some(ed) = self.rules_editor.as_mut() else { return Ok(()) };
        let RuleRow::Wave(i) = ed.row() else { return Ok(()) };
        if !ed.confirming_delete {
            ed.confirming_delete = true;
            return Ok(());
        }
        ed.confirming_delete = false;
        let number = ed.waves[i].number;
        let Some(store) = &self.store else { return Ok(()) };
        match tokio::runtime::Handle::current()
            .block_on(store.remove_wave(number))
        {
            Ok(moved) => {
                self.reload_rules_editor()?;
                self.note(format!(
                    "wave {number} removed; {moved} pair(s) back in wave 1"
                ));
                self.recompute_schedule("rule_change")
            }
            Err(precip_db::DbError::Invalid(msg)) => {
                self.note(msg);
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Re-read freezes and waves after a change the editor cannot apply
    /// to its own copy.
    fn reload_rules_editor(&mut self) -> Result<(), TuiError> {
        let Some(store) = &self.store else { return Ok(()) };
        let Some(ed) = self.rules_editor.as_mut() else { return Ok(()) };
        let rt = tokio::runtime::Handle::current();
        ed.blackouts = rt.block_on(store.blackouts())?;
        ed.waves = rt.block_on(store.waves())?;
        ed.rebuild_rows();
        Ok(())
    }

    /// Typed text for the target end ("2026-12-15" or empty to clear), a
    /// new freeze ("name 2026-12-20 2027-01-05"), a wave ("3 finance
    /// 2026-12-01" to add, "finance 2026-12-01" on a wave row), or a year
    /// of US holidays.
    fn rules_apply_input(&mut self, f: &Fields) -> Result<(), TuiError> {
        let Some(ed) = self.rules_editor.as_mut() else { return Ok(()) };
        let Some(store) = &self.store else { return Ok(()) };
        let rt = tokio::runtime::Handle::current();
        match ed.row() {
            RuleRow::TargetEnd => match parse_optional_date(f.get(0)) {
                Ok(d) => {
                    ed.rules.target_end = d;
                    self.save_rules()
                }
                Err(msg) => {
                    self.note(msg);
                    Ok(())
                }
            },
            RuleRow::AddBlackout => {
                let b = match parse_freeze(f.get(0), f.get(1), f.get(2), None) {
                    Ok(b) => b,
                    Err(msg) => {
                        self.note(msg);
                        return Ok(());
                    }
                };
                ed.blackouts.push(b.clone());
                ed.blackouts.sort_by_key(|b| (b.start, b.name.clone()));
                ed.rebuild_rows();
                rt.block_on(store.add_blackout(&b))?;
                self.note(format!(
                    "no migrations from {} to {} ({})",
                    b.start, b.end, b.name
                ));
                self.recompute_schedule("rule_change")
            }
            RuleRow::Holidays => {
                let Ok(year) = f.get(0).parse::<i16>() else {
                    self.note("the year should look like 2027");
                    return Ok(());
                };
                let holidays =
                    precip_engine::schedule::holidays::us_federal(year);
                let chosen = vec![true; holidays.len()];
                let mut state = ListState::default();
                state.select(Some(0));
                self.holiday_picker =
                    Some(HolidayPicker { year, holidays, chosen, state });
                Ok(())
            }
            RuleRow::Wave(i) => {
                let number = ed.waves[i].number;
                let name = f.get(0).to_string();
                let due = match parse_optional_date(f.get(1)) {
                    Ok(d) => d,
                    Err(msg) => {
                        self.note(msg);
                        return Ok(());
                    }
                };
                rt.block_on(store.set_wave(number, &name, due))?;
                ed.waves = rt.block_on(store.waves())?;
                ed.rebuild_rows();
                self.note(format!("wave {number} is now {name}"));
                self.recompute_schedule("rule_change")
            }
            RuleRow::AddWave => {
                let Ok(number) = f.get(0).parse::<u32>() else {
                    self.note(
                        "the wave number should be a whole number like 3",
                    );
                    return Ok(());
                };
                let name = f.get(1).to_string();
                let due = match parse_optional_date(f.get(2)) {
                    Ok(d) => d,
                    Err(msg) => {
                        self.note(msg);
                        return Ok(());
                    }
                };
                match rt.block_on(store.set_wave(number, &name, due)) {
                    Ok(()) => {}
                    Err(precip_db::DbError::Invalid(msg)) => {
                        self.note(msg);
                        return Ok(());
                    }
                    Err(e) => return Err(e.into()),
                }
                ed.waves = rt.block_on(store.waves())?;
                ed.rebuild_rows();
                self.note(format!("wave {number} added"));
                self.recompute_schedule("rule_change")
            }
            RuleRow::Header(_)
            | RuleRow::PerDay
            | RuleRow::PerWeek
            | RuleRow::PerMonth
            | RuleRow::Weekdays
            | RuleRow::NonprodFirst
            | RuleRow::Blackout(_) => Ok(()),
        }
    }

    // ---- the per-pair editor ----------------------------------------

    fn open_pair_editor(
        &mut self,
        workload: &str,
        environment: &str,
    ) -> Result<(), TuiError> {
        let Some(store) = &self.store else { return Ok(()) };
        let rt = tokio::runtime::Handle::current();
        let Some(pair) = rt
            .block_on(store.schedule_inputs())?
            .into_iter()
            .find(|p| p.workload == workload && p.environment == environment)
        else {
            self.note(format!(
                "{workload} / {environment} is not migrating (retire or retain), so it has no migration settings"
            ));
            return Ok(());
        };
        let freezes = rt
            .block_on(store.blackouts())?
            .into_iter()
            .filter(|b| {
                b.pair
                    .as_ref()
                    .is_some_and(|(w, e)| w == workload && e == environment)
            })
            .collect();
        let mut state = ListState::default();
        state.select(Some(1));
        let mut ed = PairEditor {
            pair,
            freezes,
            rows: Vec::new(),
            state,
            input: None,
            confirming_delete: false,
        };
        ed.rebuild_rows();
        self.pair_editor = Some(ed);
        Ok(())
    }

    fn save_pair(&mut self) -> Result<(), TuiError> {
        let Some(ed) = &self.pair_editor else { return Ok(()) };
        let Some(store) = &self.store else { return Ok(()) };
        let p = ed.pair.clone();
        let rt = tokio::runtime::Handle::current();
        rt.block_on(store.set_pair_rule(
            &p.workload,
            &p.environment,
            &p.weekdays,
            p.earliest,
            p.priority,
        ))?;
        rt.block_on(store.set_pair_plan(
            &p.workload,
            &p.environment,
            p.wave,
            p.duration_days,
            p.weight,
        ))?;
        self.recompute_schedule("rule_change")
    }

    fn pair_adjust(&mut self, delta: i64) -> Result<(), TuiError> {
        let Some(ed) = self.pair_editor.as_mut() else { return Ok(()) };
        if ed.input.is_some() {
            return Ok(());
        }
        let bump = |n: u32| -> u32 {
            u32::try_from((i64::from(n) + delta).clamp(1, 1000)).unwrap_or(1)
        };
        match ed.row() {
            PairRow::Wave => ed.pair.wave = bump(ed.pair.wave),
            PairRow::Duration => {
                ed.pair.duration_days = bump(ed.pair.duration_days);
            }
            PairRow::Weight => ed.pair.weight = bump(ed.pair.weight),
            PairRow::Priority => {
                ed.pair.priority = i32::try_from(
                    (i64::from(ed.pair.priority) + delta).clamp(-100, 100),
                )
                .unwrap_or(0);
            }
            PairRow::Earliest => {
                if let Some(t) = ed.pair.earliest {
                    ed.pair.earliest =
                        t.checked_add(jiff::Span::new().weeks(delta)).ok();
                }
            }
            PairRow::Header(_)
            | PairRow::Weekdays
            | PairRow::Dependency(_)
            | PairRow::AddDependency
            | PairRow::Freeze(_)
            | PairRow::AddFreeze => return Ok(()),
        }
        self.save_pair()
    }

    fn pair_enter(&mut self) -> Result<(), TuiError> {
        let Some(ed) = self.pair_editor.as_mut() else { return Ok(()) };
        if let Some(fields) = ed.input.take() {
            return self.pair_apply_input(&fields);
        }
        let Some(store) = &self.store else { return Ok(()) };
        let rt = tokio::runtime::Handle::current();
        match ed.row() {
            PairRow::Weekdays => {
                let days = ed.pair.weekdays.clone();
                self.weekday_target = WeekdayTarget::Pair;
                self.open_weekday_picker_from(days);
            }
            PairRow::Earliest => {
                let current =
                    ed.pair.earliest.map(|d| d.to_string()).unwrap_or_default();
                ed.input = Some(Fields::new(&["date"], &[&current]));
            }
            PairRow::AddFreeze => {
                ed.input = Some(Fields::new(&["name", "from", "to"], &[]));
            }
            PairRow::AddDependency => {
                let me =
                    (ed.pair.workload.clone(), ed.pair.environment.clone());
                let have = ed.pair.depends_on.clone();
                let candidates: Vec<(String, String)> = rt
                    .block_on(store.schedule_inputs())?
                    .into_iter()
                    .map(|p| (p.workload, p.environment))
                    .filter(|k| *k != me && !have.contains(k))
                    .collect();
                if candidates.is_empty() {
                    self.note("no other migrating pair to depend on");
                    return Ok(());
                }
                let mut state = ListState::default();
                state.select(Some(0));
                self.depends_picker = Some(DependsPicker { candidates, state });
            }
            PairRow::Dependency(i) => {
                if !ed.confirming_delete {
                    ed.confirming_delete = true;
                    return Ok(());
                }
                let (w, e) = ed.pair.depends_on.remove(i);
                ed.confirming_delete = false;
                ed.rebuild_rows();
                let me =
                    (ed.pair.workload.clone(), ed.pair.environment.clone());
                rt.block_on(store.remove_dependency(&me.0, &me.1, &w, &e))?;
                self.note(format!(
                    "{} / {} no longer waits for {w} / {e}",
                    me.0, me.1
                ));
                return self.recompute_schedule("rule_change");
            }
            PairRow::Freeze(i) => {
                if !ed.confirming_delete {
                    ed.confirming_delete = true;
                    return Ok(());
                }
                let b = ed.freezes.remove(i);
                ed.confirming_delete = false;
                ed.rebuild_rows();
                rt.block_on(store.remove_blackout(&b))?;
                self.note(format!("removed the {} freeze", b.name));
                return self.recompute_schedule("rule_change");
            }
            PairRow::Header(_)
            | PairRow::Wave
            | PairRow::Duration
            | PairRow::Weight
            | PairRow::Priority => {}
        }
        Ok(())
    }

    fn pair_apply_input(&mut self, f: &Fields) -> Result<(), TuiError> {
        let Some(ed) = self.pair_editor.as_mut() else { return Ok(()) };
        let Some(store) = &self.store else { return Ok(()) };
        match ed.row() {
            PairRow::Earliest => match parse_optional_date(f.get(0)) {
                Ok(d) => {
                    ed.pair.earliest = d;
                    self.save_pair()
                }
                Err(msg) => {
                    self.note(msg);
                    Ok(())
                }
            },
            PairRow::AddFreeze => {
                let me =
                    (ed.pair.workload.clone(), ed.pair.environment.clone());
                let b = match parse_freeze(
                    f.get(0),
                    f.get(1),
                    f.get(2),
                    Some(me),
                ) {
                    Ok(b) => b,
                    Err(msg) => {
                        self.note(msg);
                        return Ok(());
                    }
                };
                ed.freezes.push(b.clone());
                ed.rebuild_rows();
                tokio::runtime::Handle::current()
                    .block_on(store.add_blackout(&b))?;
                self.note(format!(
                    "{} / {}: no migration from {} to {} ({})",
                    b.pair.as_ref().map(|p| p.0.as_str()).unwrap_or_default(),
                    b.pair.as_ref().map(|p| p.1.as_str()).unwrap_or_default(),
                    b.start,
                    b.end,
                    b.name
                ));
                self.recompute_schedule("rule_change")
            }
            PairRow::Header(_)
            | PairRow::Wave
            | PairRow::Duration
            | PairRow::Weight
            | PairRow::Priority
            | PairRow::Weekdays
            | PairRow::Dependency(_)
            | PairRow::AddDependency
            | PairRow::Freeze(_) => Ok(()),
        }
    }

    fn apply_holidays(&mut self) -> Result<(), TuiError> {
        let Some(p) = self.holiday_picker.take() else { return Ok(()) };
        let Some(store) = &self.store else { return Ok(()) };
        let chosen: Vec<_> = p
            .holidays
            .into_iter()
            .zip(p.chosen)
            .filter(|(_, on)| *on)
            .map(|(h, _)| h)
            .collect();
        tokio::runtime::Handle::current()
            .block_on(store.add_holidays(&chosen))?;
        self.reload_rules_editor()?;
        self.note(format!(
            "added {} of the US federal holidays for {}",
            chosen.len(),
            p.year
        ));
        self.recompute_schedule("rule_change")
    }

    fn apply_dependency(&mut self) -> Result<(), TuiError> {
        let Some(p) = self.depends_picker.take() else { return Ok(()) };
        let Some(chosen) = p.state.selected().and_then(|i| p.candidates.get(i))
        else {
            return Ok(());
        };
        let Some(ed) = self.pair_editor.as_mut() else { return Ok(()) };
        let Some(store) = &self.store else { return Ok(()) };
        let me = (ed.pair.workload.clone(), ed.pair.environment.clone());
        ed.pair.depends_on.push(chosen.clone());
        ed.rebuild_rows();
        tokio::runtime::Handle::current().block_on(
            store.add_dependency(&me.0, &me.1, &chosen.0, &chosen.1),
        )?;
        self.note(format!(
            "{} / {} now waits for {} / {}",
            me.0, me.1, chosen.0, chosen.1
        ));
        self.recompute_schedule("rule_change")
    }

    fn open_weekday_picker_from(&mut self, days: Vec<jiff::civil::Weekday>) {
        let mut state = ListState::default();
        state.select(Some(0));
        self.weekday_picker = Some(WeekdayPicker { days, state });
    }

    /// Apply the highlighted row of the weekday picker: a preset replaces
    /// the set, a day toggles in or out.
    fn toggle_weekday_row(&mut self) {
        let Some(p) = self.weekday_picker.as_mut() else { return };
        let Some(i) = p.state.selected() else { return };
        match i {
            0 => p.days.clear(),
            1 => p.days = ALL_WEEKDAYS[..5].to_vec(),
            2 => p.days = ALL_WEEKDAYS[5..].to_vec(),
            _ => {
                let day = ALL_WEEKDAYS[i - 3];
                if let Some(pos) = p.days.iter().position(|d| *d == day) {
                    p.days.remove(pos);
                } else {
                    p.days.push(day);
                }
                p.days.sort_by_key(|d| d.to_monday_zero_offset());
                if p.days.len() == ALL_WEEKDAYS.len() {
                    p.days.clear();
                }
            }
        }
    }

    /// Save the weekday picker's selection as the global rule and
    /// recompute.
    /// Save the weekday picker's selection where it came from: the global
    /// rule, or the open pair's rule.
    fn close_weekday_picker(&mut self) -> Result<(), TuiError> {
        let Some(p) = self.weekday_picker.take() else { return Ok(()) };
        match self.weekday_target {
            WeekdayTarget::Pair => {
                let Some(ed) = self.pair_editor.as_mut() else { return Ok(()) };
                if ed.pair.weekdays == p.days {
                    return Ok(());
                }
                ed.pair.weekdays = p.days;
                self.save_pair()
            }
            WeekdayTarget::Global => {
                let Some(store) = &self.store else { return Ok(()) };
                let Some(v) = &self.schedule else { return Ok(()) };
                if p.days == v.rules.weekdays {
                    return Ok(());
                }
                let mut rules = v.rules.clone();
                rules.weekdays = p.days;
                tokio::runtime::Handle::current()
                    .block_on(store.set_global_rules(&rules))?;
                self.note(format!(
                    "migrations may run on {}",
                    weekday_rule_label(&rules.weekdays)
                ));
                if let Some(ed) = self.rules_editor.as_mut() {
                    ed.rules.weekdays = rules.weekdays.clone();
                }
                self.recompute_schedule("rule_change")
            }
        }
    }

    fn open_date_picker(&mut self) {
        let Some(slot) = self.selected_slot() else { return };
        // A confirmed date never moves, but another future date can be
        // confirmed in its place; the picker then offers only alternatives.
        let mut dates = if slot.locked { Vec::new() } else { vec![slot.date] };
        dates.extend(slot.alternatives.iter().copied());
        if dates.is_empty() {
            self.note("no open dates to offer; adjust the rules or blackouts");
            return;
        }
        let mut state = ListState::default();
        state.select(Some(0));
        self.date_picker = Some(DatePicker {
            workload: slot.workload,
            environment: slot.environment,
            dates,
            replacing: slot.locked,
            state,
        });
    }

    fn apply(&mut self, e: ScanEvent) {
        let Some(p) = self.progress.as_mut() else { return };
        let line = match &e {
            ScanEvent::AccountsDiscovered { total } => {
                p.accounts_total = *total;
                format!("organization enumerated: {total} accounts to scan")
            }
            ScanEvent::AccountStarted { account_id, name } => {
                let a = p.accounts.entry(account_id.clone()).or_default();
                a.name = name.clone();
                let label = name.as_deref().unwrap_or(account_id);
                format!(
                    "{label} ({account_id}): credentials ok, listing regions"
                )
            }
            ScanEvent::RegionsPlanned { account_id, regions, skipped } => {
                let a = p.accounts.entry(account_id.clone()).or_default();
                a.planned = *regions;
                a.skipped = *skipped;
                if *skipped > 0 {
                    format!(
                        "{account_id}: {regions} regions have resources, {skipped} skipped"
                    )
                } else {
                    format!("{account_id}: scanning {regions} regions")
                }
            }
            ScanEvent::RegionFinished {
                account_id,
                region,
                status,
                resources,
            } => {
                let a = p.accounts.entry(account_id.clone()).or_default();
                a.done += 1;
                a.resources += resources;
                match status {
                    TargetStatus::Scanned | TargetStatus::Empty => {
                        format!("{account_id} {region}: {resources} resources")
                    }
                    TargetStatus::Partial(apis) => {
                        a.failed += 1;
                        format!(
                            "{account_id} {region}: {resources} resources, failed {apis}"
                        )
                    }
                    TargetStatus::Failed(err) => {
                        a.failed += 1;
                        format!("{account_id} {region}: failed, {err}")
                    }
                }
            }
            ScanEvent::AccountFinished { account_id, resources } => {
                let a = p.accounts.entry(account_id.clone()).or_default();
                a.resources = *resources;
                format!("{account_id}: done, {resources} resources")
            }
            ScanEvent::AccountSkipped { account_id, reason } => {
                let a = p.accounts.entry(account_id.clone()).or_default();
                a.note = Some(reason.clone());
                format!("{account_id}: skipped, {reason}")
            }
        };
        self.note(line);
    }

    /// Drain pending events and notice completion.
    fn poll(&mut self) -> Result<(), TuiError> {
        let mut pending = Vec::new();
        let mut finished = None;
        if let Some(p) = self.progress.as_mut() {
            while let Ok(e) = p.handle.events.try_recv() {
                pending.push(e);
            }
            if p.outcome.is_none() && p.handle.done.is_finished() {
                let result = tokio::runtime::Handle::current()
                    .block_on(&mut p.handle.done)
                    .unwrap_or_else(|e| Err(format!("scan task failed: {e}")));
                p.outcome = Some(result.clone());
                finished = Some(result);
            }
        }
        for e in pending {
            self.apply(e);
        }
        if let Some(result) = finished {
            match result {
                Ok(s) => self.note(format!(
                    "stored as scan #{}: {} added, {} changed, {} unchanged, {} reappeared",
                    s.seq, s.added, s.changed, s.unchanged, s.reappeared
                )),
                Err(e) => self.note(format!("scan failed: {e}")),
            }
            self.refresh()?;
            self.tab = Tab::Overview;
        }
        Ok(())
    }

    fn event_loop(
        &mut self,
        terminal: &mut DefaultTerminal,
    ) -> Result<(), TuiError> {
        loop {
            self.poll()?;
            terminal.draw(|frame| ui::draw(self, frame))?;
            if !event::poll(Duration::from_millis(100))? {
                continue;
            }
            let key = match event::read()? {
                Event::Key(key) => key,
                Event::Mouse(m) => {
                    if m.kind == MouseEventKind::Down(MouseButton::Left) {
                        self.click_workload_row(m.column, m.row)?;
                    }
                    continue;
                }
                _ => continue,
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if matches!(self.screen, Screen::Picker(_)) {
                if self.picker_key(key.code)? {
                    return Ok(());
                }
                continue;
            }
            // Popups capture every key while open, most recent first.
            if let Some(m) = self.action_menu.as_mut() {
                match key.code {
                    KeyCode::Esc => {
                        if m.confirming.take().is_none() {
                            self.action_menu = None;
                        }
                    }
                    KeyCode::Down => {
                        let i = m.state.selected().unwrap_or(0);
                        m.state.select(Some((i + 1).min(m.actions.len() - 1)));
                        m.confirming = None;
                    }
                    KeyCode::Up => {
                        let i = m.state.selected().unwrap_or(0);
                        m.state.select(Some(i.saturating_sub(1)));
                        m.confirming = None;
                    }
                    KeyCode::Enter => self.action_menu_enter()?,
                    _ => {}
                }
                continue;
            }
            if let Some(p) = self.merge_prompt.as_mut() {
                match key.code {
                    KeyCode::Esc => self.merge_prompt = None,
                    KeyCode::Backspace => {
                        p.text.pop();
                    }
                    KeyCode::Char(c) => p.text.push(c),
                    KeyCode::Enter => self.apply_merge()?,
                    _ => {}
                }
                continue;
            }
            if let Some(p) = self.holiday_picker.as_mut() {
                match key.code {
                    KeyCode::Esc => self.holiday_picker = None,
                    KeyCode::Down => {
                        let i = p.state.selected().unwrap_or(0);
                        p.state.select(Some((i + 1).min(p.holidays.len() - 1)));
                    }
                    KeyCode::Up => {
                        let i = p.state.selected().unwrap_or(0);
                        p.state.select(Some(i.saturating_sub(1)));
                    }
                    KeyCode::Char(' ') => {
                        if let Some(i) = p.state.selected() {
                            p.chosen[i] = !p.chosen[i];
                        }
                    }
                    KeyCode::Enter => self.apply_holidays()?,
                    _ => {}
                }
                continue;
            }
            if let Some(p) = self.depends_picker.as_mut() {
                match key.code {
                    KeyCode::Esc => self.depends_picker = None,
                    KeyCode::Down => {
                        let i = p.state.selected().unwrap_or(0);
                        p.state
                            .select(Some((i + 1).min(p.candidates.len() - 1)));
                    }
                    KeyCode::Up => {
                        let i = p.state.selected().unwrap_or(0);
                        p.state.select(Some(i.saturating_sub(1)));
                    }
                    KeyCode::Enter => self.apply_dependency()?,
                    _ => {}
                }
                continue;
            }
            if let Some(p) = self.weekday_picker.as_mut() {
                match key.code {
                    KeyCode::Esc | KeyCode::Enter => {
                        self.close_weekday_picker()?;
                    }
                    KeyCode::Down => {
                        let i = p.state.selected().unwrap_or(0);
                        p.state
                            .select(Some((i + 1).min(WEEKDAY_ROWS.len() - 1)));
                    }
                    KeyCode::Up => {
                        let i = p.state.selected().unwrap_or(0);
                        p.state.select(Some(i.saturating_sub(1)));
                    }
                    KeyCode::Char(' ') => self.toggle_weekday_row(),
                    _ => {}
                }
                continue;
            }
            if let Some(ed) = self.pair_editor.as_mut() {
                if let Some(fields) = ed.input.as_mut() {
                    match key.code {
                        KeyCode::Esc => ed.input = None,
                        KeyCode::Tab | KeyCode::Down => fields.next(),
                        KeyCode::BackTab | KeyCode::Up => fields.prev(),
                        KeyCode::Backspace => fields.pop(),
                        KeyCode::Char(c) => fields.push(c),
                        KeyCode::Enter => self.pair_enter()?,
                        _ => {}
                    }
                    continue;
                }
                match key.code {
                    KeyCode::Esc => {
                        if ed.confirming_delete {
                            ed.confirming_delete = false;
                        } else {
                            self.pair_editor = None;
                        }
                    }
                    KeyCode::Down => {
                        ed.step(1);
                        ed.confirming_delete = false;
                    }
                    KeyCode::Up => {
                        ed.step(-1);
                        ed.confirming_delete = false;
                    }
                    KeyCode::Left => self.pair_adjust(-1)?,
                    KeyCode::Right => self.pair_adjust(1)?,
                    KeyCode::Enter => self.pair_enter()?,
                    _ => {}
                }
                continue;
            }
            if let Some(ed) = self.rules_editor.as_mut() {
                if let Some(fields) = ed.input.as_mut() {
                    match key.code {
                        KeyCode::Esc => ed.input = None,
                        KeyCode::Tab | KeyCode::Down => fields.next(),
                        KeyCode::BackTab | KeyCode::Up => fields.prev(),
                        KeyCode::Backspace => fields.pop(),
                        KeyCode::Char(c) => fields.push(c),
                        KeyCode::Enter => self.rules_enter()?,
                        _ => {}
                    }
                    continue;
                }
                match key.code {
                    KeyCode::Esc => {
                        if ed.confirming_delete {
                            ed.confirming_delete = false;
                        } else {
                            self.rules_editor = None;
                        }
                    }
                    KeyCode::Down => {
                        ed.step(1);
                        ed.confirming_delete = false;
                    }
                    KeyCode::Up => {
                        ed.step(-1);
                        ed.confirming_delete = false;
                    }
                    KeyCode::Left => self.rules_adjust(-1)?,
                    KeyCode::Right => self.rules_adjust(1)?,
                    KeyCode::Enter => self.rules_enter()?,
                    KeyCode::Backspace | KeyCode::Delete => {
                        self.rules_remove_wave()?;
                    }
                    _ => {}
                }
                continue;
            }
            if let Some(h) = self.history.as_mut() {
                match key.code {
                    KeyCode::Esc | KeyCode::Enter => self.history = None,
                    KeyCode::Down => h.state.select_next(),
                    KeyCode::Up => h.state.select_previous(),
                    _ => {}
                }
                continue;
            }
            // The date picker captures everything while open.
            if let Some(p) = self.date_picker.as_mut() {
                match key.code {
                    KeyCode::Esc => self.date_picker = None,
                    KeyCode::Down => p.state.select_next(),
                    KeyCode::Up => p.state.select_previous(),
                    KeyCode::Enter => {
                        let chosen = p
                            .state
                            .selected()
                            .and_then(|i| p.dates.get(i).copied());
                        let (w, e) =
                            (p.workload.clone(), p.environment.clone());
                        self.date_picker = None;
                        if let Some(d) = chosen {
                            self.change_status(
                                &w,
                                &e,
                                precip_engine::schedule::Status::Confirmed,
                                Some(d),
                            )?;
                        }
                    }
                    _ => {}
                }
                continue;
            }
            // The picker captures everything while open.
            if let Some(p) = self.picker.as_mut() {
                match key.code {
                    KeyCode::Esc => self.picker = None,
                    KeyCode::Enter => self.apply_picker()?,
                    KeyCode::Down => {
                        let i = p.state.selected().unwrap_or(0);
                        p.state
                            .select(Some((i + 1).min(DISPOSITIONS.len() - 1)));
                    }
                    KeyCode::Up => {
                        let i = p.state.selected().unwrap_or(0);
                        p.state.select(Some(i.saturating_sub(1)));
                    }
                    _ => {}
                }
                continue;
            }
            // An open workload captures Esc, Enter and the row keys.
            if let Some(x) = self.expanded.as_mut() {
                match key.code {
                    KeyCode::Esc | KeyCode::Backspace => self.expanded = None,
                    KeyCode::Enter => self.open_workload_menu(),
                    KeyCode::Down => x.selected.select_next(),
                    KeyCode::Up => x.selected.select_previous(),
                    KeyCode::Char('q') => return Ok(()),
                    _ => {}
                }
                continue;
            }
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('s') => self.start_scan(),
                KeyCode::Enter if self.tab == Tab::Workloads => {
                    self.open_workload_menu();
                }
                KeyCode::Char('1') => self.tab = Tab::Overview,
                KeyCode::Char('2') => self.tab = Tab::Workloads,
                KeyCode::Char('3') => self.tab = Tab::Sizing,
                KeyCode::Char('4') => self.tab = Tab::Schedule,
                KeyCode::Char('5') => self.tab = Tab::Scans,
                KeyCode::Char('6') => self.tab = Tab::Accounts,
                KeyCode::Enter if self.tab == Tab::Schedule => {
                    self.open_action_menu();
                }
                KeyCode::Char('r') if self.tab == Tab::Schedule => {
                    self.open_rules_editor()?;
                }
                KeyCode::Char('h') if self.tab == Tab::Schedule => {
                    self.open_history()?;
                }
                KeyCode::Char('g') if self.tab == Tab::Sizing => {
                    self.adjust_sizing(|a| {
                        a.growth_factor = (a.growth_factor + 0.1).min(5.0)
                    });
                }
                KeyCode::Char('G') if self.tab == Tab::Sizing => {
                    self.adjust_sizing(|a| {
                        a.growth_factor = (a.growth_factor - 0.1).max(1.0)
                    });
                }
                KeyCode::Char('t') if self.tab == Tab::Sizing => {
                    self.adjust_sizing(|a| {
                        a.threshold = match a.threshold {
                            x if x < 0.7 => 0.75,
                            x if x < 0.8 => 0.85,
                            x if x < 0.9 => 0.95,
                            _ => 0.6,
                        }
                    });
                }
                KeyCode::Char('n') if self.tab == Tab::Sizing => {
                    self.adjust_sizing(|a| a.headroom = (a.headroom + 1) % 3);
                }
                KeyCode::Tab | KeyCode::Right => {
                    self.tab = self.tab.next();
                }
                KeyCode::BackTab | KeyCode::Left => {
                    self.tab = self.tab.previous();
                }
                KeyCode::Down => match self.tab {
                    Tab::Workloads => self.workload_selected.select_next(),
                    Tab::Schedule => {
                        if let Some(v) = self.schedule.as_mut() {
                            v.selected.select_next();
                        }
                    }
                    _ => self.selected.select_next(),
                },
                KeyCode::Up => match self.tab {
                    Tab::Workloads => self.workload_selected.select_previous(),
                    Tab::Schedule => {
                        if let Some(v) = self.schedule.as_mut() {
                            v.selected.select_previous();
                        }
                    }
                    _ => self.selected.select_previous(),
                },
                _ => {}
            }
        }
    }

    /// Handle a key on the picker. Returns true to quit.
    fn picker_key(&mut self, code: KeyCode) -> Result<bool, TuiError> {
        let Screen::Picker(p) = &mut self.screen else { return Ok(false) };
        if let Some(name) = p.new_name.as_mut() {
            match code {
                KeyCode::Esc => p.new_name = None,
                KeyCode::Backspace => {
                    name.pop();
                }
                KeyCode::Char(c) => name.push(c),
                KeyCode::Enter => {
                    let name = name.clone();
                    if !workspace::valid_name(&name) {
                        p.error = Some(
                            "names use letters, digits, - and _ only"
                                .to_string(),
                        );
                        return Ok(false);
                    }
                    if p.entries.iter().any(|e| e.name == name) {
                        p.error = Some(format!("{name} already exists"));
                        return Ok(false);
                    }
                    p.dir.ensure()?;
                    let path = p.dir.path_for(&name);
                    self.open_workspace(&name, &path)?;
                }
                _ => {}
            }
            return Ok(false);
        }
        match code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
            KeyCode::Down => {
                let i = p.state.selected().unwrap_or(0);
                p.state.select(Some((i + 1).min(p.row_count() - 1)));
            }
            KeyCode::Up => {
                let i = p.state.selected().unwrap_or(0);
                p.state.select(Some(i.saturating_sub(1)));
            }
            KeyCode::Char('n') => p.new_name = Some(String::new()),
            KeyCode::Enter => {
                let i = p.state.selected().unwrap_or(0);
                match p.entries.get(i).cloned() {
                    Some(e) => self.open_workspace(&e.name, &e.path)?,
                    None => p.new_name = Some(String::new()),
                }
            }
            _ => {}
        }
        Ok(false)
    }
}

impl App {
    /// `name (id)` when the name is known, else the id.
    pub fn account_label(&self, id: &str) -> String {
        match self.account_names.get(id) {
            Some(name) => format!("{name} ({id})"),
            None => id.to_string(),
        }
    }
}

/// "any day", "weekdays", "weekends", or the day list.
pub fn weekday_rule_label(days: &[jiff::civil::Weekday]) -> String {
    match precip_db::weekday_preset(days) {
        Some(preset) => preset.to_string(),
        None => days
            .iter()
            .map(|d| precip_db::weekday_name(*d))
            .collect::<Vec<_>>()
            .join(","),
    }
}

/// The one sentence that says what to do now on the Schedule tab.
pub fn next_step(v: &ScheduleView) -> String {
    let s = &v.schedule;
    let m = &v.metrics;
    if !v.findings.is_empty() {
        return format!(
            "{} pair(s) marked done still have servers in AWS. Select one below and press enter.",
            v.findings.len()
        );
    }
    if !s.conflicts.is_empty() {
        return "An agreed date breaks the order of the plan. See the activity feed and agree a different date.".into();
    }
    if m.total == 0 {
        return "Nothing to schedule yet. Give workloads a disposition on the Workloads tab.".into();
    }
    if s.slots.is_empty() {
        return "Everything is migrated.".into();
    }
    let today_runs: Vec<&precip_engine::schedule::Slot> = s
        .slots
        .iter()
        .filter(|x| x.locked && x.date <= v.today && x.end >= v.today)
        .collect();
    if let Some(x) = today_runs.first() {
        return format!(
            "{} / {} runs today. When it finishes, select it, press enter and choose It's done.",
            x.workload, x.environment
        );
    }
    let open = s.slots.iter().filter(|x| !x.locked).count();
    if open > 0 {
        return format!(
            "{open} proposed date(s) need agreement. Select a row, press enter and choose Agree a date.",
        );
    }
    "All dates are agreed. Come back on each date to mark it done.".into()
}

/// A freeze from its three boxes. An empty "to" means the one day.
fn parse_freeze(
    name: &str,
    from: &str,
    to: &str,
    pair: Option<(String, String)>,
) -> Result<precip_engine::schedule::Blackout, String> {
    if name.is_empty() {
        return Err("the freeze needs a name".into());
    }
    let start: jiff::civil::Date = from
        .parse()
        .map_err(|_| format!("from: {from:?} is not a date like 2026-12-20"))?;
    let end = if to.is_empty() {
        start
    } else {
        to.parse()
            .map_err(|_| format!("to: {to:?} is not a date like 2027-01-05"))?
    };
    if start > end {
        return Err("the freeze ends before it starts".into());
    }
    Ok(precip_engine::schedule::Blackout {
        name: name.to_string(),
        start,
        end,
        pair,
    })
}

/// A date box that may be left empty to mean none.
fn parse_optional_date(
    text: &str,
) -> Result<Option<jiff::civil::Date>, String> {
    if text.is_empty() {
        return Ok(None);
    }
    text.parse()
        .map(Some)
        .map_err(|_| format!("{text:?} is not a date like 2026-12-15"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    #[test]
    fn a_freeze_with_no_end_is_one_day() {
        let b = parse_freeze("audit", "2026-11-03", "", None).unwrap();
        assert_eq!((b.start, b.end), (date(2026, 11, 3), date(2026, 11, 3)));
        let b =
            parse_freeze("close", "2026-12-20", "2027-01-05", None).unwrap();
        assert_eq!(b.end, date(2027, 1, 5));
        assert!(parse_freeze("", "2026-11-03", "", None).is_err());
        assert!(parse_freeze("x", "tomorrow", "", None).is_err());
        assert!(parse_freeze("x", "2026-11-03", "2026-11-01", None).is_err());
    }

    #[test]
    fn form_boxes_tab_around_and_edit_the_active_one() {
        let mut f = Fields::new(&["name", "from", "to"], &["x"]);
        f.push('y');
        assert_eq!(f.get(0), "xy");
        f.next();
        f.push('2');
        f.pop();
        assert_eq!(f.get(1), "");
        f.prev();
        f.prev();
        assert_eq!(f.active, 2, "wraps around");
    }
}
