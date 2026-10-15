//! From scan rows to sizing inputs. Pure over the resources, relationships
//! and placements of one scan.

use crate::sizing::{Fit, Instance};
use precip_types::scan::{Edge, Relationship, Resource};
use precip_types::{
    AccountId, Arn, Body, Disposition, PairKey, Profile, Region,
    WorkloadMember, body,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fmt;

/// An instance's CPU architecture as AWS reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum Architecture {
    X86_64,
    Arm64,
    /// Anything else AWS reports, such as `i386` or `x86_64_mac`, or
    /// `unknown` when the scan did not record one.
    Other(String),
}

impl From<&str> for Architecture {
    fn from(s: &str) -> Self {
        match s {
            "x86_64" => Self::X86_64,
            "arm64" => Self::Arm64,
            other => Self::Other(other.to_string()),
        }
    }
}

impl From<String> for Architecture {
    fn from(s: String) -> Self {
        Self::from(s.as_str())
    }
}

impl From<Architecture> for String {
    fn from(a: Architecture) -> Self {
        a.to_string()
    }
}

impl fmt::Display for Architecture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Self::X86_64 => "x86_64",
            Self::Arm64 => "arm64",
            Self::Other(s) => s,
        })
    }
}

/// How an instance boots. AWS reports `legacy-bios`, `uefi` or
/// `uefi-preferred`; only legacy BIOS needs work, as Oxide guests boot
/// UEFI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootMode {
    Bios,
    Uefi,
}

impl From<&str> for BootMode {
    fn from(s: &str) -> Self {
        if s.contains("legacy-bios") || s == "bios" {
            Self::Bios
        } else {
            Self::Uefi
        }
    }
}

/// The operating system family from an instance's platform details.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Windows,
    LinuxOrUnix,
}

impl From<&str> for Platform {
    /// AWS platform details start with `Windows` for every Windows
    /// variant, such as `Windows with SQL Server Standard`.
    fn from(s: &str) -> Self {
        if s.starts_with("Windows") { Self::Windows } else { Self::LinuxOrUnix }
    }
}

/// One reason an instance cannot move as it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "finding", rename_all = "snake_case")]
pub enum Finding {
    /// arm64 under rehost or relocate.
    ArmNeedsReplatform,
    /// arm64 under replatform: the image is rebuilt for x86-64.
    ArmRebuild,
    UnsupportedArchitecture {
        architecture: String,
    },
    Accelerator,
    BareMetal,
    MemoryOverSled {
        memory_gib: u64,
        dram_gib: u64,
    },
    UnknownType,
    TooManyVcpus {
        vcpus: u32,
        max: u32,
    },
    TooMuchMemory {
        memory_gib: u64,
        max: u64,
    },
    TooManyVolumes {
        volumes: u32,
        max: u32,
    },
    VolumeTooLarge {
        gib: u64,
        max: u64,
    },
    TooManyNics {
        nics: usize,
        max: u32,
    },
    InstanceStore,
    BiosBoot,
    Windows,
}

impl Finding {
    /// Whether the finding stops the move, rather than being work to do
    /// before it.
    pub(crate) fn blocks(&self) -> bool {
        match self {
            Self::ArmNeedsReplatform
            | Self::UnsupportedArchitecture { .. }
            | Self::Accelerator
            | Self::BareMetal
            | Self::MemoryOverSled { .. } => true,
            Self::ArmRebuild
            | Self::UnknownType
            | Self::TooManyVcpus { .. }
            | Self::TooMuchMemory { .. }
            | Self::TooManyVolumes { .. }
            | Self::VolumeTooLarge { .. }
            | Self::TooManyNics { .. }
            | Self::InstanceStore
            | Self::BiosBoot
            | Self::Windows => false,
        }
    }
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ArmNeedsReplatform => f.write_str(
                "arm64 cannot run on x86-64 sleds as-is; replatform to rebuild",
            ),
            Self::ArmRebuild => {
                f.write_str("arm64: rebuild the image for x86-64")
            }
            Self::UnsupportedArchitecture { architecture } => {
                write!(f, "{architecture} instances cannot run on x86-64 sleds")
            }
            Self::Accelerator => {
                f.write_str("GPU or accelerator instance family")
            }
            Self::BareMetal => f.write_str("bare-metal instance type"),
            Self::MemoryOverSled { memory_gib, dram_gib } => write!(
                f,
                "{memory_gib} GiB memory exceeds a sled's {dram_gib} GiB DRAM"
            ),
            Self::UnknownType => f.write_str(
                "instance type not resolved; vCPU and memory unknown",
            ),
            Self::TooManyVcpus { vcpus, max } => write!(
                f,
                "{vcpus} vCPUs exceeds the {max} per-instance cap; scale horizontally"
            ),
            Self::TooMuchMemory { memory_gib, max } => write!(
                f,
                "{memory_gib} GiB exceeds the {max} GiB per-instance cap; scale horizontally"
            ),
            Self::TooManyVolumes { volumes, max } => write!(
                f,
                "{volumes} volumes exceeds {max} disks per instance; consolidate"
            ),
            Self::VolumeTooLarge { gib, max } => write!(
                f,
                "{gib} GiB volume exceeds the {max} GiB disk maximum; span devices"
            ),
            Self::TooManyNics { nics, max } => {
                write!(f, "{nics} network interfaces exceeds {max}")
            }
            Self::InstanceStore => f.write_str(
                "instance store volumes are ephemeral; move data to a disk",
            ),
            Self::BiosBoot => f.write_str("BIOS boot; Oxide guests boot UEFI"),
            Self::Windows => f.write_str(
                "Windows: VM export unavailable, bring a licensed image",
            ),
        }
    }
}

/// Whether an instance can move, and every reason it cannot as it is,
/// blocking findings first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Assessment {
    pub fit: Fit,
    pub findings: Vec<Finding>,
}

impl Assessment {
    fn new(mut findings: Vec<Finding>) -> Self {
        findings.sort_by_key(|f| !f.blocks());
        let fit = match findings.first() {
            None => Fit::CleanFit,
            Some(f) if f.blocks() => Fit::CannotMove,
            Some(_) => Fit::NeedsRemediation,
        };
        Self { fit, findings }
    }
}

/// One instance as sizing sees it, with where it came from and why it fits
/// or does not.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SizedInstance {
    pub arn: Arn,
    pub name: String,
    #[serde(flatten)]
    pub pair: PairKey,
    pub disposition: Disposition,
    pub account_id: AccountId,
    pub region: Region,
    pub instance_type: String,
    pub architecture: Architecture,
    pub vcpus: u32,
    pub memory_gib: u64,
    pub disk_gib: u64,
    pub volumes: u32,
    pub largest_volume_gib: u64,
    pub fit: Fit,
    /// Why the instance needs remediation or cannot move. Empty when it
    /// fits as-is.
    pub findings: Vec<Finding>,
}

impl SizedInstance {
    /// The findings as one line, separated by semicolons.
    pub fn findings_text(&self) -> String {
        self.findings
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ")
    }
}

impl From<&SizedInstance> for Instance {
    fn from(i: &SizedInstance) -> Self {
        Self {
            arn: i.arn.clone(),
            disposition: i.disposition,
            vcpus: i.vcpus,
            memory_gib: i.memory_gib,
            disk_gib: i.disk_gib,
            fit: i.fit,
        }
    }
}

/// Instances in pairs with a sized disposition, with their type and
/// volume data resolved and fit assessed against `profile`.
pub fn sized_instances(
    resources: &[Resource],
    relationships: &[Relationship],
    members: &[WorkloadMember],
    dispositions: &BTreeMap<PairKey, Disposition>,
    profile: &Profile,
) -> Vec<SizedInstance> {
    let by_arn: HashMap<&str, &Resource> =
        resources.iter().map(|r| (r.arn.as_str(), r)).collect();
    let placement: HashMap<&str, &WorkloadMember> =
        members.iter().map(|m| (m.arn.as_str(), m)).collect();

    // Instance types by (region, name).
    let mut types: HashMap<(&str, &str), &body::InstanceType> = HashMap::new();
    for r in resources {
        if let Body::InstanceType(t) = &r.body {
            types.insert((r.region.as_str(), t.name.as_str()), t);
        }
    }

    // Volumes attached to each instance.
    let mut volumes: HashMap<&str, Vec<&body::Volume>> = HashMap::new();
    for rel in relationships.iter().filter(|r| r.edge == Edge::AttachedTo) {
        if let Some(Body::EbsVolume(v)) =
            by_arn.get(rel.from_arn.as_str()).map(|r| &r.body)
        {
            volumes.entry(rel.to_arn.as_str()).or_default().push(v);
        }
    }

    let mut out = Vec::new();
    for r in resources {
        let Body::Ec2Instance(inst) = &r.body else { continue };
        let Some(m) = placement.get(r.arn.as_str()) else { continue };
        let Some(disposition) =
            dispositions.get(&m.pair).copied().filter(|d| d.is_sized())
        else {
            continue;
        };
        let instance_type = inst.instance_type.as_deref().unwrap_or("unknown");
        let ty = types.get(&(r.region.as_str(), instance_type)).copied();
        let vcpus = ty
            .and_then(|t| t.vcpus)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0);
        let memory_gib = ty
            .and_then(|t| t.memory_mib)
            .and_then(|mib| u64::try_from(mib).ok())
            .map_or(0, |mib| mib.div_ceil(1024));
        let vols =
            volumes.get(r.arn.as_str()).map(Vec::as_slice).unwrap_or_default();
        let sizes: Vec<u64> = vols
            .iter()
            .filter_map(|v| v.size_gib.and_then(|g| u64::try_from(g).ok()))
            .collect();
        let disk_gib = sizes.iter().sum();
        let largest_volume_gib = sizes.iter().copied().max().unwrap_or(0);
        let volume_count = u32::try_from(vols.len()).unwrap_or(u32::MAX);
        let architecture = Architecture::from(
            inst.architecture.as_deref().unwrap_or("unknown"),
        );

        let Assessment { fit, findings } = FitInput {
            architecture: &architecture,
            vcpus,
            memory_gib,
            volumes: volume_count,
            largest_volume_gib,
            instance_store_volumes: inst.instance_store_volume_count,
            nics: inst.network_interface_count,
            boot_mode: inst
                .current_boot_mode
                .as_deref()
                .or(inst.boot_mode.as_deref())
                .map(BootMode::from),
            platform: inst.platform.as_deref().map(Platform::from),
            bare_metal: ty.and_then(|t| t.bare_metal).unwrap_or(false),
            gpus: ty.map_or(0, |t| t.gpus),
            accelerators: ty.map_or(0, |t| t.accelerators),
            type_known: ty.is_some(),
        }
        .assess(profile, disposition);

        let instance_id =
            Some(inst.instance_id.as_str()).filter(|id| !id.is_empty());
        out.push(SizedInstance {
            arn: r.arn.clone(),
            name: r
                .tags
                .get("Name")
                .map(String::as_str)
                .or(instance_id)
                .unwrap_or(r.arn.as_str())
                .to_string(),
            pair: m.pair.clone(),
            disposition,
            account_id: r.account_id.clone(),
            region: r.region.clone(),
            instance_type: instance_type.to_string(),
            architecture,
            vcpus,
            memory_gib,
            disk_gib,
            volumes: volume_count,
            largest_volume_gib,
            fit,
            findings,
        });
    }
    out.sort_by(|a, b| (&a.pair, &a.name).cmp(&(&b.pair, &b.name)));
    out
}

/// What fit assessment looks at.
pub(crate) struct FitInput<'a> {
    pub architecture: &'a Architecture,
    pub vcpus: u32,
    pub memory_gib: u64,
    pub volumes: u32,
    pub largest_volume_gib: u64,
    pub instance_store_volumes: usize,
    pub nics: usize,
    pub boot_mode: Option<BootMode>,
    pub platform: Option<Platform>,
    pub bare_metal: bool,
    pub gpus: usize,
    pub accelerators: usize,
    pub type_known: bool,
}

impl FitInput<'_> {
    /// RFD 0001, "Fit checks and blockers". Blocked conditions win over
    /// remediations.
    pub(crate) fn assess(
        &self,
        profile: &Profile,
        disposition: Disposition,
    ) -> Assessment {
        let limits = &profile.instance;
        let mut findings = Vec::new();

        // A CPU architecture change is a platform change. Under replatform
        // it is a rebuild to plan for; under rehost or relocate the
        // instance cannot move as it is.
        match self.architecture {
            Architecture::X86_64 => {}
            Architecture::Arm64 if disposition == Disposition::Replatform => {
                findings.push(Finding::ArmRebuild);
            }
            Architecture::Arm64 => findings.push(Finding::ArmNeedsReplatform),
            Architecture::Other(a) => {
                findings.push(Finding::UnsupportedArchitecture {
                    architecture: a.clone(),
                })
            }
        }
        if self.gpus > 0 || self.accelerators > 0 {
            findings.push(Finding::Accelerator);
        }
        if self.bare_metal {
            findings.push(Finding::BareMetal);
        }
        if self.memory_gib > profile.sled.dram_gib {
            findings.push(Finding::MemoryOverSled {
                memory_gib: self.memory_gib,
                dram_gib: profile.sled.dram_gib,
            });
        }
        if !self.type_known {
            findings.push(Finding::UnknownType);
        }
        if self.vcpus > limits.max_vcpu {
            findings.push(Finding::TooManyVcpus {
                vcpus: self.vcpus,
                max: limits.max_vcpu,
            });
        }
        if self.memory_gib > limits.max_memory_gib {
            findings.push(Finding::TooMuchMemory {
                memory_gib: self.memory_gib,
                max: limits.max_memory_gib,
            });
        }
        if self.volumes > limits.max_disks {
            findings.push(Finding::TooManyVolumes {
                volumes: self.volumes,
                max: limits.max_disks,
            });
        }
        if self.largest_volume_gib > limits.max_disk_gib {
            findings.push(Finding::VolumeTooLarge {
                gib: self.largest_volume_gib,
                max: limits.max_disk_gib,
            });
        }
        if self.nics > usize::try_from(limits.max_nics).unwrap_or(usize::MAX) {
            findings.push(Finding::TooManyNics {
                nics: self.nics,
                max: limits.max_nics,
            });
        }
        if self.instance_store_volumes > 0 {
            findings.push(Finding::InstanceStore);
        }
        if self.boot_mode == Some(BootMode::Bios) {
            findings.push(Finding::BiosBoot);
        }
        if self.platform == Some(Platform::Windows) {
            findings.push(Finding::Windows);
        }
        Assessment::new(findings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sizing::profiles::BuiltinProfile;

    fn gen1() -> Profile {
        BuiltinProfile::Gen1.profile().expect("parses")
    }

    const X86: Architecture = Architecture::X86_64;
    const ARM: Architecture = Architecture::Arm64;

    fn input(arch: &Architecture) -> FitInput<'_> {
        FitInput {
            architecture: arch,
            vcpus: 4,
            memory_gib: 16,
            volumes: 1,
            largest_volume_gib: 100,
            instance_store_volumes: 0,
            nics: 1,
            boot_mode: Some(BootMode::from("uefi")),
            platform: Some(Platform::from("Linux/UNIX")),
            bare_metal: false,
            gpus: 0,
            accelerators: 0,
            type_known: true,
        }
    }

    #[test]
    fn x86_linux_within_limits_is_a_clean_fit() {
        let a = input(&X86).assess(&gen1(), Disposition::Rehost);
        assert_eq!(a, Assessment { fit: Fit::CleanFit, findings: Vec::new() });
    }

    #[test]
    fn graviton_is_blocked_under_rehost_and_a_rebuild_under_replatform() {
        let a = input(&ARM).assess(&gen1(), Disposition::Rehost);
        assert_eq!(a.fit, Fit::CannotMove);
        assert_eq!(a.findings, vec![Finding::ArmNeedsReplatform]);
        assert!(a.findings[0].to_string().contains("replatform"));
        let a = input(&ARM).assess(&gen1(), Disposition::Replatform);
        assert_eq!(
            a.fit,
            Fit::NeedsRemediation,
            "a replatform sizes the rebuild"
        );
        assert_eq!(a.findings, vec![Finding::ArmRebuild]);
    }

    #[test]
    fn other_architectures_are_blocked() {
        let arch = Architecture::from("i386");
        let a = input(&arch).assess(&gen1(), Disposition::Replatform);
        assert_eq!(a.fit, Fit::CannotMove);
        assert_eq!(
            a.findings[0].to_string(),
            "i386 instances cannot run on x86-64 sleds"
        );
    }

    #[test]
    fn large_volume_and_bios_need_remediation() {
        let mut i = input(&X86);
        i.largest_volume_gib = 2048;
        i.boot_mode = Some(BootMode::from("legacy-bios"));
        let a = i.assess(&gen1(), Disposition::Rehost);
        assert_eq!(a.fit, Fit::NeedsRemediation);
        assert_eq!(
            a.findings,
            vec![
                Finding::VolumeTooLarge { gib: 2048, max: 1023 },
                Finding::BiosBoot
            ]
        );
    }

    #[test]
    fn blocked_outranks_remediation_and_keeps_both_findings_blocking_first() {
        let mut i = input(&X86);
        i.volumes = 20;
        i.gpus = 1;
        i.platform = Some(Platform::from("Windows with SQL Server Standard"));
        let a = i.assess(&gen1(), Disposition::Rehost);
        assert_eq!(a.fit, Fit::CannotMove);
        assert_eq!(
            a.findings,
            vec![
                Finding::Accelerator,
                Finding::TooManyVolumes { volumes: 20, max: 12 },
                Finding::Windows
            ]
        );
    }

    #[test]
    fn every_remaining_check_raises_its_finding() {
        let mut i = input(&X86);
        i.vcpus = 500;
        i.memory_gib = 2000;
        i.nics = 9;
        i.instance_store_volumes = 1;
        i.bare_metal = true;
        i.type_known = false;
        let a = i.assess(&gen1(), Disposition::Rehost);
        assert_eq!(
            a.findings,
            vec![
                Finding::BareMetal,
                Finding::MemoryOverSled { memory_gib: 2000, dram_gib: 1024 },
                Finding::UnknownType,
                Finding::TooManyVcpus { vcpus: 500, max: 254 },
                Finding::TooMuchMemory { memory_gib: 2000, max: 1536 },
                Finding::TooManyNics { nics: 9, max: 8 },
                Finding::InstanceStore,
            ]
        );
    }

    #[test]
    fn boot_mode_platform_and_architecture_parse_aws_values() {
        assert_eq!(BootMode::from("uefi-preferred"), BootMode::Uefi);
        assert_eq!(BootMode::from("bios"), BootMode::Bios);
        assert_eq!(
            Platform::from("Red Hat Enterprise Linux"),
            Platform::LinuxOrUnix
        );
        assert_eq!(Architecture::from("arm64").to_string(), "arm64");
        let json =
            serde_json::to_value(Architecture::from("x86_64_mac")).unwrap();
        assert_eq!(json, serde_json::json!("x86_64_mac"));
        assert_eq!(
            serde_json::to_value(Finding::TooManyNics { nics: 9, max: 8 })
                .unwrap(),
            serde_json::json!({"finding": "too_many_nics", "nics": 9, "max": 8})
        );
    }
}
