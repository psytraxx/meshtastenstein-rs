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

    config
        .compile_protos(&protos, &["../proto/meshtastic-protobufs"])
        .unwrap_or_else(|e| panic!("Failed to compile protobufs: {}", e));

    println!("cargo:rerun-if-changed=../proto/meshtastic-protobufs");
}
