//! Profiles that ship inside the binary. A user can point at their own
//! file instead; these are the defaults per hardware generation.

use precip_types::Profile;
use precip_types::profile::ProfileError;

const GEN2_1152: &str = include_str!("../../profiles/gen2-1152.toml");

/// Only profiles whose figures are backed by public Oxide sources ship
/// here; see the comments in each file. Others go in a TOML file the
/// user points at with `--profile`.
pub const NAMES: &[&str] = &["gen2-1152"];

pub fn builtin(name: &str) -> Option<Result<Profile, ProfileError>> {
    let text = match name {
        "gen2-1152" => GEN2_1152,
        _ => return None,
    };
    Some(Profile::from_toml(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_profile_parses() {
        for name in NAMES {
            builtin(name).expect("listed").expect("parses");
        }
    }
}
