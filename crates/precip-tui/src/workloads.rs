//! The Workloads tab: a pair's resources, its disposition, and folding
//! one workload into another.

use crate::{Action, App, Tab, TuiError};
use precip_db::{DispositionChange, MemberDetail, WorkloadSummary};
use precip_types::Disposition;
use ratatui::widgets::ListState;

/// Typing the workload a name folds into.
pub(crate) struct MergePrompt {
    pub alias: String,
    pub text: String,
}

/// Where each environment of `alias` lands when it is folded into
/// `into`: the environment, and whether `into` already has that pair. A
/// merge renames the workload and keeps each resource's environment.
pub(crate) fn merge_preview(
    workloads: &[WorkloadSummary],
    alias: &str,
    into: &str,
) -> Vec<(String, bool)> {
    let mut envs: Vec<&str> = workloads
        .iter()
        .filter(|w| w.workload == alias)
        .map(|w| w.environment.as_str())
        .collect();
    envs.sort_unstable();
    envs.dedup();
    envs.into_iter()
        .map(|env| {
            let exists = workloads
                .iter()
                .any(|w| w.workload == into && w.environment == env);
            (env.to_string(), exists)
        })
        .collect()
}

/// What the disposition picker sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PickerMode {
    Pair,
    /// The default for new pairs; `apply` also changes untouched pairs.
    Default {
        apply: bool,
    },
}

/// A workload and environment pair opened to show its resources.
pub(crate) struct Expanded {
    pub workload: String,
    pub environment: String,
    pub members: Vec<MemberDetail>,
    pub selected: ratatui::widgets::TableState,
    pub history: Vec<DispositionChange>,
}

/// The disposition picker, open over the Workloads tab.
pub(crate) struct DispositionPicker {
    pub workload: String,
    pub environment: String,
    pub mode: PickerMode,
    pub state: ListState,
}

pub(crate) const DISPOSITIONS: [Disposition; 7] = [
    Disposition::Rehost,
    Disposition::Relocate,
    Disposition::Replatform,
    Disposition::Refactor,
    Disposition::Repurchase,
    Disposition::Retain,
    Disposition::Retire,
];

/// What a disposition means, for the picker.
pub(crate) fn disposition_help(d: Disposition) -> &'static str {
    match d {
        Disposition::Rehost => "move the instances to Oxide as they are",
        Disposition::Relocate => "move by re-importing images, no app change",
        Disposition::Replatform => {
            "move with a platform change, same architecture"
        }
        Disposition::Refactor => "re-architect before or during the move",
        Disposition::Repurchase => "replace with a different product",
        Disposition::Retain => "leave in the cloud for now",
        Disposition::Retire => "decommission",
    }
}

impl App {
    /// Open the selected workload, or close it if already open.
    pub(crate) fn toggle_expand(&mut self) -> Result<(), TuiError> {
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

    /// The pair the user is acting on: the open one, else the selected row.
    pub(crate) fn current_pair(&self) -> Option<(String, String, Disposition)> {
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

    pub(crate) fn open_picker(&mut self, mode: PickerMode) {
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
        self.popups.open(DispositionPicker {
            workload,
            environment,
            mode,
            state,
        });
    }

    pub(crate) fn apply_picker(&mut self) -> Result<(), TuiError> {
        let Some(p) = self.popups.close::<DispositionPicker>() else {
            return Ok(());
        };
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
                    chosen.as_str()
                ));
            }
            PickerMode::Default { apply } => {
                rt.block_on(store.set_default_disposition(chosen, apply))?;
                self.note(format!(
                    "new pairs start as {}{}",
                    chosen.as_str(),
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

    pub(crate) fn click_workload_row(
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

    /// The menu for the selected workload row, plus what applies to the
    /// whole tab.
    pub(crate) fn open_workload_menu(&mut self) {
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

    pub(crate) fn regroup(&mut self) -> Result<(), TuiError> {
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

    pub(crate) fn prune(
        &mut self,
        keep_user_changed: bool,
    ) -> Result<(), TuiError> {
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

    pub(crate) fn apply_merge(&mut self) -> Result<(), TuiError> {
        let Some(p) = self.popups.close::<MergePrompt>() else { return Ok(()) };
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use precip_types::Disposition;

    fn pair(workload: &str, environment: &str) -> WorkloadSummary {
        WorkloadSummary {
            workload: workload.into(),
            environment: environment.into(),
            disposition: Disposition::Rehost,
            resources: 1,
            accounts: Vec::new(),
            regions: Vec::new(),
        }
    }

    #[test]
    fn a_merge_keeps_each_environment_and_says_which_pairs_exist() {
        let all = [
            pair("inbox", "prod"),
            pair("inbox", "dev"),
            pair("ai", "prod"),
            pair("other", "dev"),
        ];
        assert_eq!(
            merge_preview(&all, "inbox", "ai"),
            [("dev".to_string(), false), ("prod".to_string(), true)]
        );
        assert_eq!(
            merge_preview(&all, "inbox", ""),
            [("dev".to_string(), false), ("prod".to_string(), false)],
            "nothing typed yet"
        );
        assert!(merge_preview(&all, "gone", "ai").is_empty());
    }
}
