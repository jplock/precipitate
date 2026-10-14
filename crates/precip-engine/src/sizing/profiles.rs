//! Profiles that ship inside the binary. A user can point at their own
//! file instead; these are the defaults per hardware generation.

use precip_types::Profile;
use precip_types::profile::ProfileError;

const GEN1_1024: &str = include_str!("../../profiles/gen1-1024.toml");

/// Only profiles whose every figure is on docs.oxide.computer or in
/// Omicron on GitHub ship here; the file says where each comes from.
/// `profiles/examples/` has others to point `--profile` at once their
/// figures are confirmed.
pub const NAMES: &[&str] = &["gen1-1024"];

pub fn builtin(name: &str) -> Option<Result<Profile, ProfileError>> {
    let text = match name {
        "gen1-1024" => GEN1_1024,
        _ => return None,
    };
    Some(Profile::from_toml(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_example_profile_parses() {
        Profile::from_toml(include_str!(
            "../../profiles/examples/gen2-1152.toml"
        ))
        .expect("parses");
    }

    #[test]
    fn every_builtin_profile_parses() {
        for name in NAMES {
            builtin(name).expect("listed").expect("parses");
        }
    }
}
