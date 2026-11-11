//! Client-side view of the server module.
//!
//! Everything under `module_bindings/` is generated. Do not edit it; after
//! changing tables or reducers in `server/`, run `make bindings`.

mod module_bindings;

pub use module_bindings::*;
pub use spacetimedb_sdk as sdk;
