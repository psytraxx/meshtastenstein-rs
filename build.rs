fn main() {
    compile_protobufs();
    linker_be_nice();
    println!("cargo:rustc-link-arg=-Tlinkall.x");
}

fn compile_protobufs() {
    let proto_dir = "proto/meshtastic-protobufs/meshtastic";

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
        .compile_protos(&protos, &["proto/meshtastic-protobufs"])
        .unwrap_or_else(|e| panic!("Failed to compile protobufs: {}", e));

    println!("cargo:rerun-if-changed=proto/meshtastic-protobufs");
}

fn linker_be_nice() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        let kind = &args[1];
        let what = &args[2];

        match kind.as_str() {
            "undefined-symbol" => match what.as_str() {
                what if what.starts_with("_defmt_") => {
                    eprintln!();
                    eprintln!(
                        "defmt not found - make sure defmt.x is added as a linker script and you have included use defmt_rtt as _;"
                    );
                    eprintln!();
                }
                "_stack_start" => {
                    eprintln!();
                    eprintln!("Is the linker script linkall.x missing?");
                    eprintln!();
                }
                what if what.starts_with("esp_rtos_") => {
                    eprintln!();
                    eprintln!(
                        "esp-radio has no scheduler enabled. Make sure you have initialized esp-rtos or provided an external scheduler."
                    );
                    eprintln!();
                }
                "embedded_test_linker_file_not_added_to_rustflags" => {
                    eprintln!();
                    eprintln!(
                        "embedded-test not found - make sure embedded-test.x is added as a linker script for tests"
                    );
                    eprintln!();
                }
                "free"
                | "malloc"
                | "calloc"
                | "get_free_internal_heap_size"
                | "malloc_internal"
                | "realloc_internal"
                | "calloc_internal"
                | "free_internal" => {
                    eprintln!();
                    eprintln!(
                        "Did you forget the esp-alloc dependency or didn't enable the compat feature on it?"
                    );
                    eprintln!();
                }
                _ => (),
            },
            _ => {
                std::process::exit(1);
            }
        }

        std::process::exit(0);
    }

    println!(
        "cargo:rustc-link-arg=-Wl,--error-handling-script={}",
        std::env::current_exe().unwrap().display()
    );
}
