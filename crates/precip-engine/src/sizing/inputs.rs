//! From scan rows to sizing inputs. Pure over the resources, relationships
//! and placements of one scan.

use crate::sizing::{Fit, Instance};
use precip_types::scan::{Edge, Relationship, Resource, ResourceKind};
use precip_types::{Disposition, Profile, WorkloadMember};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

/// One instance as sizing sees it, with where it came from and why it fits
/// or does not.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SizedInstance {
    pub arn: String,
    pub name: String,
    pub workload: String,
    pub environment: String,
    pub disposition: Disposition,
    pub account_id: String,
    pub region: String,
    pub instance_type: String,
    pub architecture: String,
    pub vcpus: u32,
    pub memory_gib: u64,
    pub disk_gib: u64,
    pub volumes: u32,
    pub largest_volume_gib: u64,
    pub fit: Fit,
    /// Why the instance needs remediation or cannot move. Empty when it
    /// fits as-is.
    pub findings: Vec<String>,
}

impl SizedInstance {
    pub fn to_instance(&self) -> Instance {
        Instance {
            id: self.arn.clone(),
            disposition: self.disposition,
            vcpus: self.vcpus,
            memory_gib: self.memory_gib,
            disk_gib: self.disk_gib,
            fit: self.fit,
        }
    }
}

fn body_str<'a>(r: &'a Resource, key: &str) -> Option<&'a str> {
    r.body.get(key).and_then(|v| v.as_str())
}

fn body_u64(r: &Resource, key: &str) -> Option<u64> {
    r.body.get(key).and_then(|v| v.as_u64())
}

/// Instances in pairs with a sized disposition, with their type and
/// volume data resolved and fit assessed against `profile`.
pub fn sized_instances(
    resources: &[Resource],
    relationships: &[Relationship],
    members: &[WorkloadMember],
    dispositions: &BTreeMap<(String, String), Disposition>,
    profile: &Profile,
) -> Vec<SizedInstance> {
    let by_arn: HashMap<&str, &Resource> =
        resources.iter().map(|r| (r.arn.as_str(), r)).collect();
    let placement: HashMap<&str, &WorkloadMember> =
        members.iter().map(|m| (m.arn.as_str(), m)).collect();

    // Instance types by (region, name).
    let types: HashMap<(&str, &str), &Resource> = resources
        .iter()
        .filter(|r| r.kind == ResourceKind::InstanceType)
        .filter_map(|r| Some(((r.region.as_str(), body_str(r, "name")?), r)))
        .collect();

    // Volumes attached to each instance.
    let mut volumes: HashMap<&str, Vec<&Resource>> = HashMap::new();
    for rel in relationships.iter().filter(|r| r.edge == Edge::AttachedTo) {
        if let Some(v) = by_arn.get(rel.from_arn.as_str()) {
            if v.kind == ResourceKind::EbsVolume {
                volumes.entry(rel.to_arn.as_str()).or_default().push(v);
            }
        }
    }

    let mut out = Vec::new();
    for r in resources.iter().filter(|r| r.kind == ResourceKind::Ec2Instance) {
        let Some(m) = placement.get(r.arn.as_str()) else { continue };
        let Some(disposition) = dispositions
            .get(&(m.workload.clone(), m.environment.clone()))
            .copied()
            .filter(|d| d.is_sized())
        else {
            continue;
        };
        let instance_type = body_str(r, "instance_type").unwrap_or("unknown");
        let ty = types.get(&(r.region.as_str(), instance_type));
        let vcpus = ty
            .and_then(|t| body_u64(t, "vcpus"))
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0);
        let memory_gib = ty
            .and_then(|t| body_u64(t, "memory_mib"))
            .map(|mib| mib.div_ceil(1024))
            .unwrap_or(0);
        let vols = volumes.get(r.arn.as_str()).cloned().unwrap_or_default();
        let sizes: Vec<u64> =
            vols.iter().filter_map(|v| body_u64(v, "size_gib")).collect();
        let disk_gib = sizes.iter().sum();
        let largest_volume_gib = sizes.iter().copied().max().unwrap_or(0);
        let architecture =
            body_str(r, "architecture").unwrap_or("unknown").to_string();

        let (fit, findings) = assess(
            &FitInput {
                architecture: &architecture,
                vcpus,
                memory_gib,
                volumes: u32::try_from(vols.len()).unwrap_or(u32::MAX),
                largest_volume_gib,
                instance_store_volumes: body_u64(
                    r,
                    "instance_store_volume_count",
                )
                .unwrap_or(0),
                nics: body_u64(r, "network_interface_count").unwrap_or(0),
                boot_mode: body_str(r, "current_boot_mode")
                    .or(body_str(r, "boot_mode")),
                platform: body_str(r, "platform"),
                bare_metal: ty
                    .and_then(|t| t.body.get("bare_metal")?.as_bool())
                    .unwrap_or(false),
                gpus: ty.and_then(|t| body_u64(t, "gpus")).unwrap_or(0),
                accelerators: ty
                    .and_then(|t| body_u64(t, "accelerators"))
                    .unwrap_or(0),
                type_known: ty.is_some(),
            },
            profile,
            disposition,
        );

        out.push(SizedInstance {
            arn: r.arn.clone(),
            name: r
                .tags
                .get("Name")
                .cloned()
                .or_else(|| body_str(r, "instance_id").map(str::to_string))
                .unwrap_or_else(|| r.arn.clone()),
            workload: m.workload.clone(),
            environment: m.environment.clone(),
            disposition,
            account_id: r.account_id.clone(),
            region: r.region.clone(),
            instance_type: instance_type.to_string(),
            architecture,
            vcpus,
            memory_gib,
            disk_gib,
            volumes: u32::try_from(vols.len()).unwrap_or(u32::MAX),
            largest_volume_gib,
            fit,
            findings,
        });
    }
    out.sort_by(|a, b| {
        (&a.workload, &a.environment, &a.name).cmp(&(
            &b.workload,
            &b.environment,
            &b.name,
        ))
    });
    out
}

/// What fit assessment looks at.
pub struct FitInput<'a> {
    pub architecture: &'a str,
    pub vcpus: u32,
    pub memory_gib: u64,
    pub volumes: u32,
    pub largest_volume_gib: u64,
    pub instance_store_volumes: u64,
    pub nics: u64,
    pub boot_mode: Option<&'a str>,
    pub platform: Option<&'a str>,
    pub bare_metal: bool,
    pub gpus: u64,
    pub accelerators: u64,
    pub type_known: bool,
}

/// RFD 0001, "Fit checks and blockers". Blocked conditions win over
/// remediations.
pub fn assess(
    i: &FitInput<'_>,
    profile: &Profile,
    disposition: Disposition,
) -> (Fit, Vec<String>) {
    let mut blocked = Vec::new();
    let mut remediate = Vec::new();

    // A CPU architecture change is a platform change. Under replatform it
    // is a rebuild to plan for; under rehost or relocate the instance
    // cannot move as it is.
    if i.architecture != "x86_64" {
        if i.architecture == "arm64" && disposition == Disposition::Replatform {
            remediate.push("arm64: rebuild the image for x86-64".to_string());
        } else if i.architecture == "arm64" {
            blocked.push(
                "arm64 cannot run on x86-64 sleds as-is; replatform to rebuild"
                    .to_string(),
            );
        } else {
            blocked.push(format!(
                "{} instances cannot run on x86-64 sleds",
                i.architecture
            ));
        }
    }
    if i.gpus > 0 || i.accelerators > 0 {
        blocked.push("GPU or accelerator instance family".to_string());
    }
    if i.bare_metal {
        blocked.push("bare-metal instance type".to_string());
    }
    if i.memory_gib > profile.sled.dram_gib {
        blocked.push(format!(
            "{} GiB memory exceeds a sled's {} GiB DRAM",
            i.memory_gib, profile.sled.dram_gib
        ));
    }
    if !i.type_known {
        remediate.push(
            "instance type not resolved; vCPU and memory unknown".to_string(),
        );
    }
    if i.vcpus > profile.instance.max_vcpu {
        remediate.push(format!(
            "{} vCPUs exceeds the {} per-instance cap; scale horizontally",
            i.vcpus, profile.instance.max_vcpu
        ));
    }
    if i.memory_gib > profile.instance.max_memory_gib {
        remediate.push(format!(
            "{} GiB exceeds the {} GiB per-instance cap; scale horizontally",
            i.memory_gib, profile.instance.max_memory_gib
        ));
    }
    if i.volumes > profile.instance.max_disks {
        remediate.push(format!(
            "{} volumes exceeds {} disks per instance; consolidate",
            i.volumes, profile.instance.max_disks
        ));
    }
    if i.largest_volume_gib > profile.instance.max_disk_gib {
        remediate.push(format!(
            "{} GiB volume exceeds the {} GiB disk maximum; span devices",
            i.largest_volume_gib, profile.instance.max_disk_gib
        ));
    }
    if i.nics > u64::from(profile.instance.max_nics) {
        remediate.push(format!(
            "{} network interfaces exceeds {}",
            i.nics, profile.instance.max_nics
        ));
    }
    if i.instance_store_volumes > 0 {
        remediate.push(
            "instance store volumes are ephemeral; move data to a disk"
                .to_string(),
        );
    }
    if i.boot_mode.is_some_and(|b| b.contains("legacy-bios") || b == "bios") {
        remediate.push("BIOS boot; Oxide guests boot UEFI".to_string());
    }
    if i.platform.is_some_and(|p| p.starts_with("Windows")) {
        remediate.push(
            "Windows: VM export unavailable, bring a licensed image"
                .to_string(),
        );
    }

    if !blocked.is_empty() {
        blocked.extend(remediate);
        (Fit::CannotMove, blocked)
    } else if !remediate.is_empty() {
        (Fit::NeedsRemediation, remediate)
    } else {
        (Fit::CleanFit, Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gen2() -> Profile {
        crate::sizing::profiles::builtin("gen1-1024")
            .expect("listed")
            .expect("parses")
    }

    fn input<'a>(arch: &'a str) -> FitInput<'a> {
        FitInput {
            architecture: arch,
            vcpus: 4,
            memory_gib: 16,
            volumes: 1,
            largest_volume_gib: 100,
            instance_store_volumes: 0,
            nics: 1,
            boot_mode: Some("uefi"),
            platform: Some("Linux/UNIX"),
            bare_metal: false,
            gpus: 0,
            accelerators: 0,
            type_known: true,
        }
    }

    #[test]
    fn x86_linux_within_limits_is_a_clean_fit() {
        let (fit, findings) =
            assess(&input("x86_64"), &gen2(), Disposition::Rehost);
        assert_eq!(fit, Fit::CleanFit);
        assert!(findings.is_empty());
    }

    #[test]
    fn graviton_is_blocked_under_rehost_and_a_rebuild_under_replatform() {
        let (fit, findings) =
            assess(&input("arm64"), &gen2(), Disposition::Rehost);
        assert_eq!(fit, Fit::CannotMove);
        assert!(findings[0].contains("replatform"));
        let (fit, findings) =
            assess(&input("arm64"), &gen2(), Disposition::Replatform);
        assert_eq!(
            fit,
            Fit::NeedsRemediation,
            "a replatform sizes the rebuild"
        );
        assert!(findings[0].contains("rebuild"));
    }

    #[test]
    fn large_volume_and_bios_need_remediation() {
        let mut i = input("x86_64");
        i.largest_volume_gib = 2048;
        i.boot_mode = Some("legacy-bios");
        let (fit, findings) = assess(&i, &gen2(), Disposition::Rehost);
        assert_eq!(fit, Fit::NeedsRemediation);
        assert_eq!(findings.len(), 2);
    }

    #[test]
    fn blocked_outranks_remediation_and_keeps_both_findings() {
        let mut i = input("x86_64");
        i.gpus = 1;
        i.volumes = 20;
        let (fit, findings) = assess(&i, &gen2(), Disposition::Rehost);
        assert_eq!(fit, Fit::CannotMove);
        assert_eq!(findings.len(), 2);
    }
}
