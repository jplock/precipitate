# precip

Scans an AWS estate, groups what it finds into workloads, sizes the Oxide
racks they would need, and keeps a migration calendar for moving them.
One static binary with a terminal interface and a command line; both are
views over the same SQLite workspace, so anything you can do in one you
can do in the other.

The design is in [RFD 0001](docs/rfd/0001/README.adoc).

## Build

Rust stable with the toolchain in `rust-toolchain.toml`. The Makefile
targets are the same commands the pre-commit hooks and CI run.

```
make check          # fmt, clippy -D warnings, tests, cargo deny
make run ARGS=""    # the terminal interface
make release        # optimised build for this machine
make release-musl   # static Linux binaries, needs cargo-zigbuild
make hooks          # install the prek git hooks
```

## Run

The terminal interface opens with a workspace picker. A workspace is one
estate under assessment: a SQLite file under
`$XDG_DATA_HOME/precip/workspaces/` (`~/.local/share/precip/workspaces/`
by default). Credentials come from the usual AWS places; `--profile`
picks a named profile.

```
precip --profile mgmt                      # terminal interface
precip --profile mgmt --workspace acme scan   # headless scan into a workspace
precip --workspace acme workloads          # what the scan found
precip --workspace acme size               # sleds and racks
precip --workspace acme schedule show      # the migration calendar
precip --help                              # everything else
```

From an Organization management account the scan covers every active
member account by assuming `OrganizationAccountAccessRole` or
`AWSControlTowerExecution` in each. The read-only IAM policy it needs is
in `iam/precip-scan-policy.json`.

## What it does

**Scan.** Read-only describe calls across every account and region in
parallel: EC2 instances and their types, volumes, images, VPCs, subnets,
security groups and rules, route tables, gateways, peering, prefix lists,
auto scaling groups, launch templates and configurations, load balancers
and target groups, CloudFormation stacks, and the managed services a
workload leans on (RDS, EKS, ECS, Lambda, ElastiCache, DynamoDB, EFS).
AWS Resource Explorer, when enabled, prunes regions with nothing to see.
Every scan is kept; resources are versioned by content so changes between
scans are visible.

**Group.** Resources become workload and environment pairs from
CloudFormation stacks first, then auto scaling groups, load balancers,
tags, names, and the account they live in. Names that are really the same
workload can be merged. Each pair gets a disposition, one of the AWS
migration strategies: rehost, relocate, replatform, refactor, repurchase,
retain, retire. Rehost, relocate and replatform are sized and scheduled.

**Size.** The algorithm from the RFD: demand per instance, a growth
factor, usable capacity per sled from an Oxide rack profile and a
utilisation threshold, first-fit packing by memory then vCPU, storage
with replication, N+1 headroom, racks. Instances that cannot move as they
are (too many disks, a disk over the cap, arm64 under rehost) are flagged
with what would have to change.

**Schedule.** Pairs are placed on a calendar under the plan's rules: how
many migrations per day, week and month, which days of the week, freezes
and holidays, a target end date, waves that run in order, dependencies,
and per-pair rules. Each pair has a proposed date with alternatives until
the owners agree one; an agreed date never moves. It is either done,
missed, replaced by another agreed date, or rolled back. A scan that still
sees a migrated pair's servers is flagged. Metrics cover the reschedule
rate, first-time success, rollbacks, projected end and its drift.

**Change log.** Every change to the plan is an appended event: a rule, a
freeze, a wave, a pair's settings, a dependency, a disposition, a merge,
a status. The tables are projections of the log and every event produces
a new schedule version, so the history shows what changed and what it did
to the end date. `precip schedule replay` rebuilds everything from the log.

## Layout

```
crates/precip         the binary: command line and terminal interface entry
crates/precip-tui     the terminal interface (ratatui)
crates/precip-aws     the scanner
crates/precip-db      the SQLite workspace: scans, projections, event log
crates/precip-engine  pure functions: grouping, sizing, scheduling
crates/precip-types   shared types
iam/                  the IAM policy for scanning
docs/rfd/             the design document
```

## License and trademarks

precip is licensed under the Mozilla Public License 2.0; see `LICENSE`.

Oxide and Oxide Computer are trademarks of Oxide Computer Company. precip
is an independent project, not affiliated with, sponsored by, or endorsed
by Oxide Computer Company. The name appears here only to say what the
tool sizes for. See `NOTICE`.
