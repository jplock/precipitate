//! Progress events emitted while a scan runs, for a progress display.

use crate::scan::TargetStatus;

#[derive(Debug, Clone)]
pub enum ScanEvent {
    /// Organization enumerated. `total` includes the caller's account.
    AccountsDiscovered {
        total: usize,
    },
    AccountStarted {
        account_id: String,
        name: Option<String>,
    },
    /// Regions chosen for one account after the Resource Explorer check.
    RegionsPlanned {
        account_id: String,
        regions: usize,
        skipped: usize,
    },
    RegionFinished {
        account_id: String,
        region: String,
        status: TargetStatus,
        resources: usize,
    },
    AccountFinished {
        account_id: String,
        resources: usize,
    },
    /// The account could not be scanned at all.
    AccountSkipped {
        account_id: String,
        reason: String,
    },
}
