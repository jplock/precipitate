//! Profiles that ship inside the binary. A user can point at their own
//! file instead; these are the defaults per hardware generation.

use precip_types::Profile;
use precip_types::profile::ProfileError;

const GEN2_768: &str = include_str!("../../profiles/gen2-768.toml");
const GEN2_1152: &str = include_str!("../../profiles/gen2-1152.toml");
const GEN2_1536: &str = include_str!("../../profiles/gen2-1536.toml");

/// Gen 2 sleds by DRAM option, smallest first.
pub const NAMES: &[&str] = &["gen2-768", "gen2-1152", "gen2-1536"];

pub fn builtin(name: &str) -> Option<Result<Profile, ProfileError>> {
    let text = match name {
        "gen2-768" => GEN2_768,
        "gen2-1152" => GEN2_1152,
        "gen2-1536" => GEN2_1536,
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
