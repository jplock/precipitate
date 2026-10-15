//! Migration disposition per workload, following the seven strategies in
//! AWS' large migration guide
//! (https://docs.aws.amazon.com/prescriptive-guidance/latest/large-migration-guide/migration-strategies.html).
//! Only `Rehost` and `Relocate` workloads are sized for Oxide.

crate::named_enum! {
pub enum Disposition {
    /// Decommission the workload. It is no longer needed and is not
    /// migrated anywhere.
    Retire = "retire",
    /// Leave the workload where it is for now, for example because it was
    /// recently upgraded or there is no business case to move it yet. It may
    /// be revisited in a later wave.
    Retain = "retain",
    /// Lift and shift. Move the instances to Oxide as they are, without
    /// changing the application. Counted toward sizing.
    Rehost = "rehost",
    /// Move the workload to Oxide by changing the platform underneath it
    /// without changing the application, for example by re-importing a VM
    /// image rather than rebuilding. Counted toward sizing.
    Relocate = "relocate",
    /// Replace the workload with a different product, typically a SaaS
    /// offering, instead of migrating it.
    Repurchase = "repurchase",
    /// Lift and reshape. Move the workload with some change to the platform
    /// it runs on (for example, a rebuild for x86-64, or a managed database
    /// replaced by a self-hosted one) but without changing its core
    /// architecture. Counted toward sizing.
    Replatform = "replatform",
    /// Re-architect the application before or during the move. The result
    /// on Oxide may look nothing like the AWS footprint, so it is not sized
    /// from the current inventory.
    Refactor = "refactor",
}
}

impl Disposition {
    /// Whether this workload ends up as instances on Oxide and therefore
    /// counts toward sled capacity. Replatform counts: the platform under
    /// the application changes, such as a rebuild for x86-64 or a managed
    /// database replaced by a self-hosted one, but it still lands on sleds.
    pub fn is_sized(self) -> bool {
        match self {
            Self::Rehost | Self::Relocate | Self::Replatform => true,
            Self::Retire | Self::Retain | Self::Repurchase | Self::Refactor => {
                false
            }
        }
    }

    /// Whether the workload moves at all, and so gets a migration date.
    /// Retired and retained workloads stay off the calendar.
    pub fn is_migrating(self) -> bool {
        match self {
            Self::Retire | Self::Retain => false,
            Self::Rehost
            | Self::Relocate
            | Self::Repurchase
            | Self::Replatform
            | Self::Refactor => true,
        }
    }
}
