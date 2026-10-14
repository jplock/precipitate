//! The sizing arithmetic from RFD 0001, section "Sizing". Pure functions
//! over an inventory and a profile so the whole thing is unit-testable.

pub mod inputs;
pub mod profiles;

use precip_types::{Disposition, Profile};
use serde::{Deserialize, Serialize};

/// Whether an instance can run on Oxide as-is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fit {
    CleanFit,
    NeedsRemediation,
    CannotMove,
}

/// Demand for one instance, already multiplied out for ASG capacity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instance {
    pub id: String,
    pub disposition: Disposition,
    pub vcpus: u32,
    pub memory_gib: u64,
    pub disk_gib: u64,
    pub fit: Fit,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Assumptions {
    /// Multiplier on demand. 1.0 means size for today.
    pub growth_factor: f64,
    /// Share of sled capacity the packer may fill. RFD 70 default is 0.75.
    pub threshold: f64,
    /// Extra sleds per rack for failure and maintenance. 1 means N+1.
    pub headroom: u32,
}

impl Default for Assumptions {
    fn default() -> Self {
        Self { growth_factor: 1.0, threshold: 0.75, headroom: 1 }
    }
}

/// Which resource set the sled count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Bound {
    Compute,
    Storage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sizing {
    pub compute_sleds: u32,
    pub storage_sleds: u32,
    pub bound: Bound,
    pub sleds: u32,
    pub racks: u32,
    pub spare_sleds: u32,
    /// Instances whose workload disposition is not rehost or relocate.
    pub not_sized: Vec<String>,
    /// Instances in scope whose fit is "cannot move".
    pub excluded: Vec<String>,
    pub sled_capacity: SledCapacity,
}

/// Usable capacity of one sled after the threshold is applied.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SledCapacity {
    pub vcpus: f64,
    pub memory_gib: f64,
    pub disk_tib: f64,
}

pub fn sled_capacity(profile: &Profile, a: &Assumptions) -> SledCapacity {
    let s = &profile.sled;
    let vcpus = f64::from(s.cores * s.threads_per_core) * a.threshold;
    let usable_dram = s.dram_gib.saturating_sub(s.reservation_gib) as f64;
    let memory_gib = usable_dram * s.reservoir_pct * a.threshold;
    let disk_tib = f64::from(s.nvme_bays) * s.bay_size_tib * a.threshold;
    SledCapacity { vcpus, memory_gib, disk_tib }
}

struct Bin {
    vcpus: f64,
    memory_gib: f64,
}

/// First-fit decreasing by memory, then vCPU.
fn pack_compute(demands: &[(f64, f64)], cap: &SledCapacity) -> u32 {
    let mut sorted: Vec<(f64, f64)> = demands.to_vec();
    sorted
        .sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| b.0.total_cmp(&a.0)));
    let mut bins: Vec<Bin> = Vec::new();
    for (vcpus, mem) in sorted {
        let slot = bins.iter_mut().find(|b| {
            b.vcpus + vcpus <= cap.vcpus && b.memory_gib + mem <= cap.memory_gib
        });
        match slot {
            Some(b) => {
                b.vcpus += vcpus;
                b.memory_gib += mem;
            }
            None => bins.push(Bin { vcpus, memory_gib: mem }),
        }
    }
    u32::try_from(bins.len()).unwrap_or(u32::MAX)
}

pub fn size(
    profile: &Profile,
    instances: &[Instance],
    a: &Assumptions,
) -> Sizing {
    let cap = sled_capacity(profile, a);

    let mut not_sized = Vec::new();
    let mut excluded = Vec::new();
    let mut demands = Vec::new();
    let mut total_disk_gib = 0.0;
    for i in instances {
        if !i.disposition.is_sized() {
            not_sized.push(i.id.clone());
            continue;
        }
        let vcpus = f64::from(i.vcpus) * a.growth_factor;
        let memory_gib = i.memory_gib as f64 * a.growth_factor;
        // An instance that does not fit an empty sled cannot be placed
        // anywhere; treat it like any other "cannot move" instance rather
        // than opening a sled it would overflow.
        let oversized = vcpus > cap.vcpus || memory_gib > cap.memory_gib;
        if i.fit == Fit::CannotMove || oversized {
            excluded.push(i.id.clone());
            continue;
        }
        demands.push((vcpus, memory_gib));
        total_disk_gib += i.disk_gib as f64 * a.growth_factor;
    }

    let compute_sleds = pack_compute(&demands, &cap);

    let storage_demand_tib =
        total_disk_gib / 1024.0 * f64::from(profile.storage.replication_factor);
    let storage_sleds = if storage_demand_tib == 0.0 {
        0
    } else {
        (storage_demand_tib / cap.disk_tib).ceil() as u32
    };

    let (bound, mut sleds) = if storage_sleds > compute_sleds {
        (Bound::Storage, storage_sleds)
    } else {
        (Bound::Compute, compute_sleds)
    };

    let racks_before_headroom = sleds.div_ceil(profile.sleds_per_rack).max(1);
    sleds += a.headroom * racks_before_headroom;
    let racks = sleds.div_ceil(profile.sleds_per_rack);
    let spare_sleds = racks * profile.sleds_per_rack - sleds;

    Sizing {
        compute_sleds,
        storage_sleds,
        bound,
        sleds,
        racks,
        spare_sleds,
        not_sized,
        excluded,
        sled_capacity: cap,
    }
}

/// Demand per workload and environment, for the sizing table.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkloadDemand {
    pub workload: String,
    pub environment: String,
    pub instances: usize,
    pub vcpus: u64,
    pub memory_gib: u64,
    pub disk_gib: u64,
    pub clean: usize,
    pub remediate: usize,
    pub blocked: usize,
}

pub fn demand_by_workload(
    instances: &[inputs::SizedInstance],
) -> Vec<WorkloadDemand> {
    let mut map: std::collections::BTreeMap<(String, String), WorkloadDemand> =
        std::collections::BTreeMap::new();
    for i in instances {
        let d = map
            .entry((i.workload.clone(), i.environment.clone()))
            .or_insert_with(|| WorkloadDemand {
                workload: i.workload.clone(),
                environment: i.environment.clone(),
                ..Default::default()
            });
        d.instances += 1;
        d.vcpus += u64::from(i.vcpus);
        d.memory_gib += i.memory_gib;
        d.disk_gib += i.disk_gib;
        match i.fit {
            Fit::CleanFit => d.clean += 1,
            Fit::NeedsRemediation => d.remediate += 1,
            Fit::CannotMove => d.blocked += 1,
        }
    }
    map.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gen2() -> Profile {
        profiles::builtin("gen2-1152").expect("listed").expect("parses")
    }

    fn inst(id: &str, vcpus: u32, mem: u64, disk: u64) -> Instance {
        Instance {
            id: id.to_string(),
            disposition: Disposition::Rehost,
            vcpus,
            memory_gib: mem,
            disk_gib: disk,
            fit: Fit::CleanFit,
        }
    }

    #[test]
    fn sled_capacity_applies_reservation_reservoir_and_threshold() {
        let cap = sled_capacity(&gen2(), &Assumptions::default());
        assert_eq!(cap.vcpus, 288.0);
        let expected_mem = (1152.0 - 44.0) * 0.91 * 0.75;
        assert!((cap.memory_gib - expected_mem).abs() < 1e-9);
    }

    #[test]
    fn empty_inventory_is_one_headroom_sled_in_one_rack() {
        let s = size(&gen2(), &[], &Assumptions::default());
        assert_eq!(s.compute_sleds, 0);
        assert_eq!(s.sleds, 1);
        assert_eq!(s.racks, 1);
    }

    #[test]
    fn cannot_move_instances_are_excluded_and_listed() {
        let mut blocked = inst("arm", 4, 16, 100);
        blocked.fit = Fit::CannotMove;
        let s = size(&gen2(), &[blocked], &Assumptions::default());
        assert_eq!(s.excluded, vec!["arm".to_string()]);
        assert_eq!(s.compute_sleds, 0);
    }

    #[test]
    fn retired_workloads_are_not_sized() {
        let mut retired = inst("old", 64, 512, 1000);
        retired.disposition = Disposition::Retire;
        let s = size(&gen2(), &[retired], &Assumptions::default());
        assert_eq!(s.not_sized, vec!["old".to_string()]);
        assert_eq!(s.compute_sleds, 0);
    }

    #[test]
    fn replatform_workloads_are_sized() {
        let mut re = inst("re", 4, 16, 100);
        re.disposition = Disposition::Replatform;
        let s = size(&gen2(), &[re], &Assumptions::default());
        assert!(s.not_sized.is_empty());
        assert_eq!(s.compute_sleds, 1);
    }

    #[test]
    fn memory_heavy_fleet_is_compute_bound_by_memory() {
        // 20 r-class style instances: 16 vCPU, 128 GiB each. One sled holds
        // ~756 GiB after threshold, so 5 per sled, 4 sleds.
        let fleet: Vec<Instance> =
            (0..20).map(|i| inst(&format!("r{i}"), 16, 128, 100)).collect();
        let s = size(&gen2(), &fleet, &Assumptions::default());
        assert_eq!(s.compute_sleds, 4);
        assert_eq!(s.bound, Bound::Compute);
        assert_eq!(s.sleds, 5);
        assert_eq!(s.racks, 1);
    }

    #[test]
    fn large_disks_make_storage_bind() {
        let fleet: Vec<Instance> =
            (0..40).map(|i| inst(&format!("d{i}"), 2, 8, 10_000)).collect();
        let s = size(&gen2(), &fleet, &Assumptions::default());
        assert_eq!(s.bound, Bound::Storage);
        assert!(s.storage_sleds > s.compute_sleds);
    }

    #[test]
    fn growth_factor_scales_demand() {
        // Two 300 GiB instances share one sled (~756 GiB usable); doubled,
        // each needs its own.
        let fleet = vec![inst("a", 50, 300, 0), inst("b", 50, 300, 0)];
        let base = size(&gen2(), &fleet, &Assumptions::default());
        let grown = size(
            &gen2(),
            &fleet,
            &Assumptions { growth_factor: 2.0, ..Assumptions::default() },
        );
        assert_eq!(base.compute_sleds, 1);
        assert_eq!(grown.compute_sleds, 2);
    }

    #[test]
    fn instance_larger_than_a_sled_is_excluded_not_packed() {
        let fleet = vec![inst("huge", 8, 1000, 0)];
        let s = size(&gen2(), &fleet, &Assumptions::default());
        assert_eq!(s.excluded, vec!["huge".to_string()]);
        assert_eq!(s.compute_sleds, 0);
    }
}
