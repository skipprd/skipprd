fn main() {
    let target = std::env::var("TARGET").expect("Cargo must provide TARGET to build.rs");
    println!("cargo:rustc-env=SKIPPR_BUILD_TARGET_TRIPLE={target}");
}
