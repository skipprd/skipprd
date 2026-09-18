//! Reflect in-tree plugin config structs into connect CLI/Python/persist metadata.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use syn::{Attribute, Fields, Item, Type};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum PluginKind {
    DataSource,
    DataSink,
    SchemaSink,
}

impl PluginKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DataSource => "DataSource",
            Self::DataSink => "DataSink",
            Self::SchemaSink => "SchemaSink",
        }
    }

    pub fn clap_role(self) -> &'static str {
        match self {
            Self::DataSource => "data-source",
            Self::DataSink => "data-sink",
            Self::SchemaSink => "schema-sink",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SecretKind {
    None,
    Secret,
    SecretPath,
    NotSecret,
}

#[derive(Clone, Debug)]
pub struct FieldSpec {
    pub ident: String,
    pub yaml_path: String,
    pub optional: bool,
    pub secret: SecretKind,
    pub rust_ty: String,
}

#[derive(Clone, Debug)]
pub struct PluginSpec {
    pub kind: PluginKind,
    pub plugin_name: String,
    pub crate_name: String,
    pub fields: Vec<FieldSpec>,
}

pub fn workspace_root(from: &Path) -> PathBuf {
    from.to_path_buf()
}

pub fn discover_plugins(root: &Path) -> Result<Vec<PluginSpec>, String> {
    let mut plugins = Vec::new();
    for kind_dir in ["data_source", "data_sink", "schema_sink"] {
        let dir = root.join("plugins").join(kind_dir);
        if !dir.exists() {
            continue;
        }
        let mut crates: Vec<PathBuf> = fs::read_dir(&dir)
            .map_err(|err| format!("read {}: {err}", dir.display()))?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.join("Cargo.toml").exists())
            .collect();
        crates.sort();
        for crate_dir in crates {
            plugins.push(load_plugin(root, &crate_dir)?);
        }
    }
    plugins.sort_by(|a, b| (a.kind, &a.plugin_name).cmp(&(b.kind, &b.plugin_name)));
    Ok(plugins)
}

fn load_plugin(root: &Path, crate_dir: &Path) -> Result<PluginSpec, String> {
    let cargo_text = fs::read_to_string(crate_dir.join("Cargo.toml"))
        .map_err(|err| format!("{}: {err}", crate_dir.display()))?;
    let cargo: toml::Value = toml::from_str(&cargo_text)
        .map_err(|err| format!("{} Cargo.toml: {err}", crate_dir.display()))?;
    let meta = cargo
        .get("package")
        .and_then(|p| p.get("metadata"))
        .and_then(|m| m.get("skippr-plugin"))
        .ok_or_else(|| {
            format!(
                "{} missing [package.metadata.skippr-plugin]",
                crate_dir.display()
            )
        })?;
    let kind = match meta.get("kind").and_then(|v| v.as_str()) {
        Some("DataSource") => PluginKind::DataSource,
        Some("DataSink") => PluginKind::DataSink,
        Some("SchemaSink") => PluginKind::SchemaSink,
        other => {
            return Err(format!(
                "{} unknown plugin kind {other:?}",
                crate_dir.display()
            ))
        }
    };
    let plugin_name = meta
        .get("plugin_name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("{} missing plugin_name", crate_dir.display()))?
        .to_string();
    let crate_name = cargo
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let mut fields = reflect_crate(crate_dir)?;
    if fields.is_none() && kind == PluginKind::SchemaSink {
        if let Some(dep_dir) = schema_sink_dep_dir(crate_dir, &cargo) {
            fields = reflect_crate(&dep_dir)?;
        }
    }
    if fields.is_none() && plugin_name == "Stdout" {
        fields = Some(Vec::new());
    }
    let fields = fields.ok_or_else(|| {
        format!(
            "{} has skippr-plugin metadata but no parseable config struct",
            crate_dir.display()
        )
    })?;
    validate_secrets(&crate_name, &fields)?;
    let _ = root;
    Ok(PluginSpec {
        kind,
        plugin_name,
        crate_name,
        fields,
    })
}

fn schema_sink_dep_dir(crate_dir: &Path, cargo: &toml::Value) -> Option<PathBuf> {
    let deps = cargo.get("dependencies")?.as_table()?;
    for (name, spec) in deps {
        if !name.starts_with("skippr-plugin-data-") {
            continue;
        }
        let path = spec.get("path").and_then(|v| v.as_str())?;
        return Some(crate_dir.join(path));
    }
    None
}

fn reflect_crate(crate_dir: &Path) -> Result<Option<Vec<FieldSpec>>, String> {
    let mut files = Vec::new();
    collect_mod_graph(crate_dir, &crate_dir.join("src/lib.rs"), &mut files)?;
    if crate_dir.join("src/main.rs").exists() {
        collect_mod_graph(crate_dir, &crate_dir.join("src/main.rs"), &mut files)?;
    }
    if let Ok(cargo_text) = fs::read_to_string(crate_dir.join("Cargo.toml")) {
        if let Ok(cargo) = toml::from_str::<toml::Value>(&cargo_text) {
            if let Some(deps) = cargo.get("dependencies").and_then(|v| v.as_table()) {
                if let Some(spec) = deps.get("skippr-iceberg-catalog") {
                    if let Some(rel) = spec.get("path").and_then(|v| v.as_str()) {
                        let dep_dir = crate_dir.join(rel);
                        collect_mod_graph(&dep_dir, &dep_dir.join("src/lib.rs"), &mut files)?;
                    }
                }
            }
        }
    }
    let mut structs: BTreeMap<String, (bool, Vec<FieldSpec>)> = BTreeMap::new();
    let mut enums: BTreeMap<String, (Option<String>, Vec<FieldSpec>)> = BTreeMap::new();
    let mut try_from_targets = BTreeSet::new();
    for file in files {
        let text = match fs::read_to_string(&file) {
            Ok(text) => text,
            Err(_) => continue,
        };
        let parsed = match syn::parse_file(&text) {
            Ok(file) => file,
            Err(_) => continue,
        };
        for item in parsed.items {
            match item {
                Item::Struct(item) => {
                    if !has_deserialize(&item.attrs) && item.ident != "OtlpConfigRaw" {
                        continue;
                    }
                    let fields = struct_fields(&item.fields)?;
                    structs.insert(item.ident.to_string(), (true, fields));
                }
                Item::Enum(item) => {
                    if !has_deserialize(&item.attrs) {
                        continue;
                    }
                    let mut fields = Vec::new();
                    let mut seen = BTreeSet::new();
                    for variant in item.variants {
                        let nested = struct_fields(&variant.fields)?;
                        for field in nested {
                            if seen.insert(field.ident.clone()) {
                                fields.push(field);
                            }
                        }
                    }
                    enums.insert(item.ident.to_string(), (serde_tag(&item.attrs), fields));
                }
                Item::Impl(item) => {
                    if let Some((_, path, _)) = &item.trait_ {
                        if path_ends_with(path, "TryFrom") {
                            if let Type::Path(ty) = &*item.self_ty {
                                if let Some(ident) = ty.path.segments.last() {
                                    try_from_targets.insert(ident.ident.to_string());
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(name) = pick_config_name(&structs, &try_from_targets) {
        let fields = structs.get(&name).map(|(_, fields)| fields.clone());
        return Ok(fields.map(|fields| expand_nested(fields, &structs, &enums)));
    }
    Ok(None)
}

fn rust_type_name(ty: &str) -> String {
    let ty = ty
        .trim()
        .trim_start_matches("Option<")
        .trim_end_matches('>')
        .rsplit("::")
        .next()
        .unwrap_or(ty);
    ty.to_string()
}

fn expand_nested(
    fields: Vec<FieldSpec>,
    structs: &BTreeMap<String, (bool, Vec<FieldSpec>)>,
    enums: &BTreeMap<String, (Option<String>, Vec<FieldSpec>)>,
) -> Vec<FieldSpec> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for field in fields {
        let ty_name = rust_type_name(&field.rust_ty);
        if let Some((_, nested)) = structs.get(&ty_name) {
            for child in nested {
                let expanded = FieldSpec {
                    ident: format!("{}_{}", field.ident, child.ident),
                    yaml_path: format!("{}.{}", field.yaml_path, child.yaml_path),
                    optional: field.optional || child.optional,
                    secret: child.secret,
                    rust_ty: child.rust_ty.clone(),
                };
                if seen.insert(expanded.ident.clone()) {
                    out.push(expanded);
                }
            }
            continue;
        }
        if let Some((Some(tag), nested)) = enums.get(&ty_name) {
            let type_field = FieldSpec {
                ident: format!("{}_{}", field.ident, tag.replace('-', "_")),
                yaml_path: format!("{}.{tag}", field.yaml_path),
                optional: field.optional,
                secret: SecretKind::None,
                rust_ty: "String".into(),
            };
            if seen.insert(type_field.ident.clone()) {
                out.push(type_field);
            }
            for child in nested {
                let expanded = FieldSpec {
                    ident: format!("{}_{}", field.ident, child.ident),
                    yaml_path: format!("{}.{}", field.yaml_path, child.yaml_path),
                    optional: true,
                    secret: child.secret,
                    rust_ty: child.rust_ty.clone(),
                };
                if seen.insert(expanded.ident.clone()) {
                    out.push(expanded);
                }
            }
            continue;
        }
        if seen.insert(field.ident.clone()) {
            out.push(field);
        }
    }
    out
}

fn pick_config_name(
    structs: &BTreeMap<String, (bool, Vec<FieldSpec>)>,
    try_from_targets: &BTreeSet<String>,
) -> Option<String> {
    let plugin_configs: Vec<String> = structs
        .keys()
        .filter(|name| name.ends_with("PluginConfig"))
        .cloned()
        .collect();
    if plugin_configs.len() == 1 {
        return Some(plugin_configs[0].clone());
    }
    for name in try_from_targets {
        if structs.contains_key(name) {
            return Some(name.clone());
        }
    }
    if structs.contains_key("OtlpConfigRaw") {
        return Some("OtlpConfigRaw".into());
    }
    let configs: Vec<String> = structs
        .keys()
        .filter(|name| {
            name.ends_with("Config")
                && !name.ends_with("Checkpoint")
                && !name.contains("Privacy")
                && *name != "RetryConfig"
        })
        .cloned()
        .collect();
    if configs.len() == 1 {
        return Some(configs[0].clone());
    }
    None
}

fn collect_mod_graph(crate_dir: &Path, file: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    if out.iter().any(|p| p == file) {
        return Ok(());
    }
    if !file.exists() {
        return Ok(());
    }
    out.push(file.to_path_buf());
    let text = fs::read_to_string(file).map_err(|err| format!("read {}: {err}", file.display()))?;
    let parsed = match syn::parse_file(&text) {
        Ok(file) => file,
        Err(_) => return Ok(()),
    };
    let parent = file.parent().unwrap_or(crate_dir);
    for item in parsed.items {
        let Item::Mod(module) = item else { continue };
        if module.content.is_some() {
            continue;
        }
        let name = module.ident.to_string();
        let path_attr = module.attrs.iter().find_map(|attr| {
            if let syn::Meta::NameValue(nv) = &attr.meta {
                if nv.path.is_ident("path") {
                    if let syn::Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Str(lit),
                        ..
                    }) = &nv.value
                    {
                        return Some(lit.value());
                    }
                }
            }
            None
        });
        let candidates = if let Some(rel) = path_attr {
            vec![parent.join(&rel)]
        } else {
            vec![
                parent.join(format!("{name}.rs")),
                parent.join(name).join("mod.rs"),
            ]
        };
        for candidate in candidates {
            collect_mod_graph(crate_dir, &candidate, out)?;
        }
    }
    Ok(())
}

fn serde_tag(attrs: &[Attribute]) -> Option<String> {
    for attr in attrs {
        if !attr.path().is_ident("serde") {
            continue;
        }
        let Ok(meta) = attr.parse_args_with(
            syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
        ) else {
            continue;
        };
        for item in meta {
            let syn::Meta::NameValue(nv) = item else {
                continue;
            };
            if !nv.path.is_ident("tag") {
                continue;
            }
            if let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(value),
                ..
            }) = nv.value
            {
                return Some(value.value());
            }
        }
    }
    None
}

fn has_deserialize(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if !attr.path().is_ident("derive") {
            return false;
        }
        let Ok(meta) = attr.parse_args_with(
            syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated,
        ) else {
            return false;
        };
        meta.iter().any(|path| {
            path.segments
                .last()
                .map(|seg| seg.ident == "Deserialize")
                .unwrap_or(false)
        })
    })
}

fn struct_fields(fields: &Fields) -> Result<Vec<FieldSpec>, String> {
    let Fields::Named(named) = fields else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for field in &named.named {
        let ident = field
            .ident
            .as_ref()
            .ok_or_else(|| "unnamed plugin config field".to_string())?
            .to_string();
        let optional = is_option(&field.ty) || has_serde_default(&field.attrs);
        let secret = parse_secret_attr(&field.attrs);
        out.push(FieldSpec {
            ident: ident.clone(),
            yaml_path: ident,
            optional,
            secret,
            rust_ty: type_to_string(&field.ty),
        });
    }
    Ok(out)
}

fn is_option(ty: &Type) -> bool {
    if let Type::Path(path) = ty {
        path.path
            .segments
            .last()
            .map(|seg| seg.ident == "Option")
            .unwrap_or(false)
    } else {
        false
    }
}

fn has_serde_default(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if !attr.path().is_ident("serde") {
            return false;
        }
        attr.to_token_stream_contains("default")
    })
}

trait TokenContains {
    fn to_token_stream_contains(&self, needle: &str) -> bool;
}

impl TokenContains for Attribute {
    fn to_token_stream_contains(&self, needle: &str) -> bool {
        self.meta.to_token_stream_string().contains(needle)
    }
}

trait ToTokensString {
    fn to_token_stream_string(&self) -> String;
}

impl ToTokensString for syn::Meta {
    fn to_token_stream_string(&self) -> String {
        quote::ToTokens::to_token_stream(self).to_string()
    }
}

fn parse_secret_attr(attrs: &[Attribute]) -> SecretKind {
    for attr in attrs {
        if !attr.path().is_ident("skippr") {
            continue;
        }
        let tokens = quote::ToTokens::to_token_stream(attr).to_string();
        if tokens.contains("secret_path") {
            return SecretKind::SecretPath;
        }
        if tokens.contains("not_secret") {
            return SecretKind::NotSecret;
        }
        if tokens.contains("secret") {
            return SecretKind::Secret;
        }
    }
    SecretKind::None
}

fn type_to_string(ty: &Type) -> String {
    quote::ToTokens::to_token_stream(ty)
        .to_string()
        .replace(' ', "")
}

fn path_ends_with(path: &syn::Path, name: &str) -> bool {
    path.segments
        .last()
        .map(|seg| seg.ident == name)
        .unwrap_or(false)
}

pub fn heuristic_secret(ident: &str) -> Option<SecretKind> {
    if ident.ends_with("_token_url") {
        return Some(SecretKind::NotSecret);
    }
    if ident.ends_with("_path")
        && (ident.contains("private_key")
            || ident.contains("credential")
            || ident.contains("service_account")
            || ident.contains("json"))
    {
        return Some(SecretKind::SecretPath);
    }
    if ident == "password"
        || ident == "sasl_password"
        || ident == "connection_string"
        || ident == "private_key_pem"
        || ident == "credentials_json"
        || ident == "api_key"
        || ident == "account_key"
        || ident == "sas_token"
        || ident == "consumer_key"
        || ident == "oauth_consumer_key"
        || ident.ends_with("_secret")
        || ident.ends_with("_token")
        || ident == "token"
        || ident == "developer_token"
    {
        return Some(SecretKind::Secret);
    }
    None
}

fn validate_secrets(crate_name: &str, fields: &[FieldSpec]) -> Result<(), String> {
    for field in fields {
        let leaf = field
            .yaml_path
            .rsplit('.')
            .next()
            .unwrap_or(field.ident.as_str());
        if let Some(expected) = heuristic_secret(leaf) {
            match (expected, field.secret) {
                (_, SecretKind::Secret | SecretKind::SecretPath | SecretKind::NotSecret) => {}
                (
                    SecretKind::Secret | SecretKind::SecretPath | SecretKind::NotSecret,
                    SecretKind::None,
                ) => {
                    return Err(format!(
                        "{crate_name}.{} matches the secret heuristic but is unmarked; add #[skippr(secret|secret_path|not_secret)]",
                        field.ident
                    ));
                }
                _ => {}
            }
        }
    }
    Ok(())
}

pub fn to_kebab(plugin_name: &str) -> String {
    let chars: Vec<char> = plugin_name.chars().collect();
    let mut out = String::new();
    for (i, c) in chars.iter().copied().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                let prev = chars[i - 1];
                let next_lower = chars
                    .get(i + 1)
                    .copied()
                    .map(|n| n.is_lowercase())
                    .unwrap_or(false);
                if prev.is_lowercase()
                    || prev.is_ascii_digit()
                    || (prev.is_uppercase() && next_lower)
                {
                    out.push('-');
                }
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

pub fn emit_kinds_rs(plugins: &[PluginSpec]) -> String {
    let mut out = String::from("// @generated by skippr-connect-gen. Do not edit.\n\n");
    out.push_str("#[derive(Clone, Copy, Debug, Eq, PartialEq)]\n");
    out.push_str("pub enum ConnectPlugin {\n");
    for plugin in plugins {
        let variant = format!("{:?}{}", plugin.kind, plugin.plugin_name);
        out.push_str(&format!("    {variant},\n"));
    }
    out.push_str("}\n\n");
    out.push_str("impl ConnectPlugin {\n");
    out.push_str("    pub fn role(self) -> ConnectRole {\n        match self {\n");
    for plugin in plugins {
        let variant = format!("{:?}{}", plugin.kind, plugin.plugin_name);
        let role = match plugin.kind {
            PluginKind::DataSource => "DataSource",
            PluginKind::DataSink => "DataSink",
            PluginKind::SchemaSink => "SchemaSink",
        };
        out.push_str(&format!(
            "            Self::{variant} => ConnectRole::{role},\n"
        ));
    }
    out.push_str("        }\n    }\n");
    out.push_str("    pub fn plugin_name(self) -> &'static str {\n        match self {\n");
    for plugin in plugins {
        let variant = format!("{:?}{}", plugin.kind, plugin.plugin_name);
        out.push_str(&format!(
            "            Self::{variant} => \"{}\",\n",
            plugin.plugin_name
        ));
    }
    out.push_str("        }\n    }\n");
    out.push_str(
        "    pub fn secret_fields(self) -> &'static [&'static str] {\n        match self {\n",
    );
    for plugin in plugins {
        let variant = format!("{:?}{}", plugin.kind, plugin.plugin_name);
        let secrets: Vec<String> = plugin
            .fields
            .iter()
            .filter(|f| matches!(f.secret, SecretKind::Secret))
            .map(|f| format!("\"{}\"", f.yaml_path))
            .collect();
        out.push_str(&format!(
            "            Self::{variant} => &[{}],\n",
            secrets.join(", ")
        ));
    }
    out.push_str("        }\n    }\n");
    out.push_str(
        "    pub fn required_fields(self) -> &'static [&'static str] {\n        match self {\n",
    );
    for plugin in plugins {
        let variant = format!("{:?}{}", plugin.kind, plugin.plugin_name);
        let required: Vec<String> = plugin
            .fields
            .iter()
            .filter(|f| !f.optional && !is_complex_ty(&f.rust_ty))
            .map(|f| format!("\"{}\"", f.yaml_path))
            .collect();
        out.push_str(&format!(
            "            Self::{variant} => &[{}],\n",
            required.join(", ")
        ));
    }
    out.push_str("        }\n    }\n");
    out.push_str(
        "    pub fn yaml_path_for(self, ident: &str) -> Option<&'static str> {\n        match (self, ident) {\n",
    );
    for plugin in plugins {
        let variant = format!("{:?}{}", plugin.kind, plugin.plugin_name);
        for field in &plugin.fields {
            if is_complex_ty(&field.rust_ty) {
                continue;
            }
            if matches!(
                field.ident.as_str(),
                "name" | "pipeline" | "save" | "data_source" | "data_sink" | "schema_sink"
            ) {
                continue;
            }
            out.push_str(&format!(
                "            (Self::{variant}, \"{}\") => Some(\"{}\"),\n",
                field.ident, field.yaml_path
            ));
        }
    }
    out.push_str("            _ => None,\n        }\n    }\n}\n\n");

    fn emit_kind_enum(out: &mut String, name: &str, plugins: &[PluginSpec]) {
        out.push_str(&format!(
            "#[derive(Clone, Copy, Debug, Eq, PartialEq)]\npub enum {name} {{\n"
        ));
        for plugin in plugins {
            out.push_str(&format!("    {},\n", ident_ok(&plugin.plugin_name)));
        }
        out.push_str("}\n\n");
        out.push_str(&format!(
            "impl {name} {{\n    pub fn plugin(self) -> ConnectPlugin {{\n        match self {{\n"
        ));
        for plugin in plugins {
            out.push_str(&format!(
                "            Self::{} => ConnectPlugin::{:?}{},\n",
                ident_ok(&plugin.plugin_name),
                plugin.kind,
                plugin.plugin_name
            ));
        }
        out.push_str("        }\n    }\n}\n\n");
    }
    let sources: Vec<_> = plugins
        .iter()
        .filter(|p| p.kind == PluginKind::DataSource)
        .cloned()
        .collect();
    let sinks: Vec<_> = plugins
        .iter()
        .filter(|p| p.kind == PluginKind::DataSink)
        .cloned()
        .collect();
    let schemas: Vec<_> = plugins
        .iter()
        .filter(|p| p.kind == PluginKind::SchemaSink)
        .cloned()
        .collect();
    emit_kind_enum(&mut out, "DataSource", &sources);
    emit_kind_enum(&mut out, "DataSink", &sinks);
    emit_kind_enum(&mut out, "SchemaSink", &schemas);

    out.push_str("impl ConnectBuilder<'_> {\n");
    let mut seen = BTreeSet::new();
    for plugin in plugins {
        for field in &plugin.fields {
            if !seen.insert(field.ident.clone()) {
                continue;
            }
            if is_complex_ty(&field.rust_ty) {
                continue;
            }
            if matches!(
                field.ident.as_str(),
                "name" | "pipeline" | "save" | "data_source" | "data_sink" | "schema_sink"
            ) {
                continue;
            }
            out.push_str(&format!(
                "    pub fn {}(mut self, value: impl Into<String>) -> Self {{\n        self.set_field(\"{}\", value.into());\n        self\n    }}\n",
                ident_ok(&field.ident), field.ident
            ));
        }
    }
    out.push_str("}\n");
    out
}

pub fn emit_cli_rs(plugins: &[PluginSpec]) -> String {
    let mut out = String::from("// @generated by skippr-connect-gen. Do not edit.\n");
    out.push_str("use clap::Parser;\n");
    out.push_str("use std::collections::BTreeMap;\n");
    out.push_str("use crate::connect::{ConnectPlugin, ConnectRole};\n\n");
    out.push_str("#[derive(Parser, Clone, PartialEq)]\n");
    out.push_str(
        "pub struct ConnectArgs {\n    #[command(subcommand)]\n    pub role: ConnectRoleCmd,\n}\n\n",
    );
    out.push_str("#[derive(Parser, Clone, PartialEq)]\n#[command(rename_all = \"kebab-case\")]\n");
    out.push_str("pub enum ConnectRoleCmd {\n");
    out.push_str("    DataSource {\n        #[command(subcommand)]\n        kind: DataSourceKindCmd,\n    },\n");
    out.push_str(
        "    DataSink {\n        #[command(subcommand)]\n        kind: DataSinkKindCmd,\n    },\n",
    );
    out.push_str("    SchemaSink {\n        #[command(subcommand)]\n        kind: SchemaSinkKindCmd,\n    },\n}\n\n");

    fn args_name(plugin: &PluginSpec) -> String {
        format!("{:?}{}Args", plugin.kind, plugin.plugin_name)
    }

    fn emit_kind_enum(out: &mut String, name: &str, plugins: &[PluginSpec]) {
        out.push_str(&format!(
            "#[derive(Parser, Clone, PartialEq)]\npub enum {name} {{\n"
        ));
        for plugin in plugins {
            out.push_str(&format!(
                "    #[command(name = \"{}\")]\n    {}({}),\n",
                to_kebab(&plugin.plugin_name),
                ident_ok(&plugin.plugin_name),
                args_name(plugin)
            ));
        }
        out.push_str("}\n\n");
    }

    fn emit_args_struct(out: &mut String, plugin: &PluginSpec) {
        out.push_str("#[derive(Parser, Clone, PartialEq, Default)]\n");
        out.push_str(&format!("pub struct {} {{\n", args_name(plugin)));
        out.push_str("    #[arg(long)]\n    pub pipeline: Option<String>,\n");
        out.push_str("    #[arg(long)]\n    pub name: Option<String>,\n");
        for field in &plugin.fields {
            if is_complex_ty(&field.rust_ty) {
                continue;
            }
            if field.ident == "name" || field.ident == "pipeline" {
                continue;
            }
            out.push_str(&format!(
                "    #[arg(long)]\n    pub {}: Option<String>,\n",
                ident_ok(&field.ident)
            ));
        }
        out.push_str("}\n\n");
        out.push_str(&format!(
            "impl {} {{\n    pub fn to_yaml_map(&self) -> BTreeMap<String, serde_yaml::Value> {{\n",
            args_name(plugin)
        ));
        let scalar_fields: Vec<&FieldSpec> = plugin
            .fields
            .iter()
            .filter(|field| {
                !is_complex_ty(&field.rust_ty) && field.ident != "name" && field.ident != "pipeline"
            })
            .collect();
        if scalar_fields.is_empty() {
            out.push_str("        BTreeMap::new()\n    }\n}\n\n");
        } else {
            out.push_str("        let mut fields = BTreeMap::new();\n");
            for field in scalar_fields {
                out.push_str(&format!(
                    "        if let Some(value) = &self.{} {{\n            fields.insert(\"{}\".into(), serde_yaml::Value::String(value.clone()));\n        }}\n",
                    ident_ok(&field.ident),
                    field.ident
                ));
            }
            out.push_str("        fields\n    }\n}\n\n");
        }
    }

    let sources: Vec<_> = plugins
        .iter()
        .filter(|p| p.kind == PluginKind::DataSource)
        .cloned()
        .collect();
    let sinks: Vec<_> = plugins
        .iter()
        .filter(|p| p.kind == PluginKind::DataSink)
        .cloned()
        .collect();
    let schemas: Vec<_> = plugins
        .iter()
        .filter(|p| p.kind == PluginKind::SchemaSink)
        .cloned()
        .collect();
    emit_kind_enum(&mut out, "DataSourceKindCmd", &sources);
    emit_kind_enum(&mut out, "DataSinkKindCmd", &sinks);
    emit_kind_enum(&mut out, "SchemaSinkKindCmd", &schemas);
    for plugin in plugins {
        emit_args_struct(&mut out, plugin);
    }

    out.push_str(
        "impl ConnectRoleCmd {\n    pub fn role(&self) -> ConnectRole {\n        match self {\n",
    );
    out.push_str("            Self::DataSource { .. } => ConnectRole::DataSource,\n");
    out.push_str("            Self::DataSink { .. } => ConnectRole::DataSink,\n");
    out.push_str("            Self::SchemaSink { .. } => ConnectRole::SchemaSink,\n");
    out.push_str(
        "        }\n    }\n    pub fn plugin(&self) -> ConnectPlugin {\n        match self {\n",
    );
    for plugin in &sources {
        out.push_str(&format!(
            "            Self::DataSource {{ kind: DataSourceKindCmd::{}(_) }} => ConnectPlugin::DataSource{},\n",
            ident_ok(&plugin.plugin_name),
            plugin.plugin_name
        ));
    }
    for plugin in &sinks {
        out.push_str(&format!(
            "            Self::DataSink {{ kind: DataSinkKindCmd::{}(_) }} => ConnectPlugin::DataSink{},\n",
            ident_ok(&plugin.plugin_name),
            plugin.plugin_name
        ));
    }
    for plugin in &schemas {
        out.push_str(&format!(
            "            Self::SchemaSink {{ kind: SchemaSinkKindCmd::{}(_) }} => ConnectPlugin::SchemaSink{},\n",
            ident_ok(&plugin.plugin_name),
            plugin.plugin_name
        ));
    }
    out.push_str(
        "        }\n    }\n    pub fn pipeline(&self) -> Option<&str> {\n        match self {\n",
    );
    for plugin in &sources {
        out.push_str(&format!(
            "            Self::DataSource {{ kind: DataSourceKindCmd::{}(args) }} => args.pipeline.as_deref(),\n",
            ident_ok(&plugin.plugin_name)
        ));
    }
    for plugin in &sinks {
        out.push_str(&format!(
            "            Self::DataSink {{ kind: DataSinkKindCmd::{}(args) }} => args.pipeline.as_deref(),\n",
            ident_ok(&plugin.plugin_name)
        ));
    }
    for plugin in &schemas {
        out.push_str(&format!(
            "            Self::SchemaSink {{ kind: SchemaSinkKindCmd::{}(args) }} => args.pipeline.as_deref(),\n",
            ident_ok(&plugin.plugin_name)
        ));
    }
    out.push_str(
        "        }\n    }\n    pub fn name(&self) -> Option<&str> {\n        match self {\n",
    );
    for plugin in &sources {
        out.push_str(&format!(
            "            Self::DataSource {{ kind: DataSourceKindCmd::{}(args) }} => args.name.as_deref(),\n",
            ident_ok(&plugin.plugin_name)
        ));
    }
    for plugin in &sinks {
        out.push_str(&format!(
            "            Self::DataSink {{ kind: DataSinkKindCmd::{}(args) }} => args.name.as_deref(),\n",
            ident_ok(&plugin.plugin_name)
        ));
    }
    for plugin in &schemas {
        out.push_str(&format!(
            "            Self::SchemaSink {{ kind: SchemaSinkKindCmd::{}(args) }} => args.name.as_deref(),\n",
            ident_ok(&plugin.plugin_name)
        ));
    }
    out.push_str(
        "        }\n    }\n    pub fn to_yaml_map(&self) -> BTreeMap<String, serde_yaml::Value> {\n        match self {\n",
    );
    for plugin in &sources {
        out.push_str(&format!(
            "            Self::DataSource {{ kind: DataSourceKindCmd::{}(args) }} => args.to_yaml_map(),\n",
            ident_ok(&plugin.plugin_name)
        ));
    }
    for plugin in &sinks {
        out.push_str(&format!(
            "            Self::DataSink {{ kind: DataSinkKindCmd::{}(args) }} => args.to_yaml_map(),\n",
            ident_ok(&plugin.plugin_name)
        ));
    }
    for plugin in &schemas {
        out.push_str(&format!(
            "            Self::SchemaSink {{ kind: SchemaSinkKindCmd::{}(args) }} => args.to_yaml_map(),\n",
            ident_ok(&plugin.plugin_name)
        ));
    }
    out.push_str("        }\n    }\n}\n");
    out
}

fn ident_ok(name: &str) -> String {
    if name
        .chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
    {
        format!("P{name}")
    } else if is_rust_keyword(name) {
        format!("r#{name}")
    } else {
        name.to_string()
    }
}

fn is_rust_keyword(name: &str) -> bool {
    matches!(
        name,
        "as" | "async"
            | "await"
            | "break"
            | "const"
            | "continue"
            | "crate"
            | "dyn"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "fn"
            | "for"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "Self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "type"
            | "unsafe"
            | "use"
            | "where"
            | "while"
            | "box"
            | "try"
            | "union"
            | "yield"
    )
}

fn is_complex_ty(ty: &str) -> bool {
    let name = rust_type_name(ty);
    ty.contains("HashMap")
        || ty.contains("BTreeMap")
        || ty.contains("BTreeSet")
        || ty.contains("Vec<")
        || name == "TargetEntry"
        || name == "CompetitorEntry"
}

pub fn emit_python_rs(plugins: &[PluginSpec]) -> String {
    let mut out = String::from("// @generated by skippr-connect-gen. Do not edit.\n\n");
    out.push_str("#[pyclass(eq, eq_int, from_py_object, name = \"DataSource\", module = \"skippr\")]\n#[derive(Clone, Copy, PartialEq, Eq, Hash)]\n");
    out.push_str("pub enum PyDataSource {\n");
    for plugin in plugins.iter().filter(|p| p.kind == PluginKind::DataSource) {
        out.push_str(&format!("    {},\n", ident_ok(&plugin.plugin_name)));
    }
    out.push_str("}\n\n#[pyclass(eq, eq_int, from_py_object, name = \"DataSink\", module = \"skippr\")]\n#[derive(Clone, Copy, PartialEq, Eq, Hash)]\n");
    out.push_str("pub enum PyDataSink {\n");
    for plugin in plugins.iter().filter(|p| p.kind == PluginKind::DataSink) {
        out.push_str(&format!("    {},\n", ident_ok(&plugin.plugin_name)));
    }
    out.push_str("}\n\n#[pyclass(eq, eq_int, from_py_object, name = \"SchemaSink\", module = \"skippr\")]\n#[derive(Clone, Copy, PartialEq, Eq, Hash)]\n");
    out.push_str("pub enum PySchemaSink {\n");
    for plugin in plugins.iter().filter(|p| p.kind == PluginKind::SchemaSink) {
        out.push_str(&format!("    {},\n", ident_ok(&plugin.plugin_name)));
    }
    out.push_str("}\n\n");
    fn emit_py_plugin_map(
        out: &mut String,
        fn_name: &str,
        py_ty: &str,
        kind: PluginKind,
        plugins: &[PluginSpec],
    ) {
        out.push_str(&format!(
            "pub fn {fn_name}(kind: {py_ty}) -> ConnectPlugin {{\n    match kind {{\n"
        ));
        for plugin in plugins.iter().filter(|p| p.kind == kind) {
            out.push_str(&format!(
                "        {py_ty}::{} => ConnectPlugin::{:?}{},\n",
                ident_ok(&plugin.plugin_name),
                plugin.kind,
                plugin.plugin_name
            ));
        }
        out.push_str("    }\n}\n");
    }
    emit_py_plugin_map(
        &mut out,
        "connect_plugin_data_source",
        "PyDataSource",
        PluginKind::DataSource,
        plugins,
    );
    emit_py_plugin_map(
        &mut out,
        "connect_plugin_data_sink",
        "PyDataSink",
        PluginKind::DataSink,
        plugins,
    );
    emit_py_plugin_map(
        &mut out,
        "connect_plugin_schema_sink",
        "PySchemaSink",
        PluginKind::SchemaSink,
        plugins,
    );
    out.push_str("\n#[pymethods]\nimpl PyConnect {\n");
    out.push_str(
        "    fn data_source(slf: Bound<'_, Self>, kind: PyDataSource) -> PyResult<Py<Self>> {\n        {\n            let mut this = slf.borrow_mut();\n            this.plugin = Some(connect_plugin_data_source(kind));\n            this.maybe_persist()?;\n        }\n        Ok(slf.unbind())\n    }\n",
    );
    out.push_str(
        "    fn data_sink(slf: Bound<'_, Self>, kind: PyDataSink) -> PyResult<Py<Self>> {\n        {\n            let mut this = slf.borrow_mut();\n            this.plugin = Some(connect_plugin_data_sink(kind));\n            this.maybe_persist()?;\n        }\n        Ok(slf.unbind())\n    }\n",
    );
    out.push_str(
        "    fn schema_sink(slf: Bound<'_, Self>, kind: PySchemaSink) -> PyResult<Py<Self>> {\n        {\n            let mut this = slf.borrow_mut();\n            this.plugin = Some(connect_plugin_schema_sink(kind));\n            this.maybe_persist()?;\n        }\n        Ok(slf.unbind())\n    }\n",
    );
    out.push_str(
        "    fn name(slf: Bound<'_, Self>, value: String) -> PyResult<Py<Self>> {\n        {\n            let mut this = slf.borrow_mut();\n            this.name = Some(value);\n            this.maybe_persist()?;\n        }\n        Ok(slf.unbind())\n    }\n",
    );
    let mut seen = BTreeSet::new();
    for plugin in plugins {
        for field in &plugin.fields {
            if !seen.insert(field.ident.clone()) {
                continue;
            }
            if is_complex_ty(&field.rust_ty) {
                continue;
            }
            if matches!(
                field.ident.as_str(),
                "name" | "pipeline" | "save" | "data_source" | "data_sink" | "schema_sink"
            ) {
                continue;
            }
            out.push_str(&format!(
                "    fn {}(slf: Bound<'_, Self>, value: String) -> PyResult<Py<Self>> {{\n        slf.borrow_mut().set_field(\"{}\", value)?;\n        Ok(slf.unbind())\n    }}\n",
                ident_ok(&field.ident), field.ident
            ));
        }
    }
    out.push_str("}\n");
    out
}

pub fn write_generated(root: &Path, plugins: &[PluginSpec]) -> Result<(), String> {
    let kinds = emit_kinds_rs(plugins);
    let cli = emit_cli_rs(plugins);
    let py = emit_python_rs(plugins);
    fs::write(root.join("src/connect_generated.rs"), kinds)
        .map_err(|err| format!("write connect_generated.rs: {err}"))?;
    fs::write(root.join("src/cli/connect_generated.rs"), cli)
        .map_err(|err| format!("write cli/connect_generated.rs: {err}"))?;
    fs::write(root.join("python/src/connect_generated.rs"), py)
        .map_err(|err| format!("write python connect_generated.rs: {err}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .map(PathBuf::from)
            .expect("workspace root")
    }

    #[test]
    fn discovers_s3_and_nested_http_auth_secrets() {
        let plugins = discover_plugins(&root()).expect("plugins");
        let s3 = plugins
            .iter()
            .find(|p| p.kind == PluginKind::DataSource && p.plugin_name == "S3")
            .expect("S3");
        assert!(s3
            .fields
            .iter()
            .any(|f| f.ident == "s3_bucket" && !f.optional));
        let http = plugins
            .iter()
            .find(|p| p.kind == PluginKind::DataSource && p.plugin_name == "HttpClient")
            .expect("HttpClient");
        let password = http
            .fields
            .iter()
            .find(|f| f.yaml_path == "auth.password")
            .expect("auth.password");
        assert_eq!(password.secret, SecretKind::Secret);
        let iceberg = plugins
            .iter()
            .find(|p| p.kind == PluginKind::DataSink && p.plugin_name == "Iceberg")
            .expect("Iceberg");
        assert!(iceberg
            .fields
            .iter()
            .any(|f| f.yaml_path == "catalog.token" && f.secret == SecretKind::Secret));
    }

    #[test]
    fn python_maps_typed_connect_plugin_not_names() {
        let plugins = discover_plugins(&root()).expect("plugins");
        let python = emit_python_rs(&plugins);
        assert!(
            python.contains("PyDataSource::S3 => ConnectPlugin::DataSourceS3"),
            "PyDataSource must map to ConnectPlugin, not a plugin-name string"
        );
        assert!(
            !python.contains("plugin_name_data_source"),
            "string plugin-name helpers are illegal"
        );
        let kinds = emit_kinds_rs(&plugins);
        assert!(
            !kinds.contains("from_role_and_name"),
            "ConnectPlugin identity must not be recovered from (role, name) strings"
        );
        assert!(
            kinds.contains("(Self::DataSourceOtlp, \"auth_token\") => Some(\"auth_token\")"),
            "Otlp auth_token must keep the flat yaml path"
        );
        assert!(
            kinds.contains("(Self::DataSourceHttpClient, \"auth_token\") => Some(\"auth.token\")"),
            "HttpClient auth_token must keep the nested yaml path"
        );
    }

    #[test]
    fn string_enums_stay_scalar_tagged_enums_expand() {
        let plugins = discover_plugins(&root()).expect("plugins");
        let stripe = plugins
            .iter()
            .find(|p| p.kind == PluginKind::DataSource && p.plugin_name == "Stripe")
            .expect("Stripe");
        assert!(
            stripe
                .fields
                .iter()
                .any(|f| f.ident == "stream_profile" && f.yaml_path == "stream_profile"),
            "StreamProfile is a string enum at stream_profile"
        );
        assert!(
            !stripe
                .fields
                .iter()
                .any(|f| f.ident == "stream_profile_type" || f.yaml_path == "stream_profile.type"),
            "string enums must not invent .type maps"
        );
        let apple = plugins
            .iter()
            .find(|p| p.kind == PluginKind::DataSource && p.plugin_name == "AppleAppStoreSerp")
            .expect("AppleAppStoreSerp");
        assert!(apple
            .fields
            .iter()
            .any(|f| f.ident == "entity" && f.yaml_path == "entity"));
        assert!(!apple.fields.iter().any(|f| f.ident == "entity_type"));
        let iceberg = plugins
            .iter()
            .find(|p| p.kind == PluginKind::DataSink && p.plugin_name == "Iceberg")
            .expect("Iceberg");
        assert!(
            iceberg
                .fields
                .iter()
                .any(|f| f.ident == "catalog_type" && f.yaml_path == "catalog.type"),
            "IcebergCatalogConfig is internally tagged"
        );
        assert!(
            iceberg
                .fields
                .iter()
                .any(|f| f.ident == "catalog_type" && !f.optional),
            "required tagged enum keeps the tag field required"
        );
    }

    #[test]
    fn connect_docs_flags_exist_on_generated_cli() {
        let plugins = discover_plugins(&root()).expect("plugins");
        let mut allowed_flags: BTreeSet<String> = [
            "pipeline",
            "name",
            "workspace",
            "storage_mode",
            "config",
            "log",
            "wal_storage",
            "wal_s3_bucket",
            "offset_store",
            "offset_dynamodb_table",
            "skippr_s3_bucket",
            "tenant",
            "help",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        let mut allowed_kinds: BTreeSet<String> = BTreeSet::new();
        for plugin in &plugins {
            allowed_kinds.insert(to_kebab(&plugin.plugin_name));
            for field in &plugin.fields {
                if is_complex_ty(&field.rust_ty) {
                    continue;
                }
                allowed_flags.insert(field.ident.clone());
            }
        }
        let docs = root().join("docs");
        let mut unknown = Vec::new();
        for entry in walkdir::WalkDir::new(&docs).into_iter().flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let text = fs::read_to_string(path).unwrap();
            if !text.contains("skipprd connect") {
                continue;
            }
            for finding in connect_doc_mismatches(&text, &allowed_flags, &allowed_kinds) {
                unknown.push(format!("{}: {finding}", path.display()));
            }
        }
        assert!(
            unknown.is_empty(),
            "connect docs invent CLI surface not on generated clap:\n{}",
            unknown.join("\n")
        );
    }

    #[test]
    fn kind_cli_names_use_to_kebab() {
        let plugins = discover_plugins(&root()).expect("plugins");
        let cli = emit_cli_rs(&plugins);
        for plugin in &plugins {
            let needle = format!("#[command(name = \"{}\")]", to_kebab(&plugin.plugin_name));
            assert!(
                cli.contains(&needle),
                "kind CLI name must be to_kebab, missing {needle}"
            );
        }
        assert!(
            !cli.contains("#[command(rename_all = \"kebab-case\")]\npub enum DataSourceKindCmd"),
            "kind enums must not derive names from clap rename_all"
        );
    }

    #[test]
    fn connect_docs_do_not_require_sde() {
        let docs = root().join("docs");
        let mut couplings = Vec::new();
        for entry in walkdir::WalkDir::new(&docs).into_iter().flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let text = fs::read_to_string(path).unwrap();
            if !text.contains("skipprd connect") {
                continue;
            }
            for needle in [
                "cargo test -p sde",
                "test in `sde`",
                "parallel SDE",
                "`sde model`",
            ] {
                if text.contains(needle) {
                    couplings.push(format!("{}: {needle}", path.display()));
                }
            }
        }
        assert!(
            couplings.is_empty(),
            "skipprd connect docs must not require SDE:\n{}",
            couplings.join("\n")
        );
    }

    fn connect_doc_mismatches(
        text: &str,
        allowed_flags: &BTreeSet<String>,
        allowed_kinds: &BTreeSet<String>,
    ) -> Vec<String> {
        let mut findings = Vec::new();
        let mut in_code = false;
        for part in text.split("```") {
            if in_code {
                for cmd in connect_doc_commands(part) {
                    let tokens: Vec<&str> = cmd.split_whitespace().collect();
                    for window in tokens.windows(2) {
                        if matches!(window[0], "data-source" | "data-sink" | "schema-sink")
                            && !window[1].starts_with('-')
                            && window[1] != "<kebab-name>"
                            && !allowed_kinds.contains(window[1])
                        {
                            findings.push(format!("{} {}", window[0], window[1]));
                        }
                    }
                    let mut rest = cmd.as_str();
                    while let Some(idx) = rest.find("--") {
                        rest = &rest[idx + 2..];
                        let ident: String = rest
                            .chars()
                            .take_while(|c| {
                                c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-'
                            })
                            .collect();
                        if ident.is_empty() {
                            continue;
                        }
                        let flag = ident.replace('-', "_");
                        if !allowed_flags.contains(&flag) {
                            findings.push(format!("--{ident}"));
                        }
                    }
                    let template = cmd.contains("--help") || cmd.contains("<kebab-name>");
                    if !template {
                        if !cmd.contains("--pipeline") {
                            findings.push("missing --pipeline".into());
                        }
                        if !cmd.contains("--name") {
                            findings.push("missing --name".into());
                        }
                    }
                    let mut rest = cmd.as_str();
                    while let Some(idx) = rest.find("${") {
                        let quoted_single = idx >= 1 && rest.as_bytes()[idx - 1] == b'\'';
                        let end = rest[idx..]
                            .find('}')
                            .map(|off| idx + off + 1)
                            .unwrap_or(rest.len());
                        let refer = &rest[idx..end.min(rest.len())];
                        if !quoted_single {
                            findings.push(format!("{refer} must be single-quoted"));
                        }
                        rest = &rest[idx + 2..];
                    }
                }
            }
            in_code = !in_code;
        }
        findings
    }

    fn connect_doc_commands(block: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut collecting = false;
        for line in block.lines() {
            let trimmed = line.trim();
            if !collecting {
                if trimmed.contains("skipprd") && trimmed.contains(" connect") {
                    collecting = true;
                    cur = trimmed.trim_end_matches('\\').trim().to_string();
                    if !line.trim_end().ends_with('\\') {
                        out.push(std::mem::take(&mut cur));
                        collecting = false;
                    }
                }
                continue;
            }
            cur.push(' ');
            cur.push_str(trimmed.trim_end_matches('\\').trim());
            if !line.trim_end().ends_with('\\') {
                out.push(std::mem::take(&mut cur));
                collecting = false;
            }
        }
        if collecting && !cur.is_empty() {
            out.push(cur);
        }
        out
    }
}
