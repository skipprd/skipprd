use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileOpKind {
    Get,
    List,
    Patch,
    Write,
    Rm,
    Mv,
    Other,
}

impl FileOpKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::List => "list",
            Self::Patch => "patch",
            Self::Write => "write",
            Self::Rm => "rm",
            Self::Mv => "mv",
            Self::Other => "other",
        }
    }
}

pub const GENERAL_MUTATION_OPS: &[FileOpKind] =
    &[FileOpKind::Patch, FileOpKind::Rm, FileOpKind::Mv];
pub fn ops_label(ops: &[FileOpKind]) -> String {
    ops.iter()
        .map(|op| format!("op={}", op.as_str()))
        .collect::<Vec<_>>()
        .join("|")
}

pub fn general_mutation_ops_label() -> String {
    ops_label(GENERAL_MUTATION_OPS)
}

pub fn classify_file_op(args: &Value) -> FileOpKind {
    match args.get("op").and_then(|v| v.as_str()) {
        Some("get") => FileOpKind::Get,
        Some("list") => FileOpKind::List,
        Some("patch") => FileOpKind::Patch,
        Some("write") => FileOpKind::Write,
        Some("rm") => FileOpKind::Rm,
        Some("mv") => FileOpKind::Mv,
        _ => FileOpKind::Other,
    }
}

pub fn is_file_read_op(args: &Value) -> bool {
    matches!(classify_file_op(args), FileOpKind::Get | FileOpKind::List)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ops_label_renders_correctly() {
        assert_eq!(ops_label(&[FileOpKind::Patch]), "op=patch");
        assert_eq!(ops_label(&[FileOpKind::Rm, FileOpKind::Mv]), "op=rm|op=mv");
        assert_eq!(ops_label(GENERAL_MUTATION_OPS), "op=patch|op=rm|op=mv");
    }

    #[test]
    fn general_mutation_excludes_write() {
        assert!(!GENERAL_MUTATION_OPS.contains(&FileOpKind::Write));
        assert!(GENERAL_MUTATION_OPS.contains(&FileOpKind::Patch));
    }
}
