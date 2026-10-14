//! Domain types shared by the scanner, store, sizing engine and TUI.

pub mod profile;
pub mod scan;

pub use profile::Profile;
pub use scan::{Relationship, Resource, ResourceKind, Scan, ScanTarget};

pub mod disposition;
pub use disposition::Disposition;

pub mod workload;
pub use workload::{GroupingConfig, GroupingSignal, Override, WorkloadMember};

pub mod progress;
pub use progress::ScanEvent;
