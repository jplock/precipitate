# precip

precip scans AWS, Azure, Google Cloud and Oracle Cloud Infrastructure
(OCI) estates, groups the resources into workloads, sizes the Oxide sleds
and racks they need, and schedules each migration. It is one static
binary with a terminal user interface (TUI) and a command line. Both use
the same SQLite file.

The design is in RFD 0001, [docs/rfd/0001/README.adoc](docs/rfd/0001/README.adoc).

## Build

precip needs the Rust toolchain pinned in `rust-toolchain.toml`. The
Makefile runs the same checks as the pre-commit hooks and CI.

```
make check          # format, clippy, tests, cargo deny
make run ARGS=""    # open the TUI
make release        # optimised build for this machine
make release-musl   # static Linux binaries; needs cargo-zigbuild
make hooks          # install the prek git hooks
```

To release, set `version` in the workspace `Cargo.toml`, merge, and push
the matching tag (`v0.2.0`). `.github/workflows/release.yml` publishes
static Linux (amd64, arm64), macOS (arm64) and Windows (amd64, arm64)
binaries, signed where the secrets are set, with `SHA256SUMS`.

## Run

A workspace is one SQLite file under `$XDG_DATA_HOME/precip/workspaces/`
and can hold all four clouds. The TUI opens with a workspace picker.

```
precip --aws-profile mgmt                        # open the TUI
precip --aws-profile mgmt --workspace acme scan  # scan without the TUI
precip --workspace acme --cloud azure scan
precip --workspace acme scan --gcp-organization 123456789
precip --workspace acme workloads                # list the pairs
precip --workspace acme size                     # count sleds and racks
precip --workspace acme schedule show            # the migration calendar
precip --workspace acme export terraform --workload shop --environment prod
precip --help                                    # every command
```

`--cloud` picks the cloud; any cloud's flags imply it. `scan` with no
flags repeats every cloud's saved selection.

## Clouds

| Cloud | Scans by default | Narrow with | Signs in with | Permissions |
|---|---|---|---|---|
| AWS | the caller's account, or every active account from an Organization management account | `--aws-account`, `--aws-exclude-account`, `--aws-single-account` | the AWS CLI chain; `--aws-profile` | `iam/precip-scan-policy.json` |
| Azure | every readable subscription | `--azure-subscription`, `--azure-exclude-subscription`, `--azure-tenant`, `--azure-management-group` | a service principal, workload identity, or the Azure CLI | Reader, or `iam/azure-precip-reader.json` |
| Google Cloud | every active project | `--gcp-project`, `--gcp-exclude-project`, `--gcp-organization`, `--gcp-folder` | Application Default Credentials | `iam/gcp-precip-reader.yaml` |
| OCI | every readable compartment in every subscribed region | `--oci-compartment`, `--oci-exclude-compartment`, `--oci-region` | an OCI CLI profile; `--oci-profile` | `iam/oci-precip-policy.txt` |

Accounts, subscriptions, projects and compartments can be named by id or
name.

- AWS: member accounts are read through `OrganizationAccountAccessRole`,
  then `AWSControlTowerExecution`. The region picks the partition;
  `us-gov-west-1` scans GovCloud.
- Azure: `--azure-cloud AzureUSGovernment` scans Azure Government. Run
  `az cloud set` to match first.
- Google Cloud: calls bill a quota project that has the Cloud Asset API
  enabled (`GOOGLE_CLOUD_QUOTA_PROJECT`). An organization or folder is
  read in one pass.
- OCI: API keys and `oci session authenticate` sessions work; encrypted
  keys do not.

## What precip does

**Scan.** Every call is a read.

| | AWS | Azure | Google Cloud | OCI |
|---|---|---|---|---|
| Source | describe calls, 8 accounts and 8 regions at a time | Resource Graph, 1,000 subscriptions per query | Cloud Asset Inventory, 4 projects at a time | REST, 8 compartments and 4 regions at a time |
| Regions | each enabled region; Resource Explorer skips empty ones | all in one query | all in one listing | each subscribed region; Resource Search skips empty compartments |
| Compute | instances, types, images, scale groups, launch templates | VMs, scale sets, sizes, images | instances, machine types, images, managed instance groups | instances, shapes, images |
| Storage | EBS volumes | managed disks | persistent disks | boot and block volumes |
| Network | VPCs, subnets, security groups, routes, gateways, peering, prefix lists, Elastic IPs, load balancers | VNets, subnets, NSGs, routes, NAT, peering, IP groups, public IPs, load balancers | VPCs, subnets, firewalls, routes, Cloud NAT, peering, static IPs, backend services | VCNs, subnets, security lists, NSGs, routes, gateways, reserved IPs |
| Deployment groups | CloudFormation stacks | resource groups | GKE clusters | none |
| Managed services | 7 | 11 | 9 | none yet |

Every scan is kept, and each resource is versioned by content. A scan of
one cloud leaves the others as they were.

**Group.** Each resource gets a workload and an environment, such as
`shop` and `prod`, from the first signal that matches:

1. a manual override;
2. the deployment group;
3. the scale group;
4. the load balancer;
5. tags;
6. the resource name;
7. the account;
8. the network.

Folding one workload name into another moves its resources and keeps
their environments. Each pair gets one of the 7 migration strategies;
rehost, relocate and replatform are sized and scheduled.

**Size.** precip follows the 8 steps in RFD 0001: it totals vCPUs, memory
and disk, applies a growth factor, and packs instances onto sleds, largest
memory first, to 75% of usable capacity by default, with one spare sled
per rack. It names each instance that needs a change to move, such as
"13 volumes exceeds 12 disks per instance; consolidate".

| Profile | Sleds per rack | Usable capacity from |
|---|---|---|
| gen2-1152 (default) | 24 | DRAM, the control plane reservation and the guest memory reservoir |
| gen1-1024 | 32 | the Oxide operator guide: 853 GiB memory, 26.6 TiB storage per sled |

Each profile file cites its figures.

**Schedule.** Rules set migrations per day, week and month; allowed
weekdays; freezes, including US federal holidays; a target end date;
waves; dependencies, with production after its other environments; and
per-pair duration, weight and earliest date. Each pair gets a proposed
date and 2 alternatives. An agreed date never moves; it ends as done,
missed, re-agreed or rolled back. After each scan, precip lists migrated
pairs whose resources remain. The schedule reports reschedule rate,
first-time success, rollbacks, projected end and drift, and has a Gantt
chart.

**Change log.** Every plan change is an event, and each event produces a
schedule version. `precip schedule replay` rebuilds the plan from the
events alone.

**Export.** Workloads and sizing as CSV or JSON; the schedule as CSV,
JSON or iCalendar. `export terraform` writes one pair for the Oxide
provider: project, VPCs, subnets, firewall rules, custom routers, disks,
floating IPs and stopped instances. What does not translate is listed at
the top of the file.

## Layout

```
crates/precip          the binary: command line and TUI entry point
crates/precip-tui      the TUI (ratatui)
crates/precip-aws      the AWS scanner
crates/precip-azure    the Azure scanner
crates/precip-gcp      the Google Cloud scanner
crates/precip-oci      the OCI scanner
crates/precip-collect  what the scanners share: collector runner, HTTP client, region loop
crates/precip-db       the SQLite workspace: scans, plan events, projections, export
crates/precip-engine   pure functions: grouping, sizing, scheduling, Gantt, Terraform
crates/precip-types    shared types
iam/                   the permissions each cloud's scan needs
docs/rfd/              the design document
```

## License and trademarks

precip is licensed under the Mozilla Public License 2.0. See `LICENSE`.

Oxide and Oxide Computer are trademarks of Oxide Computer Company. precip
is an independent project. Oxide Computer Company does not sponsor or
endorse it, and precip has no affiliation with the company. The name
appears here only to state what precip sizes for. See `NOTICE`.
