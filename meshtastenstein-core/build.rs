fn main() {
    compile_protobufs();
}

fn compile_protobufs() {
    let proto_dir = "../proto/meshtastic-protobufs/meshtastic";

    let protos: Vec<String> = std::fs::read_dir(proto_dir)
        .unwrap_or_else(|e| panic!("Failed to read proto dir {}: {}", proto_dir, e))
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("proto") {
                Some(path.to_string_lossy().into_owned())
            } else {
                None
            }
        })
        .collect();

    if protos.is_empty() {
        panic!("No .proto files found in {}", proto_dir);
    }

    eprintln!("Compiling {} proto files from {}", protos.len(), proto_dir);

    let mut config = prost_build::Config::new();
    config.out_dir("src/proto");
    // Use BTreeMap instead of HashMap (no_std compatible)
    config.btree_map(["."]);
    // prost already derives Clone, so don't add it again

    // Upstream 2.8.0 added `AS3935_config` to admin.proto and `AS3935Config` to
    // telemetry.proto. Both are in package `meshtastic`, and prost normalizes both
    // to the Rust identifier `As3935Config` in one flat module — it cannot emit the
    // name twice. Neither type is used by this firmware (lightning-sensor config),
    // so redirect the admin one to a hand-written stub in `src/proto/mod.rs` and let
    // telemetry's keep the generated name.
    config.extern_path(
        ".meshtastic.AS3935_config",
        "crate::proto::As3935AdminConfig",
    );

    config
        .compile_protos(&protos, &["../proto/meshtastic-protobufs"])
        .unwrap_or_else(|e| panic!("Failed to compile protobufs: {}", e));

    println!("cargo:rerun-if-changed=../proto/meshtastic-protobufs");
}
