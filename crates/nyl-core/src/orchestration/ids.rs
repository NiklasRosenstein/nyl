//! Typed identifiers of orchestration.
//!
//! Every identifier that state records or decisions compare is its own type,
//! so an environment name can never stand in for a unit name or a commit for
//! a run ID. See the implementation architecture's "Types that rule out
//! illegal states" (`design/implementation-architecture.md`). Each type
//! serializes as its string form and validates it on deserialization.

use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use crate::{CoreError, Result};

macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident, $validate:expr) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Validate and wrap `value`.
            pub fn new(value: impl Into<String>) -> Result<Self> {
                let value = value.into();
                let validate: fn(&str) -> Result<()> = $validate;
                validate(&value)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = CoreError;

            fn from_str(value: &str) -> Result<Self> {
                Self::new(value)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

string_id!(
    /// An Environment's name, unique across declared Environments and all
    /// template instances because leases, run refs, and keep refs are named
    /// after it. A Kubernetes DNS label.
    EnvironmentName,
    |value| validate_dns_label("environment name", value)
);

string_id!(
    /// A unit's literal `metadata.name`, unique per environment across all
    /// unit kinds. A Kubernetes DNS label.
    UnitName,
    |value| validate_dns_label("unit name", value)
);

string_id!(
    /// The UUIDv7 of one incarnation of a unit. It fences every receipt,
    /// checkpoint, and deletion.
    Uid,
    |value| validate_uuid("uid", value)
);

string_id!(
    /// The UUID of one run, recorded in the `Nyl-Run-Id` trailer.
    RunId,
    |value| validate_uuid("run ID", value)
);

string_id!(
    /// A full, lowercase, 40-character Git commit ID.
    CommitId,
    |value| crate::resources::validate_immutable_git_commit("commit", value)
);

string_id!(
    /// A digest of everything that decides what a unit's execution does:
    /// `sha256:` and 64 lowercase hex digits.
    ExecutionKey,
    |value| validate_sha256("execution key", value)
);

string_id!(
    /// A digest of the changes a plan would apply, which `bind: plan`
    /// approvals authorize: `sha256:` and 64 lowercase hex digits.
    ChangeDigest,
    |value| validate_sha256("change digest", value)
);

string_id!(
    /// An RFC 6901 JSON Pointer, empty for the whole document.
    JsonPointer,
    |value| crate::json_pointer::validate("pointer", value)
);

impl ExecutionKey {
    /// The key whose digest is the SHA-256 of `bytes`.
    pub fn from_digest_of(bytes: &[u8]) -> Self {
        Self(crate::digest::sha256_prefixed(bytes))
    }
}

impl ChangeDigest {
    /// The digest whose value is the SHA-256 of `bytes`.
    pub fn from_digest_of(bytes: &[u8]) -> Self {
        Self(crate::digest::sha256_prefixed(bytes))
    }
}

/// A unit's address, `<environment>/<unit>`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UnitAddress {
    pub environment: EnvironmentName,
    pub unit: UnitName,
}

impl UnitAddress {
    pub fn new(environment: EnvironmentName, unit: UnitName) -> Self {
        Self { environment, unit }
    }
}

impl fmt::Display for UnitAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.environment, self.unit)
    }
}

impl FromStr for UnitAddress {
    type Err = CoreError;

    fn from_str(value: &str) -> Result<Self> {
        let (environment, unit) = value.split_once('/').ok_or_else(|| {
            CoreError::config(format!(
                "unit address {value:?} must have the form <environment>/<unit>"
            ))
        })?;
        Ok(Self::new(environment.parse()?, unit.parse()?))
    }
}

string_serde!(UnitAddress, {"type": "string", "description": "`<environment>/<unit>`"});

fn validate_dns_label(what: &str, value: &str) -> Result<()> {
    let valid = !value.is_empty()
        && value.len() <= 63
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && value.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        && value.as_bytes().last().is_some_and(u8::is_ascii_alphanumeric);
    if valid {
        Ok(())
    } else {
        Err(CoreError::config(format!(
            "{what} {value:?} must be a Kubernetes DNS label: at most 63 lowercase letters, digits, and '-', starting and ending with a letter or digit"
        )))
    }
}

fn validate_uuid(what: &str, value: &str) -> Result<()> {
    let groups = value.split('-').map(str::len).collect::<Vec<_>>();
    let valid = groups == [8, 4, 4, 4, 12]
        && value
            .bytes()
            .all(|byte| byte == b'-' || byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if valid {
        Ok(())
    } else {
        Err(CoreError::config(format!("{what} {value:?} must be a lowercase UUID")))
    }
}

fn validate_sha256(what: &str, value: &str) -> Result<()> {
    let valid = value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    });
    if valid {
        Ok(())
    } else {
        Err(CoreError::config(format!(
            "{what} {value:?} must be sha256: followed by 64 lowercase hexadecimal digits"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_identifiers_reject_values_state_cannot_name() {
        assert!(EnvironmentName::new("dev").is_ok());
        // Leases and run refs are named after environments; a slash would nest refs.
        assert!(EnvironmentName::new("dev/a").is_err());
        assert!(UnitName::new("Web").is_err());
        assert!(Uid::new("0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90").is_ok());
        assert!(Uid::new("not-a-uuid").is_err());
        assert!(CommitId::new("a".repeat(40)).is_ok());
        assert!(CommitId::new("A".repeat(40)).is_err());
        assert!(ExecutionKey::new(format!("sha256:{}", "0".repeat(64))).is_ok());
        assert!(ExecutionKey::new("0".repeat(64)).is_err());
    }

    #[test]
    fn test_unit_address_round_trips_through_its_string_form() {
        let address: UnitAddress = "dev/database".parse().unwrap();
        assert_eq!(address.to_string(), "dev/database");
        assert_eq!(serde_json::to_value(&address).unwrap(), "dev/database");
        assert!("database".parse::<UnitAddress>().is_err());
    }

    #[test]
    fn test_deserialization_validates_identifiers() {
        assert!(serde_json::from_value::<UnitName>(serde_json::json!("Bad Name")).is_err());
        assert_eq!(
            serde_json::from_value::<UnitName>(serde_json::json!("network")).unwrap(),
            UnitName::new("network").unwrap()
        );
    }
}
