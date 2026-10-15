//! Domain types shared by the scanner, store, sizing engine and TUI.

pub mod names;
pub use names::{
    AccountId, Arn, EnvName, PairKey, Region, UnknownName, WorkloadName,
};

pub mod body;
pub mod profile;
pub mod scan;

pub use body::Body;
pub use profile::Profile;
pub use scan::{Relationship, Resource, ResourceKind, Scan, ScanTarget};

pub mod disposition;
pub use disposition::Disposition;

pub mod workload;
pub use workload::{GroupingConfig, GroupingSignal, WorkloadMember};

pub mod progress;
pub use progress::ScanEvent;
