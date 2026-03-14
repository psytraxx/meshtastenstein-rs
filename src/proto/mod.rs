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

pub use generated::*;
