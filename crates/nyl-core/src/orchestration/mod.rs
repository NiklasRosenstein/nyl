//! Orchestration types and traits: identifiers, state documents, the driver
//! interface, and the ports through which the orchestration shell performs
//! effects.
//!
//! Contract: [Orchestration core](../../../../design/orchestration-core.md).
//! This module holds data and trait definitions only; the rules live in
//! `nyl-orchestration`, the Git state store in `nyl-state`, and the drivers
//! in `nyl-drivers`.

/// `Serialize` through `Display`, `Deserialize` through `FromStr`, and the
/// JSON Schema `$schema`, for a type that is written as one string.
macro_rules! string_serde {
    ($name:ident, $schema:tt) => {
        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
                <String as serde::Deserialize>::deserialize(deserializer)?
                    .parse()
                    .map_err(serde::de::Error::custom)
            }
        }

        impl schemars::JsonSchema for $name {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                stringify!($name).into()
            }

            fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
                schemars::json_schema!($schema)
            }
        }
    };
}

pub mod codes;
pub mod driver;
pub mod graph;
pub mod ids;
pub mod ports;
pub mod state;
pub mod time;
