//! The sizing arithmetic from RFD 0001, section "Sizing". Pure functions
//! over an inventory and a profile so the whole thing is unit-testable.

pub mod inputs;
pub mod profiles;

use precip_types::{Arn, PairKey};
use precip_types::{Disposition, Profile};
use serde::{Deserialize, Serialize};

precip_types::named_enum! {
    /// Whether an instance can run on Oxide as-is.
    pub enum Fit {
        CleanFit = "clean_fit",
        NeedsRemediation = "needs_remediation",
        CannotMove = "cannot_move",
    }
}

/// Demand for one instance, already multiplied out for ASG capacity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instance {
    pub arn: Arn,
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

precip_types::named_enum! {
    /// Which resource set the sled count.
    pub enum Bound {
        Compute = "compute",
        Storage = "storage",
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sizing {
    pub compute_sleds: u32,
    pub storage_sleds: u32,
    pub bound: Bound,
    pub sleds: u32,
    pub racks: u32,
    pub spare_sleds: u32,
    /// Instances whose workload disposition is not sized.
    pub not_sized: Vec<Arn>,
    /// Instances in scope whose fit is "cannot move".
    pub excluded: Vec<Arn>,
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
    // Published usable figures win over the derivation.
    let usable_memory = match s.usable_memory_gib {
        Some(gib) => gib as f64,
        None => {
            s.dram_gib.saturating_sub(s.reservation_gib) as f64
                * s.reservoir_pct
        }
    };
    let memory_gib = usable_memory * a.threshold;
    let usable_disk = match s.usable_storage_tib {
        Some(tib) => tib,
        None => {
            f64::from(s.nvme_bays) * s.bay_size_tib
                / f64::from(profile.storage.replication_factor)
        }
    };
    let disk_tib = usable_disk * a.threshold;
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
            not_sized.push(i.arn.clone());
            continue;
        }
        let vcpus = f64::from(i.vcpus) * a.growth_factor;
        let memory_gib = i.memory_gib as f64 * a.growth_factor;
        // An instance that does not fit an empty sled cannot be placed
        // anywhere; treat it like any other "cannot move" instance rather
        // than opening a sled it would overflow.
        let oversized = vcpus > cap.vcpus || memory_gib > cap.memory_gib;
        if i.fit == Fit::CannotMove || oversized {
            excluded.push(i.arn.clone());
            continue;
        }
        demands.push((vcpus, memory_gib));
        total_disk_gib += i.disk_gib as f64 * a.growth_factor;
    }

    let compute_sleds = pack_compute(&demands, &cap);

    // Sled disk capacity is already net of replication, so demand is the
    // disks as sized.
    let storage_demand_tib = total_disk_gib / 1024.0;
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkloadDemand {
    #[serde(flatten)]
    pub pair: PairKey,
    pub instances: usize,
    pub vcpus: u64,
    pub memory_gib: u64,
    pub disk_gib: u64,
    pub clean: usize,
    pub remediate: usize,
    pub blocked: usize,
}

impl WorkloadDemand {
    /// A pair with nothing counted yet.
    pub fn new(pair: PairKey) -> Self {
        Self {
            pair,
            instances: 0,
            vcpus: 0,
            memory_gib: 0,
            disk_gib: 0,
            clean: 0,
            remediate: 0,
            blocked: 0,
        }
    }
}

pub fn demand_by_workload(
    instances: &[inputs::SizedInstance],
) -> Vec<WorkloadDemand> {
    let mut map: std::collections::BTreeMap<PairKey, WorkloadDemand> =
        std::collections::BTreeMap::new();
    for i in instances {
        let d = map
            .entry(i.pair.clone())
            .or_insert_with(|| WorkloadDemand::new(i.pair.clone()));
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

    fn gen1() -> Profile {
        profiles::builtin("gen1-1024").expect("listed").expect("parses")
    }

    fn inst(id: &str, vcpus: u32, mem: u64, disk: u64) -> Instance {
        Instance {
            arn: Arn::from(id),
            disposition: Disposition::Rehost,
            vcpus,
            memory_gib: mem,
            disk_gib: disk,
            fit: Fit::CleanFit,
        }
    }

    #[test]
    fn sled_capacity_uses_published_figures_then_the_threshold() {
        let cap = sled_capacity(&gen1(), &Assumptions::default());
        assert_eq!(cap.vcpus, 96.0, "64 cores, 2 threads, 75%");
        assert!((cap.memory_gib - 853.0 * 0.75).abs() < 1e-9);
        assert!((cap.disk_tib - 26.6 * 0.75).abs() < 1e-9);
    }

    #[test]
    fn sled_capacity_is_derived_when_nothing_is_published() {
        let mut p = gen1();
        p.sled.usable_memory_gib = None;
        p.sled.usable_storage_tib = None;
        let cap = sled_capacity(&p, &Assumptions::default());
        let expected_mem = (1024.0 - 44.0) * 0.91 * 0.75;
        assert!((cap.memory_gib - expected_mem).abs() < 1e-9);
        let expected_disk = 10.0 * 3.2 / 3.0 * 0.75;
        assert!((cap.disk_tib - expected_disk).abs() < 1e-9);
    }

    #[test]
    fn empty_inventory_is_one_headroom_sled_in_one_rack() {
        let s = size(&gen1(), &[], &Assumptions::default());
        assert_eq!(s.compute_sleds, 0);
        assert_eq!(s.sleds, 1);
        assert_eq!(s.racks, 1);
    }

    #[test]
    fn cannot_move_instances_are_excluded_and_listed() {
        let mut blocked = inst("arm", 4, 16, 100);
        blocked.fit = Fit::CannotMove;
        let s = size(&gen1(), &[blocked], &Assumptions::default());
        assert_eq!(s.excluded, vec![Arn::from("arm")]);
        assert_eq!(s.compute_sleds, 0);
    }

    #[test]
    fn retired_workloads_are_not_sized() {
        let mut retired = inst("old", 64, 512, 1000);
        retired.disposition = Disposition::Retire;
        let s = size(&gen1(), &[retired], &Assumptions::default());
        assert_eq!(s.not_sized, vec![Arn::from("old")]);
        assert_eq!(s.compute_sleds, 0);
    }

    #[test]
    fn replatform_workloads_are_sized() {
        let mut re = inst("re", 4, 16, 100);
        re.disposition = Disposition::Replatform;
        let s = size(&gen1(), &[re], &Assumptions::default());
        assert!(s.not_sized.is_empty());
        assert_eq!(s.compute_sleds, 1);
    }

    #[test]
    fn memory_heavy_fleet_is_compute_bound_by_memory() {
        // 20 r-class style instances: 16 vCPU, 128 GiB each. One sled holds
        // ~640 GiB after threshold, so 4 per sled, 5 sleds.
        let fleet: Vec<Instance> =
            (0..20).map(|i| inst(&format!("r{i}"), 16, 128, 100)).collect();
        let s = size(&gen1(), &fleet, &Assumptions::default());
        assert_eq!(s.compute_sleds, 5);
        assert_eq!(s.bound, Bound::Compute);
        assert_eq!(s.sleds, 6);
        assert_eq!(s.racks, 1);
    }

    #[test]
    fn large_disks_make_storage_bind() {
        let fleet: Vec<Instance> =
            (0..40).map(|i| inst(&format!("d{i}"), 2, 8, 10_000)).collect();
        let s = size(&gen1(), &fleet, &Assumptions::default());
        assert_eq!(s.bound, Bound::Storage);
        assert!(s.storage_sleds > s.compute_sleds);
    }

    #[test]
    fn growth_factor_scales_demand() {
        // Two 300 GiB instances share one sled (~640 GiB usable); doubled,
        // each needs its own.
        let fleet = vec![inst("a", 40, 300, 0), inst("b", 40, 300, 0)];
        let base = size(&gen1(), &fleet, &Assumptions::default());
        let grown = size(
            &gen1(),
            &fleet,
            &Assumptions { growth_factor: 2.0, ..Assumptions::default() },
        );
        assert_eq!(base.compute_sleds, 1);
        assert_eq!(grown.compute_sleds, 2);
    }

    #[test]
    fn instance_larger_than_a_sled_is_excluded_not_packed() {
        let fleet = vec![inst("huge", 8, 1000, 0)];
        let s = size(&gen1(), &fleet, &Assumptions::default());
        assert_eq!(s.excluded, vec![Arn::from("huge")]);
        assert_eq!(s.compute_sleds, 0);
    }
}
