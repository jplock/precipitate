//! Names for things precip reads from a cloud provider or the user, as their own types
//! so a workload name cannot be passed where an account id belongs. Each
//! serializes as a plain string, so stored data does not change.

use serde::{Deserialize, Serialize};
use std::borrow::Borrow;
use std::fmt;

/// Generate an enum with a fixed string name per variant, plus `ALL`,
/// `as_str`, `Display`, `FromStr` and the serde names, all from one list
/// so the stored form and the displayed form cannot drift apart.
#[macro_export]
macro_rules! named_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $(
                $(#[$vmeta:meta])*
                $variant:ident = $text:literal
            ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(
            Debug,
            Clone,
            Copy,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            ::serde::Serialize,
            ::serde::Deserialize,
        )]
        $vis enum $name {
            $(
                $(#[$vmeta])*
                #[serde(rename = $text)]
                $variant,
            )+
        }

        impl $name {
            /// Every variant, in declaration order.
            $vis const ALL: &'static [Self] = &[$(Self::$variant),+];

            /// The name stored in the database and shown to the user.
            $vis fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.pad(self.as_str())
            }
        }

        impl ::std::str::FromStr for $name {
            type Err = $crate::UnknownName;

            fn from_str(s: &str) -> ::std::result::Result<Self, Self::Err> {
                Self::ALL
                    .iter()
                    .copied()
                    .find(|v| v.as_str() == s)
                    .ok_or_else(|| $crate::UnknownName {
                        kind: stringify!($name),
                        value: s.to_string(),
                    })
            }
        }
    };
}

/// A string that names no variant of the enum it was parsed as.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{value:?} is not a known {kind}")]
pub struct UnknownName {
    pub kind: &'static str,
    pub value: String,
}

/// Generate a string newtype that serializes transparently.
macro_rules! string_name {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Debug,
            Clone,
            Default,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            Serialize,
            Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.pad(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_string())
            }
        }

        impl std::str::FromStr for $name {
            type Err = std::convert::Infallible;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(s.to_string()))
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl PartialEq<str> for $name {
            fn eq(&self, other: &str) -> bool {
                self.0 == other
            }
        }

        impl PartialEq<&str> for $name {
            fn eq(&self, other: &&str) -> bool {
                self.0 == *other
            }
        }
    };
}

string_name!(
    /// A workload's name, such as `shop`.
    WorkloadName
);
string_name!(
    /// An environment's name, such as `prod`.
    EnvName
);
string_name!(
    /// A 12-digit AWS account id, an Azure subscription id, a Google
    /// Cloud project id or an OCI compartment OCID.
    AccountId
);
string_name!(
    /// A region, such as `us-east-1` or `eastus`, or `global` for resources that
    /// belong to no region.
    Region
);
string_name!(
    /// A resource's unique id: an Amazon Resource Name, a lowercased
    /// Azure resource id, a Google Cloud full resource name or an OCI
    /// OCID.
    Arn
);

impl Region {
    /// The region of resources that belong to no region, such as accounts.
    pub fn global() -> Self {
        Self::from("global")
    }

    /// The marker for a target that covers every region of an account,
    /// such as an account that could not be scanned at all.
    pub fn every() -> Self {
        Self::from("*")
    }
}

impl EnvName {
    /// The production environment's canonical name.
    pub(crate) const PRODUCTION: &'static str = "prod";

    pub fn is_production(&self) -> bool {
        self.0 == Self::PRODUCTION
    }
}

/// One workload in one environment: the unit that gets a disposition, a
/// size and a migration date.
#[derive(
    Debug,
    Clone,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
)]
pub struct PairKey {
    pub workload: WorkloadName,
    pub environment: EnvName,
}

impl PairKey {
    pub fn new(
        workload: impl Into<WorkloadName>,
        environment: impl Into<EnvName>,
    ) -> Self {
        Self { workload: workload.into(), environment: environment.into() }
    }

    /// Whether this pair is the named workload and environment.
    pub fn is(&self, workload: &str, environment: &str) -> bool {
        self.workload == workload && self.environment == environment
    }
}

impl fmt::Display for PairKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} / {}", self.workload, self.environment)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    named_enum! {
        enum Colour {
            /// Docs carry through.
            Red = "red",
            DarkBlue = "dark_blue",
        }
    }

    #[test]
    fn named_enums_round_trip_through_every_form() {
        assert_eq!(Colour::ALL, &[Colour::Red, Colour::DarkBlue]);
        assert_eq!(Colour::DarkBlue.as_str(), "dark_blue");
        assert_eq!(Colour::DarkBlue.to_string(), "dark_blue");
        assert_eq!("dark_blue".parse::<Colour>(), Ok(Colour::DarkBlue));
        assert_eq!(
            serde_json::to_value(Colour::DarkBlue).unwrap(),
            serde_json::json!("dark_blue")
        );
        let err = "green".parse::<Colour>().unwrap_err();
        assert_eq!(err.to_string(), "\"green\" is not a known Colour");
    }

    #[test]
    fn names_serialize_as_plain_strings() {
        let w = WorkloadName::new("shop");
        assert_eq!(
            serde_json::to_value(&w).unwrap(),
            serde_json::json!("shop")
        );
        assert_eq!(w, "shop");
        assert_eq!(format!("[{:<6}]", w), "[shop  ]", "width flags apply");
        assert_eq!(format!("[{:>8}]", Colour::Red), "[     red]");
        let pair = PairKey::new("shop", "prod");
        assert_eq!(pair.to_string(), "shop / prod");
        assert!(pair.environment.is_production());
        assert!(pair.is("shop", "prod"));
        let map: std::collections::HashMap<Arn, u8> =
            [(Arn::new("arn:x"), 1)].into();
        assert_eq!(map.get("arn:x"), Some(&1), "Borrow<str> lookups work");
    }
}
