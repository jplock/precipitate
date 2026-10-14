//! Profiles that ship inside the binary. A user can point at their own
//! file instead; these are the defaults per hardware generation.

use precip_types::Profile;
use precip_types::profile::ProfileError;

const GEN1_1024: &str = include_str!("../../profiles/gen1-1024.toml");
const GEN2_1152: &str = include_str!("../../profiles/gen2-1152.toml");

/// One profile per sled generation Oxide sells, current one first. Each
/// file says where every figure comes from.
pub const NAMES: &[&str] = &["gen2-1152", "gen1-1024"];

pub fn builtin(name: &str) -> Option<Result<Profile, ProfileError>> {
    let text = match name {
        "gen1-1024" => GEN1_1024,
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
