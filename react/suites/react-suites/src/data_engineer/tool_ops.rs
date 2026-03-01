use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileOpKind {
    Get,
    List,
    Patch,
    Rm,
    Mv,
    PutLegacy,
    Other,
}

pub fn classify_file_op(args: &Value) -> FileOpKind {
    match args.get("op").and_then(|v| v.as_str()) {
        Some("get") => FileOpKind::Get,
        Some("list") => FileOpKind::List,
        Some("patch") => FileOpKind::Patch,
        Some("rm") => FileOpKind::Rm,
        Some("mv") => FileOpKind::Mv,
        // Legacy logs may still contain put; treat as mutating for history-gates.
        Some("put") => FileOpKind::PutLegacy,
        _ => FileOpKind::Other,
    }
}

pub fn is_file_read_op(args: &Value) -> bool {
    matches!(classify_file_op(args), FileOpKind::Get | FileOpKind::List)
}

pub fn is_file_mutation_op(args: &Value) -> bool {
    matches!(
        classify_file_op(args),
        FileOpKind::Patch | FileOpKind::Rm | FileOpKind::Mv | FileOpKind::PutLegacy
    )
}

pub fn is_file_repair_mutation_op(args: &Value) -> bool {
    matches!(
        classify_file_op(args),
        FileOpKind::Patch | FileOpKind::Rm | FileOpKind::Mv
    )
}
