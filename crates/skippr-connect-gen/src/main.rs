use std::env;
use std::path::PathBuf;
use std::process;

fn main() {
    let mut args = env::args().skip(1);
    let check = matches!(args.next().as_deref(), Some("--check"));
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .expect("workspace root");
    let plugins = skippr_connect_gen::discover_plugins(&root).unwrap_or_else(|err| {
        eprintln!("{err}");
        process::exit(1);
    });
    if plugins.is_empty() {
        eprintln!("skippr-connect-gen found no plugins");
        process::exit(1);
    }
    if check {
        let kinds = skippr_connect_gen::emit_kinds_rs(&plugins);
        let cli = skippr_connect_gen::emit_cli_rs(&plugins);
        let py = skippr_connect_gen::emit_python_rs(&plugins);
        let ok_kinds =
            std::fs::read_to_string(root.join("src/connect_generated.rs")).unwrap_or_default();
        let ok_cli =
            std::fs::read_to_string(root.join("src/cli/connect_generated.rs")).unwrap_or_default();
        let ok_py = std::fs::read_to_string(root.join("python/src/connect_generated.rs"))
            .unwrap_or_default();
        if kinds != ok_kinds || cli != ok_cli || py != ok_py {
            eprintln!("generated connect files are stale; run cargo run -p skippr-connect-gen");
            process::exit(1);
        }
        return;
    }
    skippr_connect_gen::write_generated(&root, &plugins).unwrap_or_else(|err| {
        eprintln!("{err}");
        process::exit(1);
    });
}
