//! Which EnvironmentGroup supplies an environment's shared defaults.
//!
//! The rule lives in nyl-core next to the types, so unit rendering (in
//! nyl-render) and the shell share one owner; see
//! [`nyl_core::resources::environment::sharing_group`] and
//! [`nyl_core::resources::environment::effective_environment`].

pub use nyl_core::resources::environment::{effective_environment, sharing_group};
