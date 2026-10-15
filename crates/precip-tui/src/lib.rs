//! ratatui application: header with context, tabs for overview, scans and
//! accounts, an activity feed, and a progress gauge while a scan runs. The
//! scan itself runs on the tokio runtime; the interface polls its progress
//! channel between frames.

mod editors;
mod logo;
mod menu;
mod popup;
mod scan;
mod schedule;
mod sizing;
mod ui;
mod workloads;
pub mod workspace;

use camino::Utf8PathBuf;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode,
    KeyEventKind, MouseButton, MouseEventKind,
};
use precip_db::{
    Overview, ScanStats, ScanSummary, Store, Trigger, WorkloadSummary,
};
use precip_engine::sizing::Sizing;
use precip_engine::sizing::profiles::BuiltinProfile;
use precip_types::ScanEvent;
use ratatui::DefaultTerminal;
use ratatui::widgets::ListState;
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};
use workspace::{WorkspaceDir, WorkspaceEntry};

pub(crate) use editors::{
    DependsPicker, Fields, HolidayPicker, PairEditor, PairRow, RuleRow,
    RulesEditor, WEEKDAY_ROWS, WeekdayPicker,
};
pub(crate) use menu::{Action, ActionMenu, ExportKind, ExportPrompt};
pub(crate) use scan::Progress;
pub(crate) use schedule::{
    DatePicker, HistoryView, Pending, ReasonPicker, ScheduleView, next_step,
};
pub(crate) use schedule::{parse_freeze, parse_optional_date};
pub(crate) use sizing::SizingView;
pub(crate) use workloads::{
    DISPOSITIONS, DispositionPicker, Expanded, MergePrompt, PickerMode,
    disposition_help,
};

#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    #[error("terminal I/O")]
    Io(#[from] std::io::Error),
    #[error("database")]
    Db(#[from] precip_db::DbError),
    #[error("export")]
    Export(#[from] serde_json::Error),
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

/// One line in the activity feed.
#[derive(Debug, Clone)]
pub struct Activity {
    pub at: Instant,
    pub text: String,
}

pub struct App {
    pub(crate) screen: Screen,
    store: Option<Store>,
    pub(crate) context: Context,
    pub(crate) scans: Vec<ScanSummary>,
    pub(crate) overview: Option<Overview>,
    pub(crate) workloads: Vec<WorkloadSummary>,
    /// Account id to name, from the latest scan.
    pub(crate) account_names: BTreeMap<String, String>,
    pub(crate) workload_selected: ratatui::widgets::TableState,
    /// The expanded pair's resources, when a workload is open.
    pub(crate) expanded: Option<Expanded>,
    /// Popups open over the tab, bottom first.
    pub(crate) popups: popup::Popups,
    pub(crate) sizing: Option<SizingView>,
    pub(crate) schedule: Option<ScheduleView>,
    /// Screen area of the workload table's rows, for mouse clicks.
    pub(crate) workload_rows_area: ratatui::layout::Rect,
    pub(crate) selected: ListState,
    pub(crate) tab: Tab,
    launcher: Launcher,
    pub(crate) progress: Option<Progress>,
    pub(crate) activity: VecDeque<Activity>,
    pub(crate) opened: Instant,
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
            popups: popup::Popups::default(),
            sizing: None,
            schedule: None,
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

    pub(crate) fn note(&mut self, text: impl Into<String>) {
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
        // The remembered profile, or the built-in when it is not one any
        // more.
        let builtin = rt
            .block_on(store.sizing_profile())?
            .and_then(|n| n.parse::<BuiltinProfile>().ok())
            .unwrap_or(BuiltinProfile::DEFAULT);
        self.sizing = match self.scans.first() {
            Some(s) => match builtin.profile().ok() {
                Some(profile) => {
                    let instances =
                        rt.block_on(store.sized_instances(&s.id, &profile))?;
                    let result = Sizing::new(&profile, &[], &assumptions);
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
            },
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
        self.recompute_schedule(Trigger::Open)?;
        Ok(())
    }

    /// A session left open past midnight computes the new day's
    /// schedule, so expired confirmations and today's row stay right.
    /// Up and down move the selection of the tab on screen. Tabs
    /// without a list ignore them.
    fn move_selection(&mut self, delta: i8) {
        let down = delta > 0;
        match self.tab {
            Tab::Workloads if down => self.workload_selected.select_next(),
            Tab::Workloads => self.workload_selected.select_previous(),
            Tab::Scans if down => self.selected.select_next(),
            Tab::Scans => self.selected.select_previous(),
            Tab::Schedule => {
                if let Some(v) = self.schedule.as_mut() {
                    if down {
                        v.selected.select_next();
                    } else {
                        v.selected.select_previous();
                    }
                }
            }
            Tab::Overview | Tab::Sizing | Tab::Accounts => {}
        }
    }

    fn refresh_on_new_day(&mut self) -> Result<(), TuiError> {
        let today = jiff::Zoned::now().date();
        if self.schedule.as_ref().is_some_and(|v| v.today != today) {
            self.note(format!("new day, {today}: schedule recomputed"));
            self.recompute_schedule(Trigger::Open)?;
        }
        Ok(())
    }

    /// Act on what a key did to the popup on top.
    fn popup_outcome(
        &mut self,
        tag: popup::PopupTag,
        outcome: popup::Outcome,
    ) -> Result<(), TuiError> {
        use popup::{Outcome, PopupTag};
        match outcome {
            Outcome::Stay => {}
            Outcome::Close => self.popups.pop(),
            Outcome::Submit => match tag {
                PopupTag::Disposition => self.apply_picker()?,
                PopupTag::Date => self.date_enter()?,
                PopupTag::History => self.popups.pop(),
                PopupTag::Rules => self.rules_enter()?,
                PopupTag::Pair => self.pair_enter()?,
                PopupTag::Weekdays => self.close_weekday_picker()?,
                PopupTag::Depends => self.apply_dependency()?,
                PopupTag::Holidays => self.apply_holidays()?,
                PopupTag::Merge => self.apply_merge()?,
                PopupTag::Reason => self.reason_enter()?,
                PopupTag::Export => self.apply_export()?,
                PopupTag::Actions => self.action_menu_enter()?,
            },
            Outcome::Adjust(d) if tag == PopupTag::Rules => {
                self.rules_adjust(d)?;
            }
            Outcome::Adjust(d) if tag == PopupTag::Pair => {
                self.pair_adjust(d)?
            }
            Outcome::Remove if tag == PopupTag::Rules => {
                self.rules_remove_wave()?;
            }
            Outcome::Adjust(_) | Outcome::Remove => {}
        }
        Ok(())
    }

    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "each prompt handles a few keys and ignores the rest of KeyCode"
    )]
    fn event_loop(
        &mut self,
        terminal: &mut DefaultTerminal,
    ) -> Result<(), TuiError> {
        loop {
            self.poll()?;
            terminal.draw(|frame| ui::draw(self, frame))?;
            if !event::poll(Duration::from_millis(100))? {
                self.refresh_on_new_day()?;
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
                Event::FocusGained
                | Event::FocusLost
                | Event::Paste(_)
                | Event::Resize(..) => continue,
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
            // The popup on top takes every key while popups are open.
            if let Some(top) = self.popups.top_mut() {
                let tag = top.tag();
                let outcome = top.key(key.code);
                self.popup_outcome(tag, outcome)?;
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
                KeyCode::Char('e') if self.tab == Tab::Schedule => {
                    self.open_export(ExportKind::Schedule);
                }
                KeyCode::Char('g') if self.tab == Tab::Schedule => {
                    if let Some(v) = self.schedule.as_mut() {
                        v.gantt = !v.gantt;
                    }
                }
                KeyCode::Char('e') if self.tab == Tab::Workloads => {
                    self.open_export(ExportKind::Workloads);
                }
                KeyCode::Char('e') if self.tab == Tab::Sizing => {
                    self.open_export(ExportKind::Sizing);
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
                KeyCode::Char('p') if self.tab == Tab::Sizing => {
                    self.next_profile()?;
                }
                KeyCode::Tab | KeyCode::Right => {
                    self.tab = self.tab.next();
                }
                KeyCode::BackTab | KeyCode::Left => {
                    self.tab = self.tab.previous();
                }
                KeyCode::Down => self.move_selection(1),
                KeyCode::Up => self.move_selection(-1),
                _ => {}
            }
        }
    }

    /// Handle a key on the picker. Returns true to quit.
    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "the picker handles a few keys and ignores the rest of KeyCode"
    )]
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

    /// `name (id)` when the name is known, else the id.
    pub(crate) fn account_label(&self, id: &str) -> String {
        match self.account_names.get(id) {
            Some(name) => format!("{name} ({id})"),
            None => id.to_string(),
        }
    }
}
