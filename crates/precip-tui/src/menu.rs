//! Row menus of actions, and exports.

use crate::{App, MergePrompt, Pending, PickerMode, TuiError};
use precip_db::export;
use precip_engine::schedule::Status;
use ratatui::widgets::ListState;

/// One thing the user can do from a row menu. Only the actions that make
/// sense for the row are offered, and the ones that change more than a
/// single pair, or record an outcome, ask once more before they act.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
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
    pub(crate) fn label(self) -> &'static str {
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

    pub(crate) fn help(self) -> &'static str {
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
    pub(crate) fn needs_confirmation(self) -> bool {
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

pub(crate) struct ActionMenu {
    pub title: String,
    pub workload: String,
    pub environment: String,
    pub actions: Vec<Action>,
    pub state: ListState,
    /// The action waiting for a second Enter.
    pub confirming: Option<Action>,
}

/// What an export writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExportKind {
    Workloads,
    Sizing,
    Schedule,
}

/// Typing the path an export goes to. The extension picks the format.
pub(crate) struct ExportPrompt {
    pub kind: ExportKind,
    pub text: String,
    /// The path exists; a second Enter overwrites it.
    pub confirming_overwrite: bool,
}

impl App {
    /// Ask where to write an export of `kind`, offering a file name in
    /// its usual format.
    pub(crate) fn open_export(&mut self, kind: ExportKind) {
        let (what, ext) = match kind {
            ExportKind::Workloads => ("workloads", "csv"),
            ExportKind::Sizing => ("sizing", "csv"),
            ExportKind::Schedule => ("schedule", "ics"),
        };
        self.popups.open(ExportPrompt {
            kind,
            text: format!("precip-{}-{what}.{ext}", self.context.workspace),
            confirming_overwrite: false,
        });
    }

    /// Enter in the export prompt: write the file, after a second Enter
    /// when it already exists.
    pub(crate) fn apply_export(&mut self) -> Result<(), TuiError> {
        let Some(p) = self.popups.get_mut::<ExportPrompt>() else {
            return Ok(());
        };
        let path = camino::Utf8PathBuf::from(p.text.trim());
        if path.as_str().is_empty() {
            self.note("type a file name ending in .csv, .json or .ics");
            return Ok(());
        }
        if path.exists() && !p.confirming_overwrite {
            p.confirming_overwrite = true;
            return Ok(());
        }
        let kind = p.kind;
        self.popups.close::<ExportPrompt>();
        let ext = path.extension().unwrap_or_default().to_lowercase();
        let text = match (kind, ext.as_str()) {
            (ExportKind::Workloads, "csv") => {
                export::workloads_csv(&self.workloads)
            }
            (ExportKind::Workloads, "json") => {
                export::workloads_json(&self.workloads)?
            }
            (ExportKind::Sizing, "csv") => match &self.sizing {
                Some(v) => export::sizing_csv(&v.instances),
                None => return Ok(()),
            },
            (ExportKind::Sizing, "json") => match &self.sizing {
                Some(v) => export::sizing_json(
                    &v.profile,
                    &v.assumptions,
                    &v.result,
                    &v.demand,
                    &v.instances,
                )?,
                None => return Ok(()),
            },
            (ExportKind::Schedule, "csv") => match &self.schedule {
                Some(v) => export::schedule_csv(&v.schedule),
                None => return Ok(()),
            },
            (ExportKind::Schedule, "json") => match &self.schedule {
                Some(v) => export::schedule_json(&v.schedule)?,
                None => return Ok(()),
            },
            (ExportKind::Schedule, "ics") => match &self.schedule {
                Some(v) => export::schedule_ics(
                    &v.schedule,
                    &self.context.workspace,
                    jiff::Timestamp::now(),
                ),
                None => return Ok(()),
            },
            (_, other) => {
                self.note(format!(
                    ".{other} is not a format this export knows; use .csv, .json or .ics"
                ));
                return Ok(());
            }
        };
        match std::fs::write(&path, text) {
            Ok(()) => self.note(format!("wrote {path}")),
            Err(e) => self.note(format!("could not write {path}: {e}")),
        }
        Ok(())
    }

    pub(crate) fn show_menu(
        &mut self,
        title: String,
        workload: String,
        environment: String,
        actions: Vec<Action>,
    ) {
        let mut state = ListState::default();
        state.select(Some(0));
        self.popups.open(ActionMenu {
            title,
            workload,
            environment,
            actions,
            state,
            confirming: None,
        });
    }

    /// Enter in the action menu: run the action, or ask first.
    pub(crate) fn action_menu_enter(&mut self) -> Result<(), TuiError> {
        let Some(m) = self.popups.get_mut::<ActionMenu>() else {
            return Ok(());
        };
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
        self.popups.close::<ActionMenu>();
        match action {
            Action::AgreeDate | Action::ChangeDate => {
                let waiting = match &self.store {
                    Some(store) => tokio::runtime::Handle::current()
                        .block_on(store.unfinished_dependencies(&w, &e))?,
                    None => Vec::new(),
                };
                if let Some(dep) = waiting.first() {
                    self.note(format!(
                        "{w} / {e} waits for {dep}, which has not migrated; agree that date first"
                    ));
                } else {
                    self.open_date_picker();
                }
            }
            Action::Done => {
                self.change_status(&w, &e, Status::Migrated, None, None)?
            }
            Action::DidNotHappen => {
                self.ask_reason(Pending::Missed { workload: w, environment: e })
            }
            Action::RolledBack => {
                self.change_status(&w, &e, Status::RolledBack, None, None)?
            }
            Action::ShowResources => {
                if self.expanded.is_none() {
                    self.toggle_expand()?;
                }
            }
            Action::ChangeDisposition => self.open_picker(PickerMode::Pair),
            Action::PairSettings => self.open_pair_editor(&w, &e)?,
            Action::MergeInto => {
                self.popups.open(MergePrompt { alias: w, text: String::new() });
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
}
