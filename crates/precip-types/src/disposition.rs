//! Migration disposition per workload, following the seven strategies in
//! AWS' large migration guide
//! (https://docs.aws.amazon.com/prescriptive-guidance/latest/large-migration-guide/migration-strategies.html).
//! Only `Rehost` and `Relocate` workloads are sized for Oxide.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    /// Decommission the workload. It is no longer needed and is not
    /// migrated anywhere.
    Retire,
    /// Leave the workload where it is for now, for example because it was
    /// recently upgraded or there is no business case to move it yet. It may
    /// be revisited in a later wave.
    Retain,
    /// Lift and shift. Move the instances to Oxide as they are, without
    /// changing the application. Counted toward sizing.
    Rehost,
    /// Move the workload to Oxide by changing the platform underneath it
    /// without changing the application, for example by re-importing a VM
    /// image rather than rebuilding. Counted toward sizing.
    Relocate,
    /// Replace the workload with a different product, typically a SaaS
    /// offering, instead of migrating it.
    Repurchase,
    /// Lift and reshape. Move the workload with some change to the platform
    /// it runs on (for example, a rebuild for x86-64, or a managed database
    /// replaced by a self-hosted one) but without changing its core
    /// architecture. Counted toward sizing.
    Replatform,
    /// Re-architect the application before or during the move. The result
    /// on Oxide may look nothing like the AWS footprint, so it is not sized
    /// from the current inventory.
    Refactor,
}

impl Disposition {
    /// Whether this workload ends up as instances on Oxide and therefore
    /// counts toward sled capacity. Replatform counts: the platform under
    /// the application changes, such as a rebuild for x86-64 or a managed
    /// database replaced by a self-hosted one, but it still lands on sleds.
    pub fn is_sized(self) -> bool {
        matches!(self, Self::Rehost | Self::Relocate | Self::Replatform)
    }
}
