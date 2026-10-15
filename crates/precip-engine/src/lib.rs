//! The engine behind precip. Pure functions over the types in
//! `precip-types`: no I/O, no clock, no database, so every screen and
//! command is a view of the same computation and every piece is testable
//! with plain values.
//!
//! - [`group`] turns scanned resources into workload and environment
//!   pairs.
//! - [`sizing`] packs the pairs that migrate onto sleds and racks.
//! - [`schedule`] places them on a calendar under the plan's rules.
//! - [`terraform`] writes a pair's Oxide Terraform.

pub mod group;
pub mod schedule;
pub mod sizing;
pub mod terraform;
