use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is always set by cargo"),
    );

    // client/crates/vorcall-proto -> client/crates -> client -> repository root
    let proto_dir = manifest_dir
        .join("../../../proto")
        .canonicalize()
        .unwrap_or_else(|e| {
            panic!(
                "cannot locate the shared proto directory at {}: {e}",
                manifest_dir.join("../../../proto").display()
            )
        });
    let proto_file = proto_dir.join("vorcall.proto");

    println!("cargo:rerun-if-changed={}", proto_file.display());

    let file_descriptors = protox::compile([&proto_file], [&proto_dir])
        .unwrap_or_else(|e| panic!("failed to compile {}: {e}", proto_file.display()));

    prost_build::Config::new()
        .compile_fds(file_descriptors)
        .unwrap_or_else(|e| panic!("failed to generate Rust code from vorcall.proto: {e}"));
}
