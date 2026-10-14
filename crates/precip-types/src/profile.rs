//! An Oxide rack profile: sled specifications and per-instance limits for
//! one hardware generation and Oxide release. Profiles are TOML files.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    /// The Oxide release these limits were checked against.
    pub oxide_release: String,
    pub sleds_per_rack: u32,
    pub sled: Sled,
    pub instance: InstanceLimits,
    pub storage: Storage,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sled {
    pub cores: u32,
    pub threads_per_core: u32,
    pub dram_gib: u64,
    /// Memory held back for the control plane before the reservoir
    /// percentage is applied.
    pub reservation_gib: u64,
    /// Share of the remaining DRAM available to guests.
    pub reservoir_pct: f64,
    pub nvme_bays: u32,
    pub bay_size_tib: f64,
    /// Usable guest memory per sled as published by Oxide. When set it is
    /// used as is, instead of being derived from DRAM, the reservation
    /// and the reservoir.
    #[serde(default)]
    pub usable_memory_gib: Option<u64>,
    /// Usable disk capacity per sled as published by Oxide, already net
    /// of replication. When set it replaces bays, bay size and the
    /// replication factor.
    #[serde(default)]
    pub usable_storage_tib: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceLimits {
    pub max_vcpu: u32,
    pub max_memory_gib: u64,
    pub max_disks: u32,
    pub max_disk_gib: u64,
    pub max_nics: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Storage {
    pub replication_factor: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("profile is not valid TOML")]
    Parse(#[from] toml::de::Error),
}

impl Profile {
    pub fn from_toml(text: &str) -> Result<Self, ProfileError> {
        Ok(toml::from_str(text)?)
    }
}
