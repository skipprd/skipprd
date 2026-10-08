//! Typed Python classes for plugin and pipeline configs: the PyO3 source
//! (`python/src/connect_generated.rs`) and the type stub (`skippr.pyi`, next to `pyproject.toml` so maturin ships it with `py.typed`).

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    ident_ok, EngineSpec, FieldSpec, PluginKind, PluginSpec, SecretKind, TypeCatalog,
    GENERATED_PREAMBLE,
};

/// The core `Config` / `Session` / ref / storage surface. Hand-written in
/// `python/src/config.rs`; `mypy.stubtest` keeps the two in step.
const STUB_CORE: &str = include_str!("stub_core.pyi");

/// Engine structs reachable from `Pipeline` in `src/helpers/configuration.rs`.
pub const ENGINE_ROOT: &str = "Pipeline";

/// Pipeline fields that hold registry references. Python takes the typed ref
/// returned by registration, never a `"<registry>.<name>"` string. The bool
/// marks refs the engine requires at validation, so Python requires them too.
const ENGINE_REF_FIELDS: &[(&str, &str, &str, bool)] = &[
    ("Pipeline", "data_source", "DataSourceRef", true),
    ("Pipeline", "data_sink", "DataSinkRef", false),
    ("Pipeline", "deadletter_sink", "DeadletterSinkRef", false),
];

/// Generated stub lines wrap past this width, matching `stub_core.pyi`.
const STUB_LINE_WIDTH: usize = 100;

const PY_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

/// How one Rust field type is spelled in Python. Every reflected type maps to
/// exactly one of these; anything else is a generation error.
#[derive(Clone, Debug, PartialEq)]
enum PyTy {
    Str,
    Bool,
    Int(&'static str),
    Float,
    /// A `#[skippr(secret)]` string: only `skippr.EnvRef` is accepted.
    EnvRef,
    /// `serde_json::Value`: any JSON-serializable Python value.
    Any,
    /// A string enum, validated against its serde names.
    Literal {
        rust: String,
        alias: String,
    },
    List(Box<PyTy>),
    Map(Box<PyTy>),
    Class {
        rust: String,
        py: String,
    },
    /// A tagged enum: one class per variant.
    Union {
        rust: String,
        alias: String,
    },
    Ref(&'static str),
}

impl PyTy {
    fn rust(&self) -> String {
        match self {
            Self::Str => "String".into(),
            Self::Bool => "bool".into(),
            Self::Int(prim) => (*prim).into(),
            Self::Float => "f64".into(),
            Self::EnvRef => "PyEnvRef".into(),
            Self::Any => "PyAnyValue".into(),
            Self::Literal { rust, .. } | Self::Class { rust, .. } | Self::Union { rust, .. } => {
                rust.clone()
            }
            Self::List(inner) => format!("Vec<{}>", inner.rust()),
            Self::Map(inner) => format!("BTreeMap<String, {}>", inner.rust()),
            Self::Ref(py) => format!("Py{py}"),
        }
    }

    fn py(&self) -> String {
        match self {
            Self::Str => "str".into(),
            Self::Bool => "bool".into(),
            Self::Int(_) => "int".into(),
            Self::Float => "float".into(),
            Self::EnvRef => "EnvRef".into(),
            Self::Any => "Any".into(),
            Self::Literal { alias, .. } | Self::Union { alias, .. } => alias.clone(),
            Self::Class { py, .. } => py.clone(),
            Self::List(inner) => format!("list[{}]", inner.py()),
            Self::Map(inner) => format!("dict[str, {}]", inner.py()),
            Self::Ref(py) => (*py).into(),
        }
    }

    /// The attribute type. Getters return a copy, so containers are read-only views.
    fn attr_py(&self) -> String {
        match self {
            Self::List(inner) => format!("Sequence[{}]", inner.py()),
            Self::Map(inner) => format!("Mapping[str, {}]", inner.py()),
            other => other.py(),
        }
    }
}

struct ClassField {
    ident: String,
    wire: String,
    ty: PyTy,
    optional: bool,
    doc: String,
}

struct ClassSpec {
    rust: String,
    py: String,
    doc: String,
    frozen: bool,
    fields: Vec<ClassField>,
}

struct LiteralSpec {
    rust: String,
    alias: String,
    values: Vec<String>,
}

impl LiteralSpec {
    /// A one-value enum is spelled inline: stubtest cannot check a
    /// `Literal["x"]` alias against its runtime twin.
    fn exported(&self) -> bool {
        self.values.len() > 1
    }

    fn py(&self) -> String {
        if self.exported() {
            self.alias.clone()
        } else {
            format!("Literal[{:?}]", self.values[0])
        }
    }
}

struct UnionSpec {
    rust: String,
    alias: String,
    tag: String,
    /// (variant ident, class, serde name)
    variants: Vec<(String, ClassSpec, String)>,
}

/// One namespace of reflected types (a plugin, or the engine) and the classes
/// generated for it.
struct Scope<'a> {
    types: &'a TypeCatalog,
    owner: &'a str,
    prefix: String,
    classes: Vec<ClassSpec>,
    literals: Vec<LiteralSpec>,
    unions: Vec<UnionSpec>,
    done: BTreeSet<String>,
}

impl<'a> Scope<'a> {
    fn new(types: &'a TypeCatalog, owner: &'a str, prefix: String) -> Self {
        Self {
            types,
            owner,
            prefix,
            classes: Vec::new(),
            literals: Vec::new(),
            unions: Vec::new(),
            done: BTreeSet::new(),
        }
    }

    fn name(&self, ty_name: &str) -> String {
        format!("{}{}", self.prefix, ty_name)
    }

    fn class(
        &mut self,
        py: String,
        doc: &str,
        struct_name: &str,
        fields: &[FieldSpec],
        frozen: bool,
    ) -> Result<ClassSpec, String> {
        let mut out = Vec::new();
        for field in fields {
            if PY_KEYWORDS.contains(&field.ident.as_str()) {
                return Err(format!(
                    "{}: field `{}` is a Python keyword and cannot be a keyword argument",
                    self.owner, field.ident
                ));
            }
            let ref_field = ENGINE_REF_FIELDS
                .iter()
                .find(|(s, f, _, _)| *s == struct_name && *f == field.ident);
            let ref_ty = ref_field.map(|(_, _, py, _)| PyTy::Ref(py));
            let optional =
                field.optional && !ref_field.is_some_and(|(_, _, _, required)| *required);
            let base = unwrap_generic(&field.rust_ty, "Option").unwrap_or(&field.rust_ty);
            let ty = match ref_ty {
                Some(ty) => ty,
                None => self.resolve(base).map_err(|err| {
                    format!(
                        "{}: field `{}` ({}): {err}",
                        self.owner, field.ident, field.rust_ty
                    )
                })?,
            };
            let ty = match (field.secret, ty) {
                (SecretKind::Secret, PyTy::Str) => PyTy::EnvRef,
                (SecretKind::Secret, other) => {
                    return Err(format!(
                        "{}: secret field `{}` must be a string, found {other:?}",
                        self.owner, field.ident
                    ))
                }
                (_, ty) => ty,
            };
            out.push(ClassField {
                ident: field.ident.clone(),
                wire: field.yaml_path.clone(),
                ty,
                optional,
                doc: field.doc.clone(),
            });
        }
        out.sort_by_key(|f| f.optional);
        Ok(ClassSpec {
            rust: format!("Py{py}"),
            py,
            doc: doc.to_string(),
            frozen,
            fields: out,
        })
    }

    fn resolve(&mut self, ty: &str) -> Result<PyTy, String> {
        if unwrap_generic(ty, "Option").is_some() {
            return Err("nested Option is not representable".into());
        }
        if let Some(inner) = unwrap_generic(ty, "Box") {
            return self.resolve(inner);
        }
        for list in ["Vec", "BTreeSet", "HashSet", "VecDeque"] {
            if let Some(inner) = unwrap_generic(ty, list) {
                return Ok(PyTy::List(Box::new(self.resolve(inner)?)));
            }
        }
        for map in ["BTreeMap", "HashMap"] {
            if let Some(inner) = unwrap_generic(ty, map) {
                let (key, value) = split_top_level_comma(inner)
                    .ok_or_else(|| format!("{map} needs a key and value type"))?;
                if self.resolve(key)? != PyTy::Str {
                    return Err(format!("{map} keys must be strings"));
                }
                return Ok(PyTy::Map(Box::new(self.resolve(value)?)));
            }
        }
        let leaf = ty.rsplit("::").next().unwrap_or(ty);
        match leaf {
            "String" | "str" | "&str" | "PathBuf" => return Ok(PyTy::Str),
            "bool" => return Ok(PyTy::Bool),
            "u8" => return Ok(PyTy::Int("u8")),
            "u16" => return Ok(PyTy::Int("u16")),
            "u32" => return Ok(PyTy::Int("u32")),
            "u64" => return Ok(PyTy::Int("u64")),
            "usize" => return Ok(PyTy::Int("usize")),
            "i8" => return Ok(PyTy::Int("i8")),
            "i16" => return Ok(PyTy::Int("i16")),
            "i32" => return Ok(PyTy::Int("i32")),
            "i64" => return Ok(PyTy::Int("i64")),
            "isize" => return Ok(PyTy::Int("isize")),
            "f32" | "f64" => return Ok(PyTy::Float),
            "Value" if matches!(ty, "Value" | "serde_json::Value" | "serde_yaml::Value") => {
                return Ok(PyTy::Any)
            }
            _ => {}
        }
        if let Some(spec) = self.types.structs.get(leaf).cloned() {
            let py = self.name(leaf);
            if self.done.insert(leaf.to_string()) {
                let class = self.class(py.clone(), &spec.doc, leaf, &spec.fields, true)?;
                self.classes.push(class);
            }
            return Ok(PyTy::Class {
                rust: format!("Py{py}"),
                py,
            });
        }
        if let Some(spec) = self.types.enums.get(leaf).cloned() {
            let alias = self.name(leaf);
            let rust = format!("Py{alias}");
            if spec.is_string_enum() {
                let literal = LiteralSpec {
                    rust: rust.clone(),
                    alias,
                    values: spec.variants.iter().map(|v| v.wire.clone()).collect(),
                };
                let py = literal.py();
                if self.done.insert(leaf.to_string()) {
                    self.literals.push(literal);
                }
                return Ok(PyTy::Literal { rust, alias: py });
            }
            let Some(tag) = spec.tag.clone() else {
                return Err(format!(
                    "enum {leaf} must be a string enum or #[serde(tag = ...)]"
                ));
            };
            if self.done.insert(leaf.to_string()) {
                let mut variants = Vec::new();
                for variant in &spec.variants {
                    let py = format!("{alias}{}", variant.rust_name);
                    let doc = if variant.doc.is_empty() {
                        spec.doc.clone()
                    } else {
                        variant.doc.clone()
                    };
                    let fields = variant.fields.clone().unwrap_or_default();
                    let class = self.class(py, &doc, leaf, &fields, true)?;
                    variants.push((variant.rust_name.clone(), class, variant.wire.clone()));
                }
                self.unions.push(UnionSpec {
                    rust: rust.clone(),
                    alias: alias.clone(),
                    tag,
                    variants,
                });
            }
            return Ok(PyTy::Union { rust, alias });
        }
        Err(format!("no Python mapping for `{ty}`"))
    }
}

/// `Name<inner>` (any path prefix) → `inner`.
fn unwrap_generic<'t>(ty: &'t str, name: &str) -> Option<&'t str> {
    let open = ty.find('<')?;
    let head = &ty[..open];
    if head.rsplit("::").next() != Some(name) || !ty.ends_with('>') {
        return None;
    }
    Some(&ty[open + 1..ty.len() - 1])
}

fn split_top_level_comma(ty: &str) -> Option<(&str, &str)> {
    let mut depth = 0i32;
    for (i, ch) in ty.char_indices() {
        match ch {
            '<' => depth += 1,
            '>' => depth -= 1,
            ',' if depth == 0 => return Some((&ty[..i], &ty[i + 1..])),
            _ => {}
        }
    }
    None
}

/// Python class name for a plugin config: always role-prefixed, so
/// `DataSourceS3` and `DataSinkS3` never collide.
pub fn plugin_class_name(plugin: &PluginSpec) -> String {
    format!("{}{}", plugin.kind.as_str(), plugin.plugin_name)
}

fn is_paired(plugin: &PluginSpec, engine: &EngineSpec) -> bool {
    engine.paired_sinks.contains(&plugin.plugin_name)
}

/// Everything generated for Python, before rendering.
pub struct PythonModel {
    plugin_classes: Vec<(PluginKind, String, bool, ClassSpec)>,
    scopes_classes: Vec<ClassSpec>,
    literals: Vec<LiteralSpec>,
    unions: Vec<UnionSpec>,
    engine_classes: Vec<ClassSpec>,
}

pub fn python_model(plugins: &[PluginSpec], engine: &EngineSpec) -> Result<PythonModel, String> {
    let mut model = PythonModel {
        plugin_classes: Vec::new(),
        scopes_classes: Vec::new(),
        literals: Vec::new(),
        unions: Vec::new(),
        engine_classes: Vec::new(),
    };
    for paired in &engine.paired_sinks {
        for kind in [PluginKind::DataSink, PluginKind::SchemaSink] {
            if !plugins
                .iter()
                .any(|p| p.kind == kind && &p.plugin_name == paired)
            {
                return Err(format!(
                    "PairedSink::{paired} has no {} plugin",
                    kind.as_str()
                ));
            }
        }
    }
    let mut errors = Vec::new();
    // A paired schema sink shares its data sink's config and registers
    // through `Config.data_sink(schema_sink=...)`, so it has no class.
    let python_plugins = plugins
        .iter()
        .filter(|p| !(p.kind == PluginKind::SchemaSink && is_paired(p, engine)));
    for plugin in python_plugins {
        let py = plugin_class_name(plugin);
        let mut scope = Scope::new(&plugin.types, &plugin.crate_name, py.clone());
        let class = match scope.class(py, &plugin.doc, "", &plugin.config_fields, false) {
            Ok(class) => class,
            Err(err) => {
                errors.push(err);
                continue;
            }
        };
        model.plugin_classes.push((
            plugin.kind,
            plugin.plugin_name.clone(),
            plugin.kind == PluginKind::DataSink && is_paired(plugin, engine),
            class,
        ));
        model.scopes_classes.extend(scope.classes);
        model.literals.extend(scope.literals);
        model.unions.extend(scope.unions);
    }
    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    let root = engine
        .types
        .structs
        .get(ENGINE_ROOT)
        .ok_or_else(|| format!("engine struct {ENGINE_ROOT} not found"))?
        .clone();
    let mut scope = Scope::new(&engine.types, "skipprd configuration", String::new());
    let pipeline = scope.class(
        ENGINE_ROOT.to_string(),
        &root.doc,
        ENGINE_ROOT,
        &root.fields,
        false,
    )?;
    model.engine_classes.push(pipeline);
    model.engine_classes.extend(scope.classes);
    model.literals.extend(scope.literals);
    model.unions.extend(scope.unions);

    let mut names = BTreeSet::new();
    let all = model
        .plugin_classes
        .iter()
        .map(|(_, _, _, c)| c.py.clone())
        .chain(model.scopes_classes.iter().map(|c| c.py.clone()))
        .chain(model.engine_classes.iter().map(|c| c.py.clone()))
        .chain(
            model
                .literals
                .iter()
                .filter(|l| l.exported())
                .map(|l| l.alias.clone()),
        )
        .chain(model.unions.iter().map(|u| u.alias.clone()))
        .chain(
            model
                .unions
                .iter()
                .flat_map(|u| u.variants.iter().map(|(_, c, _)| c.py.clone())),
        );
    for name in all {
        if !names.insert(name.clone()) {
            return Err(format!("duplicate generated Python name {name}"));
        }
    }
    Ok(model)
}

impl PythonModel {
    fn every_class(&self) -> impl Iterator<Item = &ClassSpec> {
        self.scopes_classes
            .iter()
            .chain(
                self.unions
                    .iter()
                    .flat_map(|u| u.variants.iter().map(|(_, c, _)| c)),
            )
            .chain(self.engine_classes.iter())
            .chain(self.plugin_classes.iter().map(|(_, _, _, c)| c))
    }

    fn plugins_of(
        &self,
        kind: PluginKind,
    ) -> impl Iterator<Item = &(PluginKind, String, bool, ClassSpec)> {
        self.plugin_classes.iter().filter(move |(k, ..)| *k == kind)
    }
}

fn emit_rust_class(out: &mut String, class: &ClassSpec) {
    if !class.doc.is_empty() {
        out.push_str(&format!("#[doc = {:?}]\n", class.doc));
    }
    let frozen = if class.frozen { "frozen, " } else { "" };
    out.push_str(&format!(
        "#[pyclass({frozen}from_py_object, name = \"{}\", module = \"skippr\")]\n#[derive(Clone, Serialize)]\npub struct {} {{\n",
        class.py, class.rust
    ));
    let access = if class.frozen { "get" } else { "get, set" };
    for field in &class.fields {
        let doc = field_doc(field);
        out.push_str(&format!("    #[doc = {doc:?}]\n    #[pyo3({access})]\n"));
        let mut serde = Vec::new();
        if field.wire != field.ident {
            serde.push(format!("rename = {:?}", field.wire));
        }
        let ty = if field.optional {
            serde.push("skip_serializing_if = \"Option::is_none\"".to_string());
            format!("Option<{}>", field.ty.rust())
        } else {
            field.ty.rust()
        };
        if !serde.is_empty() {
            out.push_str(&format!("    #[serde({})]\n", serde.join(", ")));
        }
        out.push_str(&format!("    pub {}: {ty},\n", ident_ok(&field.ident)));
    }
    out.push_str("}\n\n#[pymethods]\n");
    out.push_str(&format!("impl {} {{\n    #[new]\n", class.rust));
    let signature = class
        .fields
        .iter()
        .map(|f| {
            if f.optional {
                format!("{} = None", ident_ok(&f.ident))
            } else {
                ident_ok(&f.ident)
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    if class.fields.is_empty() {
        out.push_str("    fn new() -> Self {\n        Self {}\n    }\n");
    } else {
        out.push_str(&format!(
            "    #[pyo3(signature = (*, {signature}))]\n    fn new(\n"
        ));
        for field in &class.fields {
            let ty = if field.optional {
                format!("Option<{}>", field.ty.rust())
            } else {
                field.ty.rust()
            };
            out.push_str(&format!("        {}: {ty},\n", ident_ok(&field.ident)));
        }
        out.push_str("    ) -> Self {\n        Self {\n");
        for field in &class.fields {
            out.push_str(&format!("            {},\n", ident_ok(&field.ident)));
        }
        out.push_str("        }\n    }\n");
    }
    out.push_str(&format!(
        "\n    fn __repr__(&self) -> PyResult<String> {{\n        config_repr(\"{}\", self)\n    }}\n}}\n\n",
        class.py
    ));
}

fn field_doc(field: &ClassField) -> String {
    if field.doc.is_empty() {
        format!("YAML key `{}`.", field.wire)
    } else {
        format!("{}\n\nYAML key `{}`.", field.doc, field.wire)
    }
}

/// `python/src/connect_generated.rs`, `include!`d by `python/src/lib.rs`.
pub fn emit_python_rs(model: &PythonModel) -> String {
    let mut out = String::from(GENERATED_PREAMBLE);
    out.push('\n');
    for literal in &model.literals {
        let values = literal
            .values
            .iter()
            .map(|v| format!("{v:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!(
            "config_literal!({}, \"{}\", [{values}]);\n",
            literal.rust, literal.alias
        ));
    }
    out.push('\n');
    for union in &model.unions {
        out.push_str(&format!(
            "config_union!({}, \"{}\", \"{}\", {{\n",
            union.rust, union.alias, union.tag
        ));
        for (variant, class, wire) in &union.variants {
            out.push_str(&format!(
                "    {}({}) = {wire:?},\n",
                ident_ok(variant),
                class.rust
            ));
        }
        out.push_str("});\n\n");
    }
    for class in model.every_class() {
        emit_rust_class(&mut out, class);
    }

    out.push_str(
        "pub fn extract_data_source_config(obj: &Bound<'_, PyAny>) -> PyResult<PluginConfigEntry> {\n",
    );
    for (_, plugin_name, _, class) in model.plugins_of(PluginKind::DataSource) {
        out.push_str(&format!(
            "    if let Ok(config) = obj.cast::<{}>() {{\n        return plugin_entry(\"{plugin_name}\", &*config.borrow());\n    }}\n",
            class.rust
        ));
    }
    out.push_str("    Err(wrong_config_kind(\"DataSource\", obj))\n}\n\n");

    out.push_str(
        "pub fn extract_data_sink_config(obj: &Bound<'_, PyAny>) -> PyResult<SinkConfig> {\n",
    );
    for (_, plugin_name, paired, class) in model.plugins_of(PluginKind::DataSink) {
        let variant = if *paired { "Paired" } else { "Unpaired" };
        out.push_str(&format!(
            "    if let Ok(config) = obj.cast::<{}>() {{\n        return plugin_entry(\"{plugin_name}\", &*config.borrow()).map(SinkConfig::{variant});\n    }}\n",
            class.rust
        ));
    }
    out.push_str("    Err(wrong_config_kind(\"DataSink\", obj))\n}\n\n");

    out.push_str(
        "pub fn extract_schema_sink_config(obj: &Bound<'_, PyAny>) -> PyResult<PluginConfigEntry> {\n",
    );
    for (_, plugin_name, _, class) in model.plugins_of(PluginKind::SchemaSink) {
        out.push_str(&format!(
            "    if let Ok(config) = obj.cast::<{}>() {{\n        return plugin_entry(\"{plugin_name}\", &*config.borrow());\n    }}\n",
            class.rust
        ));
    }
    out.push_str("    Err(wrong_config_kind(\"SchemaSink\", obj))\n}\n\n");

    out.push_str("pub fn register_generated_classes(m: &Bound<'_, PyModule>) -> PyResult<()> {\n");
    for class in model.every_class() {
        out.push_str(&format!("    m.add_class::<{}>()?;\n", class.rust));
    }
    for literal in model.literals.iter().filter(|l| l.exported()) {
        out.push_str(&format!(
            "    m.add(\"{}\", literal_alias(m.py(), {}::VALUES)?)?;\n",
            literal.alias, literal.rust
        ));
    }
    for union in &model.unions {
        out.push_str(&format!(
            "    m.add(\"{}\", {}::type_alias(m.py())?)?;\n",
            union.alias, union.rust
        ));
    }
    for (alias, classes) in model.config_aliases() {
        let types = classes
            .iter()
            .map(|c| format!("m.py().get_type::<{c}>().into_any()"))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!(
            "    m.add(\"{alias}\", union_alias([{types}])?)?;\n"
        ));
    }
    out.push_str("    Ok(())\n}\n");
    out
}

impl PythonModel {
    /// The per-role config unions (`DataSourceConfig`, ...), Rust struct names.
    fn config_aliases(&self) -> Vec<(&'static str, Vec<String>)> {
        let rust = |kind: PluginKind, paired: Option<bool>| -> Vec<String> {
            self.plugins_of(kind)
                .filter(|(_, _, p, _)| paired.map_or(true, |want| *p == want))
                .map(|(_, _, _, c)| c.rust.clone())
                .collect()
        };
        vec![
            ("DataSourceConfig", rust(PluginKind::DataSource, None)),
            (
                "PairedDataSinkConfig",
                rust(PluginKind::DataSink, Some(true)),
            ),
            (
                "UnpairedDataSinkConfig",
                rust(PluginKind::DataSink, Some(false)),
            ),
            ("DataSinkConfig", rust(PluginKind::DataSink, None)),
            ("SchemaSinkConfig", rust(PluginKind::SchemaSink, None)),
        ]
    }
}

fn py_docstring(doc: &str, indent: &str) -> String {
    if doc.is_empty() {
        return String::new();
    }
    let body = doc.replace('\\', "\\\\").replace("\"\"\"", "\\\"\\\"\\\"");
    let body = body
        .lines()
        .enumerate()
        .map(|(i, line)| {
            if i == 0 || line.is_empty() {
                line.to_string()
            } else {
                format!("{indent}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("{indent}\"\"\"{body}\"\"\"\n")
}

fn emit_stub_class(out: &mut String, class: &ClassSpec) {
    out.push_str(&format!("@final\nclass {}:\n", class.py));
    out.push_str(&py_docstring(&class.doc, "    "));
    let params = class
        .fields
        .iter()
        .map(|f| {
            if f.optional {
                format!("{}: {} | None = None", f.ident, f.ty.py())
            } else {
                format!("{}: {}", f.ident, f.ty.py())
            }
        })
        .collect::<Vec<_>>();
    let one_line = if params.is_empty() {
        format!("    def __new__(cls) -> {}: ...\n", class.py)
    } else {
        format!(
            "    def __new__(cls, *, {}) -> {}: ...\n",
            params.join(", "),
            class.py
        )
    };
    if one_line.len() <= STUB_LINE_WIDTH {
        out.push_str(&one_line);
    } else {
        out.push_str("    def __new__(\n        cls,\n        *,\n");
        for param in &params {
            out.push_str(&format!("        {param},\n"));
        }
        out.push_str(&format!("    ) -> {}: ...\n", class.py));
    }
    for field in &class.fields {
        let optional = |ty: String| {
            if field.optional {
                format!("{ty} | None")
            } else {
                ty
            }
        };
        let ty = optional(field.ty.attr_py());
        let set_ty = optional(field.ty.py());
        if class.frozen || set_ty != ty {
            out.push_str(&format!(
                "    @property\n    def {}(self) -> {ty}:\n{}        ...\n",
                field.ident,
                py_docstring(&field_doc(field), "        ")
            ));
            if !class.frozen {
                out.push_str(&format!(
                    "    @{0}.setter\n    def {0}(self, value: {set_ty}) -> None: ...\n",
                    field.ident
                ));
            }
        } else {
            out.push_str(&format!(
                "    {}: {ty}\n{}",
                field.ident,
                py_docstring(&field_doc(field), "    ")
            ));
        }
    }
    out.push('\n');
}

fn stub_union_alias(out: &mut String, alias: &str, members: &[&str]) {
    let one_line = format!("{alias}: TypeAlias = {}\n", members.join(" | "));
    if one_line.len() <= STUB_LINE_WIDTH {
        out.push_str(&one_line);
        return;
    }
    out.push_str(&format!(
        "{alias}: TypeAlias = (\n    {}\n)\n",
        members.join("\n    | ")
    ));
}

/// `skippr.pyi`: the core surface plus every generated class and alias.
pub fn emit_python_stub(model: &PythonModel) -> String {
    let mut out = String::from("# @generated by skippr-connect-gen. Do not edit.\n");
    out.push_str(STUB_CORE);
    out.push('\n');
    for literal in model.literals.iter().filter(|l| l.exported()) {
        let values = literal
            .values
            .iter()
            .map(|v| format!("{v:?}"))
            .collect::<Vec<_>>();
        let one_line = format!(
            "{}: TypeAlias = Literal[{}]\n",
            literal.alias,
            values.join(", ")
        );
        if one_line.len() <= STUB_LINE_WIDTH {
            out.push_str(&one_line);
        } else {
            out.push_str(&format!("{}: TypeAlias = Literal[\n", literal.alias));
            for value in &values {
                out.push_str(&format!("    {value},\n"));
            }
            out.push_str("]\n");
        }
    }
    for union in &model.unions {
        let variants = union
            .variants
            .iter()
            .map(|(_, c, _)| c.py.as_str())
            .collect::<Vec<_>>();
        stub_union_alias(&mut out, &union.alias, &variants);
    }
    let py_names: BTreeMap<&str, &str> = model
        .plugin_classes
        .iter()
        .map(|(_, _, _, c)| (c.rust.as_str(), c.py.as_str()))
        .collect();
    for (alias, classes) in model.config_aliases() {
        let names = classes
            .iter()
            .map(|rust| py_names[rust.as_str()])
            .collect::<Vec<_>>();
        stub_union_alias(&mut out, alias, &names);
    }
    out.push('\n');
    for class in model.every_class() {
        emit_stub_class(&mut out, class);
    }
    out
}
