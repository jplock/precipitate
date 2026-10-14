# precip

precip scans Amazon Web Services (AWS) accounts, groups the resources it finds into
workloads, sizes the Oxide sleds and racks those workloads need, and
schedules each migration on a calendar. It ships as one static binary. The
binary has a terminal user interface (TUI) and a command line. Both read
and write the same SQLite file, so every action in one is available in the
other.

The design is in Request for Discussion (RFD) 0001,
[docs/rfd/0001/README.adoc](docs/rfd/0001/README.adoc).

## Build

precip needs the Rust toolchain pinned in `rust-toolchain.toml`. The
Makefile runs the same commands as the pre-commit hooks and continuous
integration (CI).

```
make check          # format check, clippy with warnings as errors, tests, cargo deny
make run ARGS=""    # open the TUI
make release        # optimised build for this machine
make release-musl   # static Linux binaries; needs cargo-zigbuild
make hooks          # install the prek git hooks
```

## Run

The TUI opens with a workspace picker. A workspace holds one company's
assessment in one SQLite file under `$XDG_DATA_HOME/precip/workspaces/`
(`~/.local/share/precip/workspaces/` by default). precip finds AWS
credentials the same way the AWS command line does. `--profile` selects a
named profile.

```
precip --profile mgmt                         # open the TUI
precip --profile mgmt --workspace acme scan   # scan without the TUI
precip --workspace acme workloads             # list what the scan found
precip --workspace acme size                  # count sleds and racks
precip --workspace acme schedule show         # show the migration calendar
precip --workspace acme export schedule --out plan.ics
precip --help                                 # list every command
```

From an AWS Organizations management account, precip scans every active
member account. In each account it assumes `OrganizationAccountAccessRole`,
then `AWSControlTowerExecution` if the first role fails. The read-only
Identity and Access Management (IAM) policy precip needs is in
`iam/precip-scan-policy.json`.

## What precip does

**Scan.** precip makes read-only describe calls, 8 accounts and 8 regions
at a time. It collects:

- compute: Elastic Compute Cloud (EC2) instances and instance types,
  machine images, auto scaling groups, launch templates and
  configurations;
- storage: Elastic Block Store (EBS) volumes;
- networking: Virtual Private Clouds (VPCs), subnets, security groups and
  their rules, route tables, gateways, peering connections, prefix lists,
  load balancers and target groups;
- CloudFormation stacks;
- 7 managed services: RDS, EKS, ECS, Lambda, ElastiCache, DynamoDB and
  EFS.

When AWS Resource Explorer is enabled, precip skips regions with no
workload resources. precip keeps every scan
and versions each resource by its content, so it can list what changed
between two scans. Each scan saves its account selection, and a later
`precip scan` with no flags repeats it.

**Group.** precip assigns each resource to a workload and an environment,
such as `shop` and `prod`. It takes the first signal that matches:

1. a manual override;
2. the CloudFormation stack;
3. the auto scaling group;
4. the load balancer;
5. tags;
6. the resource name;
7. the account;
8. the VPC.

A user can merge two workload names that describe the same workload. Each
workload and environment pair gets one of the 7 AWS migration strategies:
rehost, relocate, replatform, refactor, repurchase, retain or retire.
precip sizes and schedules pairs marked rehost, relocate or replatform.

**Size.** precip follows the 8 steps in RFD 0001. It totals the virtual
CPUs (vCPUs), memory and disk of each instance, multiplies by a growth
factor, and packs instances onto sleds, largest memory first. Each sled
holds up to 75% of its usable capacity by default. precip adds one spare
sled per rack and reports whether compute or storage set the count. It
flags each instance that cannot move unchanged and states the change it
needs, for example "13 volumes exceeds 12 disks per instance;
consolidate".

precip ships 2 sled profiles:

| Profile | Sleds per rack | Source of usable capacity |
|---|---|---|
| gen2-1152 (default) | 24 | Derived from memory (DRAM), the control plane reservation and the guest memory reservoir |
| gen1-1024 | 32 | Published in the Oxide operator guide: 853 GiB memory, 26.6 TiB storage per sled |

Each profile file lists the source of every figure.

**Schedule.** precip places each pair on a calendar. The rules set:

- the maximum migrations per day, week and month;
- the weekdays migrations may run;
- freezes, including selected US federal holidays;
- a target end date;
- waves, which run in order;
- dependencies between pairs; production waits for its other
  environments by default;
- per-pair duration, weight, earliest date and allowed weekdays.

Each pair gets a proposed date and 2 alternatives. When the owners agree
a date, precip never moves it. An agreed date ends in one of 4 ways:

- the migration completes;
- the date passes and the pair is marked missed;
- the owners agree another date;
- a completed migration is rolled back.

precip
refuses an agreed date for a pair whose dependency has not migrated. After
each scan, precip lists any migrated pair that still has resources in AWS.

The schedule reports:

- the reschedule rate, by reason;
- the first-time success rate;
- the rollback rate;
- the projected end date and its drift;
- how far the first plan was from each actual migration date.

A Gantt chart shows every pair on one screen.

**Change log.** precip records every change to the plan as an event: a
rule, a freeze, a wave, a pair setting, a dependency, a disposition, a
merge or a status. Each event produces a new schedule version. The history
lists each change and the end date it produced. `precip schedule replay`
rebuilds the plan from the events alone.

**Export.** precip writes the workloads and the sizing as comma-separated
values (CSV) or JSON, and the schedule as CSV, JSON or an iCalendar feed. In the feed, agreed dates
are confirmed events and proposed dates are tentative.

## Layout

```
crates/precip         the binary: command line and TUI entry point
crates/precip-tui     the TUI (ratatui)
crates/precip-aws     the scanner
crates/precip-db      the SQLite workspace: scans, plan events, projections, export
crates/precip-engine  pure functions: grouping, sizing, scheduling, Gantt layout
crates/precip-types   shared types
iam/                  the IAM policy for scanning
docs/rfd/             the design document
```

## License and trademarks

precip is licensed under the Mozilla Public License 2.0. See `LICENSE`.

Oxide and Oxide Computer are trademarks of Oxide Computer Company. precip
is an independent project. Oxide Computer Company does not sponsor or
endorse it, and precip has no affiliation with the company. The name
appears here only to state what precip sizes for. See `NOTICE`.
