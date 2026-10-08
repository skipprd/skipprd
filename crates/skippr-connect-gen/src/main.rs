use std::env;
use std::path::PathBuf;
use std::process;

fn fail(err: String) -> ! {
    eprintln!("{err}");
    process::exit(1);
}

fn main() {
    let mut args = env::args().skip(1);
    let check = matches!(args.next().as_deref(), Some("--check"));
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .expect("workspace root");
    let plugins = skippr_connect_gen::discover_plugins(&root).unwrap_or_else(|err| fail(err));
    if plugins.is_empty() {
        fail("skippr-connect-gen found no plugins".into());
    }
    let engine = skippr_connect_gen::discover_engine(&root).unwrap_or_else(|err| fail(err));
    let generated =
        skippr_connect_gen::generated_sources(&plugins, &engine).unwrap_or_else(|err| fail(err));
    if check {
        let stale = skippr_connect_gen::stale_files(&root, &generated);
        if !stale.is_empty() {
            fail(format!(
                "generated files are stale ({}); run cargo run -p skippr-connect-gen",
                stale.join(", ")
            ));
        }
        return;
    }
    skippr_connect_gen::write_generated(&root, &generated).unwrap_or_else(|err| fail(err));
}
