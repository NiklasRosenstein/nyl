//! Finding references without resolving them.
//!
//! Contract: [References](../../../../design/orchestration-core.md#references):
//! "References add edges implicitly", and the edge is known after rendering.
//! The dependency graph needs every `fromUnit` reference of a rendered spec
//! before any producer has a value, so this walk shares the reference-object
//! rule of [`super::resolve_document`] (an object whose only field is
//! `fromUnit` or `fromPromotion`, at any depth) but reads no provider.

use serde_json::Value;

use super::UnitInputSource;

/// One `fromUnit` reference object of a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitReference {
    /// JSON Pointer of the reference object, below the pointer the caller
    /// passes, such as `/variables/vpc_id`.
    pub pointer: String,
    pub source: UnitInputSource,
}

/// Every `fromUnit` reference object in `document`, at any depth of its
/// objects and arrays, with `pointer` locating `document` itself. A
/// malformed reference object (unknown fields, both `output` and `artifact`)
/// is an issue naming its pointer; `fromPromotion` references are skipped.
pub fn unit_references(pointer: &str, document: &Value) -> Result<Vec<UnitReference>, Vec<String>> {
    let _ = (pointer, document);
    todo!("references-graph")
}
