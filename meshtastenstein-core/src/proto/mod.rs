//! Re-exports for generated protobuf code
//!
//! The proto files are compiled by prost-build in build.rs and output to this directory.
//! Include them here for the rest of the crate to use.

// Include the generated protobuf modules
// prost-build generates files named like "meshtastic.rs" or "meshtastic.mesh.rs"
// We include all generated files

#[allow(clippy::all)]
#[allow(warnings)]
mod generated {
    include!("meshtastic.rs");
}

/// Stand-in for admin.proto's `AS3935_config`, which collides with
/// telemetry.proto's `AS3935Config` under prost's flat package module (both
/// normalize to `As3935Config`). Unused by this firmware; present only so
/// codegen resolves — see the `extern_path` redirect in `build.rs`.
#[derive(Clone, Copy, PartialEq, ::prost::Message)]
pub struct As3935AdminConfig {
    #[prost(uint32, optional, tag = "1")]
    pub set_tuning_cap_pf: ::core::option::Option<u32>,
}

pub use generated::*;
