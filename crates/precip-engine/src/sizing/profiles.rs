//! Profiles that ship inside the binary. A user can point at their own
//! file instead; these are the defaults per hardware generation.

use precip_types::Profile;
use precip_types::profile::ProfileError;

precip_types::named_enum! {
    /// One profile per sled generation Oxide sells, current one first.
    /// Each file says where every figure comes from.
    pub enum BuiltinProfile {
        Gen2 = "gen2-1152",
        Gen1 = "gen1-1024",
    }
}

impl BuiltinProfile {
    /// The profile sizing uses when nothing else is chosen.
    pub const DEFAULT: Self = Self::Gen2;

    fn text(self) -> &'static str {
        match self {
            Self::Gen2 => include_str!("../../profiles/gen2-1152.toml"),
            Self::Gen1 => include_str!("../../profiles/gen1-1024.toml"),
        }
    }

    pub fn profile(self) -> Result<Profile, ProfileError> {
        Profile::from_toml(self.text())
    }

    /// The profile after this one, wrapping around.
    pub fn next(self) -> Self {
        let i = Self::ALL.iter().position(|p| *p == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_profile_parses_under_its_own_name() {
        for p in BuiltinProfile::ALL {
            assert_eq!(p.profile().expect("parses").name, p.as_str());
        }
    }

    #[test]
    fn next_cycles_through_every_profile() {
        assert_eq!(BuiltinProfile::Gen2.next(), BuiltinProfile::Gen1);
        assert_eq!(BuiltinProfile::Gen1.next(), BuiltinProfile::Gen2);
        assert_eq!(BuiltinProfile::DEFAULT, BuiltinProfile::ALL[0]);
    }
}
