//! Running a scan in the background and showing its progress.

use crate::{App, ScanHandle, Tab, TuiError};
use precip_db::ScanStats;
use precip_types::ScanEvent;
use precip_types::scan::TargetStatus;
use std::collections::BTreeMap;
use std::time::Instant;

#[derive(Debug, Default, Clone)]
pub(crate) struct AccountProgress {
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
    pub(crate) fn finished(&self) -> bool {
        self.note.is_some()
            || (self.planned + self.skipped > 0 && self.done >= self.planned)
    }

    pub(crate) fn state(&self) -> String {
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

pub(crate) struct Progress {
    handle: ScanHandle,
    pub started: Instant,
    pub accounts_total: usize,
    pub accounts: BTreeMap<precip_types::AccountId, AccountProgress>,
    pub outcome: Option<Result<ScanStats, String>>,
}

impl Progress {
    pub(crate) fn regions_planned(&self) -> usize {
        self.accounts.values().map(|a| a.planned).sum()
    }

    pub(crate) fn regions_done(&self) -> usize {
        self.accounts.values().map(|a| a.done).sum()
    }

    pub(crate) fn accounts_done(&self) -> usize {
        self.accounts.values().filter(|a| a.finished()).count()
    }

    pub(crate) fn running(&self) -> bool {
        self.outcome.is_none()
    }
}

impl App {
    pub(crate) fn start_scan(&mut self) {
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

    pub(crate) fn apply(&mut self, e: ScanEvent) {
        let Some(p) = self.progress.as_mut() else { return };
        let line = match &e {
            ScanEvent::AccountsDiscovered { total } => {
                p.accounts_total = *total;
                format!("organization enumerated: {total} accounts to scan")
            }
            ScanEvent::AccountStarted { account_id, name } => {
                let a = p.accounts.entry(account_id.clone()).or_default();
                a.name = name.clone();
                let label = name.as_deref().unwrap_or(account_id.as_str());
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
    pub(crate) fn poll(&mut self) -> Result<(), TuiError> {
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
}
