fn main() {
    let target = std::env::var("TARGET").expect("Cargo must provide TARGET to build.rs");
    println!("cargo:rustc-env=SKIPPR_BUILD_TARGET_TRIPLE={target}");
    println!("cargo:rerun-if-changed=proto/cluster.proto");
    prost_build::Config::new()
        .compile_protos(&["proto/cluster.proto"], &["proto"])
        .expect("compile cluster.proto");
}
