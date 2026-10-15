//! Progress events emitted while a scan runs, for a progress display.

use crate::scan::TargetStatus;
use crate::{AccountId, Region};

#[derive(Debug, Clone)]
pub enum ScanEvent {
    /// Organization enumerated. `total` includes the caller's account.
    AccountsDiscovered {
        total: usize,
    },
    AccountStarted {
        account_id: AccountId,
        name: Option<String>,
    },
    /// Regions chosen for one account after the Resource Explorer check.
    RegionsPlanned {
        account_id: AccountId,
        regions: usize,
        skipped: usize,
    },
    RegionFinished {
        account_id: AccountId,
        region: Region,
        status: TargetStatus,
        resources: usize,
    },
    AccountFinished {
        account_id: AccountId,
        resources: usize,
    },
    /// The account could not be scanned at all.
    AccountSkipped {
        account_id: AccountId,
        reason: String,
    },
}
