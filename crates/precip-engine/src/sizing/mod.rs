//! The sizing arithmetic from RFD 0001, section "Sizing". Pure functions
//! over an inventory and a profile so the whole thing is unit-testable.

pub mod inputs;
pub mod profiles;

use inputs::SizedInstance;
use precip_types::{Arn, Disposition, PairKey, Profile};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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

/// What one instance asks of a sled, after growth.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Demand {
    vcpus: f64,
    memory_gib: f64,
}

impl Demand {
    fn fits_in(&self, other: Demand, cap: &SledCapacity) -> bool {
        self.vcpus + other.vcpus <= cap.vcpus
            && self.memory_gib + other.memory_gib <= cap.memory_gib
    }

    fn add(&mut self, other: Demand) {
        self.vcpus += other.vcpus;
        self.memory_gib += other.memory_gib;
    }
}

impl SledCapacity {
    pub fn new(profile: &Profile, a: &Assumptions) -> Self {
        let s = &profile.sled;
        let vcpus = f64::from(s.cores * s.threads_per_core) * a.threshold;
        // Published usable figures win over the derivation.
        let usable_memory = match s.usable_memory_gib {
            Some(gib) => gib as f64,
            None => {
                s.dram_gib.saturating_sub(s.reservation_gib) as f64
                    * s.reservoir_fraction
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
        Self { vcpus, memory_gib, disk_tib }
    }

    /// Whether the demand fits an empty sled.
    fn holds(&self, d: Demand) -> bool {
        Demand::default().fits_in(d, self)
    }

    /// Sleds needed for the demands, packed first-fit decreasing by
    /// memory, then vCPU.
    fn pack(&self, demands: &[Demand]) -> u32 {
        let mut sorted = demands.to_vec();
        sorted.sort_by(|a, b| {
            b.memory_gib
                .total_cmp(&a.memory_gib)
                .then_with(|| b.vcpus.total_cmp(&a.vcpus))
        });
        let mut sleds: Vec<Demand> = Vec::new();
        for d in sorted {
            match sleds.iter_mut().find(|s| s.fits_in(d, self)) {
                Some(s) => s.add(d),
                None => sleds.push(d),
            }
        }
        u32::try_from(sleds.len()).unwrap_or(u32::MAX)
    }
}

impl Sizing {
    /// Size the instances onto sleds and racks of the profile.
    pub fn new(
        profile: &Profile,
        instances: &[Instance],
        a: &Assumptions,
    ) -> Self {
        let cap = SledCapacity::new(profile, a);

        let mut not_sized = Vec::new();
        let mut excluded = Vec::new();
        let mut demands = Vec::new();
        let mut total_disk_gib = 0.0;
        for i in instances {
            if !i.disposition.is_sized() {
                not_sized.push(i.arn.clone());
                continue;
            }
            let demand = Demand {
                vcpus: f64::from(i.vcpus) * a.growth_factor,
                memory_gib: i.memory_gib as f64 * a.growth_factor,
            };
            // An instance that does not fit an empty sled cannot be placed
            // anywhere; treat it like any other "cannot move" instance
            // rather than opening a sled it would overflow.
            if i.fit == Fit::CannotMove || !cap.holds(demand) {
                excluded.push(i.arn.clone());
                continue;
            }
            demands.push(demand);
            total_disk_gib += i.disk_gib as f64 * a.growth_factor;
        }

        let compute_sleds = cap.pack(&demands);

        // Sled disk capacity is already net of replication, so demand is
        // the disks as sized.
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

        let racks_before_headroom =
            sleds.div_ceil(profile.sleds_per_rack).max(1);
        sleds += a.headroom * racks_before_headroom;
        let racks = sleds.div_ceil(profile.sleds_per_rack);
        let spare_sleds = racks * profile.sleds_per_rack - sleds;

        Self {
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

    fn add(&mut self, i: &SizedInstance) {
        self.instances += 1;
        self.vcpus += u64::from(i.vcpus);
        self.memory_gib += i.memory_gib;
        self.disk_gib += i.disk_gib;
        match i.fit {
            Fit::CleanFit => self.clean += 1,
            Fit::NeedsRemediation => self.remediate += 1,
            Fit::CannotMove => self.blocked += 1,
        }
    }

    /// One row per pair, in pair order.
    pub fn by_pair(instances: &[SizedInstance]) -> Vec<Self> {
        let mut map: BTreeMap<PairKey, Self> = BTreeMap::new();
        for i in instances {
            map.entry(i.pair.clone())
                .or_insert_with(|| Self::new(i.pair.clone()))
                .add(i);
        }
        map.into_values().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gen1() -> Profile {
        profiles::BuiltinProfile::Gen1.profile().expect("parses")
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
        let cap = SledCapacity::new(&gen1(), &Assumptions::default());
        assert_eq!(cap.vcpus, 96.0, "64 cores, 2 threads, 75%");
        assert!((cap.memory_gib - 853.0 * 0.75).abs() < 1e-9);
        assert!((cap.disk_tib - 26.6 * 0.75).abs() < 1e-9);
    }

    #[test]
    fn sled_capacity_is_derived_when_nothing_is_published() {
        let mut p = gen1();
        p.sled.usable_memory_gib = None;
        p.sled.usable_storage_tib = None;
        let cap = SledCapacity::new(&p, &Assumptions::default());
        let expected_mem = (1024.0 - 44.0) * 0.91 * 0.75;
        assert!((cap.memory_gib - expected_mem).abs() < 1e-9);
        let expected_disk = 10.0 * 3.2 / 3.0 * 0.75;
        assert!((cap.disk_tib - expected_disk).abs() < 1e-9);
    }

    #[test]
    fn empty_inventory_is_one_headroom_sled_in_one_rack() {
        let s = Sizing::new(&gen1(), &[], &Assumptions::default());
        assert_eq!(s.compute_sleds, 0);
        assert_eq!(s.sleds, 1);
        assert_eq!(s.racks, 1);
    }

    #[test]
    fn cannot_move_instances_are_excluded_and_listed() {
        let mut blocked = inst("arm", 4, 16, 100);
        blocked.fit = Fit::CannotMove;
        let s = Sizing::new(&gen1(), &[blocked], &Assumptions::default());
        assert_eq!(s.excluded, vec![Arn::from("arm")]);
        assert_eq!(s.compute_sleds, 0);
    }

    #[test]
    fn retired_workloads_are_not_sized() {
        let mut retired = inst("old", 64, 512, 1000);
        retired.disposition = Disposition::Retire;
        let s = Sizing::new(&gen1(), &[retired], &Assumptions::default());
        assert_eq!(s.not_sized, vec![Arn::from("old")]);
        assert_eq!(s.compute_sleds, 0);
    }

    #[test]
    fn replatform_workloads_are_sized() {
        let mut re = inst("re", 4, 16, 100);
        re.disposition = Disposition::Replatform;
        let s = Sizing::new(&gen1(), &[re], &Assumptions::default());
        assert!(s.not_sized.is_empty());
        assert_eq!(s.compute_sleds, 1);
    }

    #[test]
    fn memory_heavy_fleet_is_compute_bound_by_memory() {
        // 20 r-class style instances: 16 vCPU, 128 GiB each. One sled holds
        // ~640 GiB after threshold, so 4 per sled, 5 sleds.
        let fleet: Vec<Instance> =
            (0..20).map(|i| inst(&format!("r{i}"), 16, 128, 100)).collect();
        let s = Sizing::new(&gen1(), &fleet, &Assumptions::default());
        assert_eq!(s.compute_sleds, 5);
        assert_eq!(s.bound, Bound::Compute);
        assert_eq!(s.sleds, 6);
        assert_eq!(s.racks, 1);
    }

    #[test]
    fn large_disks_make_storage_bind() {
        let fleet: Vec<Instance> =
            (0..40).map(|i| inst(&format!("d{i}"), 2, 8, 10_000)).collect();
        let s = Sizing::new(&gen1(), &fleet, &Assumptions::default());
        assert_eq!(s.bound, Bound::Storage);
        assert!(s.storage_sleds > s.compute_sleds);
    }

    #[test]
    fn growth_factor_scales_demand() {
        // Two 300 GiB instances share one sled (~640 GiB usable); doubled,
        // each needs its own.
        let fleet = vec![inst("a", 40, 300, 0), inst("b", 40, 300, 0)];
        let base = Sizing::new(&gen1(), &fleet, &Assumptions::default());
        let grown = Sizing::new(
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
        let s = Sizing::new(&gen1(), &fleet, &Assumptions::default());
        assert_eq!(s.excluded, vec![Arn::from("huge")]);
        assert_eq!(s.compute_sleds, 0);
    }
}
