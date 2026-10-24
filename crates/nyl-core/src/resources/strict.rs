//! Strict field handling for structs that flatten shared field groups.
//!
//! Serde cannot combine `deny_unknown_fields` with `flatten`, yet resources
//! reject unknown fields so that a typo never passes silently. Structs that
//! flatten a shared group, such as [`GitSource`](super::GitSource) or the
//! common unit fields, end with a flattened [`DenyUnknownFields`] instead.

use std::fmt;

use serde::de::{Deserializer, MapAccess, Visitor};
use serde::{Deserialize, Serialize};

/// Rejects fields that no other field of the containing struct consumed.
///
/// A struct that flattens a field group declares this as its last field,
/// flattened too: serde hands it exactly the entries the other fields left, and any
/// entry fails deserialization with serde's own "unknown field" message.
/// Mark it `#[schemars(skip)]` and close the schema with
/// [`deny_additional_properties`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DenyUnknownFields;

impl Serialize for DenyUnknownFields {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        serializer.serialize_map(Some(0))?.end()
    }
}

impl<'de> Deserialize<'de> for DenyUnknownFields {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct NoEntries;
        impl<'de> Visitor<'de> for NoEntries {
            type Value = DenyUnknownFields;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("no further fields")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
                if let Some(field) = map.next_key::<String>()? {
                    return Err(serde::de::Error::custom(format_args!("unknown field `{field}`")));
                }
                Ok(DenyUnknownFields)
            }
        }
        deserializer.deserialize_map(NoEntries)
    }
}

/// Close a flattened schema the way `deny_unknown_fields` would.
pub(crate) fn deny_additional_properties(schema: &mut schemars::Schema) {
    schema.insert("additionalProperties".into(), serde_json::Value::Bool(false));
}
