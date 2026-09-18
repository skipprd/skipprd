//! Connect persist: read → parse → merge → write unresolved `skippr.yml`.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::helpers::configuration::Config;
use crate::helpers::wal_storage::{ElStorageMode, OffsetStoreKind};

const SKELETON: &str = r#"skippr:
  workspace: ""
  skipprd_el_storage_mode: local

pipelines: {}

data_sources: {}

data_sinks: {}

schema_sinks: {}
"#;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectRole {
    DataSource,
    DataSink,
    SchemaSink,
}

impl ConnectRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DataSource => "data_source",
            Self::DataSink => "data_sink",
            Self::SchemaSink => "schema_sink",
        }
    }

    fn registry(self) -> &'static str {
        match self {
            Self::DataSource => "data_sources",
            Self::DataSink => "data_sinks",
            Self::SchemaSink => "schema_sinks",
        }
    }
}

pub fn discover_config_path() -> PathBuf {
    let found = Config::find_config_file();
    if !found.is_empty() {
        return PathBuf::from(found);
    }
    PathBuf::from("./skippr.yml")
}

pub fn load_document(path: &Path) -> Result<serde_yaml::Value, String> {
    if !path.exists() {
        return serde_yaml::from_str(SKELETON)
            .map_err(|err| format!("invalid connect skeleton: {err}"));
    }
    let raw = fs::read_to_string(path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    serde_yaml::from_str(&raw).map_err(|err| format!("failed to parse {}: {err}", path.display()))
}

fn mapping<'a>(
    value: &'a mut serde_yaml::Value,
    key: &str,
) -> Result<&'a mut serde_yaml::Mapping, String> {
    if value.is_null() {
        *value = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
    }
    let map = value
        .as_mapping_mut()
        .ok_or_else(|| format!("expected mapping at {key}"))?;
    Ok(map)
}

fn child_mapping<'a>(
    parent: &'a mut serde_yaml::Mapping,
    key: &str,
) -> Result<&'a mut serde_yaml::Mapping, String> {
    let k = serde_yaml::Value::String(key.to_string());
    if !parent.contains_key(&k) || parent.get(&k).map(|v| v.is_null()).unwrap_or(false) {
        parent.insert(
            k.clone(),
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        );
    }
    parent
        .get_mut(&k)
        .and_then(|v| v.as_mapping_mut())
        .ok_or_else(|| format!("expected mapping for {key}"))
}

pub fn persist_skippr_keys(
    path: &Path,
    workspace: Option<&str>,
    storage_mode: Option<ElStorageMode>,
    wal_s3_bucket: Option<&str>,
    offset_store: Option<OffsetStoreKind>,
    offset_dynamodb_table: Option<&str>,
    skippr_s3_bucket: Option<&str>,
    tenant: Option<&str>,
) -> Result<serde_yaml::Value, String> {
    let mut doc = load_document(path)?;
    let root = mapping(&mut doc, "document")?;
    let skippr = child_mapping(root, "skippr")?;
    if let Some(workspace) = workspace {
        skippr.insert(
            serde_yaml::Value::String("workspace".into()),
            serde_yaml::Value::String(workspace.to_string()),
        );
    }
    if let Some(mode) = storage_mode {
        skippr.insert(
            serde_yaml::Value::String("skipprd_el_storage_mode".into()),
            serde_yaml::Value::String(mode.as_str().to_string()),
        );
    }
    if let Some(bucket) = wal_s3_bucket {
        skippr.insert(
            serde_yaml::Value::String("wal_s3_bucket".into()),
            serde_yaml::Value::String(bucket.to_string()),
        );
    }
    if let Some(store) = offset_store {
        skippr.insert(
            serde_yaml::Value::String("offset_store".into()),
            serde_yaml::Value::String(store.as_str().to_string()),
        );
    }
    if let Some(table) = offset_dynamodb_table {
        skippr.insert(
            serde_yaml::Value::String("offset_dynamodb_table".into()),
            serde_yaml::Value::String(table.to_string()),
        );
    }
    if let Some(bucket) = skippr_s3_bucket {
        skippr.insert(
            serde_yaml::Value::String("skippr_s3_bucket".into()),
            serde_yaml::Value::String(bucket.to_string()),
        );
    }
    if let Some(tenant) = tenant {
        skippr.insert(
            serde_yaml::Value::String("tenant".into()),
            serde_yaml::Value::String(tenant.to_string()),
        );
    }
    write_document(path, &doc)?;
    Ok(doc)
}

pub fn persist_plugin(
    path: &Path,
    pipeline: &str,
    plugin: ConnectPlugin,
    name: &str,
    fields: BTreeMap<String, serde_yaml::Value>,
) -> Result<serde_yaml::Value, String> {
    if pipeline.trim().is_empty() {
        return Err("connect requires --pipeline".into());
    }
    if name.trim().is_empty() {
        return Err("connect requires --name".into());
    }
    let role = plugin.role();
    let plugin_name = plugin.plugin_name();
    for key in plugin.secret_fields() {
        if let Some(value) = fields.get(*key) {
            let raw = value.as_str().unwrap_or("");
            if !raw.is_empty() && !(raw.starts_with("${") && raw.ends_with('}')) {
                return Err(format!(
                    "secret field '{key}' must be an unresolved ${{ENV}} reference"
                ));
            }
        }
    }

    let mut doc = load_document(path)?;
    let root = mapping(&mut doc, "document")?;
    let registry_key = role.registry();
    let reference = format!("{registry_key}.{name}");
    let schema_sink_target = {
        let pipelines = child_mapping(root, "pipelines")?;
        let pipeline_map = child_mapping(pipelines, pipeline)?;
        if role == ConnectRole::SchemaSink {
            let data_sink_ref = pipeline_map
                .get(serde_yaml::Value::String("data_sink".into()))
                .and_then(|value| value.as_str())
                .ok_or_else(|| {
                    "connect schema-sink requires a data_sink on this pipeline".to_string()
                })?;
            let sink_name = data_sink_ref.strip_prefix("data_sinks.").ok_or_else(|| {
                format!("pipeline data_sink must reference data_sinks.<name>, got {data_sink_ref}")
            })?;
            Some(sink_name.to_string())
        } else {
            pipeline_map.insert(
                serde_yaml::Value::String(role.as_str().to_string()),
                serde_yaml::Value::String(reference.clone()),
            );
            None
        }
    };

    {
        let registry = child_mapping(root, registry_key)?;
        let entry_key = serde_yaml::Value::String(name.to_string());
        if let Some(existing) = registry.get(&entry_key).and_then(|v| v.as_mapping()) {
            let existing_kind = existing
                .keys()
                .filter_map(|key| key.as_str())
                .find(|key| *key != "schema_sink")
                .unwrap_or("");
            if !existing_kind.is_empty() && existing_kind != plugin_name {
                return Err(format!(
                    "plugin name '{name}' already uses {existing_kind}, not {plugin_name}"
                ));
            }
        }
        let entry = child_mapping(registry, name)?;
        let plugin_key = serde_yaml::Value::String(plugin_name.to_string());
        if !entry.contains_key(&plugin_key)
            || entry.get(&plugin_key).map(|v| v.is_null()).unwrap_or(false)
        {
            entry.insert(
                plugin_key.clone(),
                serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
            );
        }
        let plugin_map = entry
            .get_mut(&plugin_key)
            .and_then(|v| v.as_mapping_mut())
            .ok_or_else(|| format!("expected mapping for {plugin_name}"))?;
        for (k, v) in fields {
            insert_path(plugin_map, &k, v)?;
        }
    }
    if let Some(sink_name) = schema_sink_target {
        let sinks = child_mapping(root, "data_sinks")?;
        let sink_entry = child_mapping(sinks, &sink_name)?;
        sink_entry.insert(
            serde_yaml::Value::String("schema_sink".into()),
            serde_yaml::Value::String(reference),
        );
    }
    write_document(path, &doc)?;
    Ok(doc)
}

fn insert_path(
    map: &mut serde_yaml::Mapping,
    path: &str,
    value: serde_yaml::Value,
) -> Result<(), String> {
    let parts: Vec<&str> = path.split('.').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return Err("empty connect field path".into());
    }
    insert_parts(map, &parts, value)
}

fn insert_parts(
    map: &mut serde_yaml::Mapping,
    parts: &[&str],
    value: serde_yaml::Value,
) -> Result<(), String> {
    let (head, rest) = parts.split_first().expect("non-empty path");
    let key = serde_yaml::Value::String((*head).to_string());
    if rest.is_empty() {
        map.insert(key, value);
        return Ok(());
    }
    if !map.get(&key).map(|v| v.is_mapping()).unwrap_or(false) {
        map.insert(
            key.clone(),
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        );
    }
    let child = map
        .get_mut(&key)
        .and_then(|v| v.as_mapping_mut())
        .ok_or_else(|| format!("expected mapping at {head}"))?;
    insert_parts(child, rest, value)
}

pub fn write_document(path: &Path, doc: &serde_yaml::Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
        }
    }
    let rendered = serde_yaml::to_string(doc)
        .map_err(|err| format!("failed to serialize skippr.yml: {err}"))?;
    let tmp = path.with_extension("yml.tmp");
    let mut file = fs::File::create(&tmp)
        .map_err(|err| format!("failed to write {}: {err}", tmp.display()))?;
    file.write_all(rendered.as_bytes())
        .map_err(|err| format!("failed to write {}: {err}", tmp.display()))?;
    file.sync_all()
        .map_err(|err| format!("failed to sync {}: {err}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|err| format!("failed to replace {}: {err}", path.display()))?;
    Ok(())
}

pub fn yaml_scalar_from_string(value: String) -> serde_yaml::Value {
    if value.starts_with("${") {
        return serde_yaml::Value::String(value);
    }
    match value.as_str() {
        "true" => serde_yaml::Value::Bool(true),
        "false" => serde_yaml::Value::Bool(false),
        _ => {
            if let Ok(n) = value.parse::<i64>() {
                serde_yaml::Value::Number(n.into())
            } else if let Ok(n) = value.parse::<u64>() {
                serde_yaml::Value::Number(n.into())
            } else {
                serde_yaml::Value::String(value)
            }
        }
    }
}

pub fn yaml_string_map(fields: BTreeMap<String, String>) -> BTreeMap<String, serde_yaml::Value> {
    fields
        .into_iter()
        .map(|(k, v)| (k, yaml_scalar_from_string(v)))
        .collect()
}

pub fn yaml_path_fields(
    plugin: ConnectPlugin,
    ident_fields: &BTreeMap<String, serde_yaml::Value>,
) -> Result<BTreeMap<String, serde_yaml::Value>, String> {
    let mut fields = BTreeMap::new();
    for (ident, value) in ident_fields {
        let path = plugin
            .yaml_path_for(ident)
            .ok_or_else(|| format!("{ident} is not a field of {}", plugin.plugin_name()))?;
        fields.insert(path.to_string(), value.clone());
    }
    Ok(fields)
}

#[derive(Debug)]
pub struct ConnectBuilder<'a> {
    session: &'a mut crate::api::Session,
    plugin: Option<ConnectPlugin>,
    name: Option<String>,
    fields: BTreeMap<String, serde_yaml::Value>,
}

impl crate::api::Session {
    pub fn connect(&mut self) -> Result<ConnectBuilder<'_>, String> {
        let _ = self.require_pipeline()?;
        Ok(ConnectBuilder {
            session: self,
            plugin: None,
            name: None,
            fields: BTreeMap::new(),
        })
    }
}

impl<'a> ConnectBuilder<'a> {
    pub fn data_source(mut self, kind: DataSource) -> Self {
        self.plugin = Some(kind.plugin());
        self
    }

    pub fn data_sink(mut self, kind: DataSink) -> Self {
        self.plugin = Some(kind.plugin());
        self
    }

    pub fn schema_sink(mut self, kind: SchemaSink) -> Self {
        self.plugin = Some(kind.plugin());
        self
    }

    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    fn set_field(&mut self, ident: &str, value: String) {
        self.fields
            .insert(ident.to_string(), yaml_scalar_from_string(value));
    }

    pub fn save(self) -> Result<(), String> {
        let pipeline = self.session.require_pipeline()?.to_string();
        let plugin = self
            .plugin
            .ok_or_else(|| "connect requires a plugin kind".to_string())?;
        let fields = yaml_path_fields(plugin, &self.fields)?;
        let name = self
            .name
            .filter(|n| !n.trim().is_empty())
            .ok_or_else(|| "connect requires a name".to_string())?;
        let path = self.session.connect_path();
        let doc = persist_plugin(&path, &pipeline, plugin, &name, fields)?;
        self.session.reload_from_document(path, doc)
    }
}

pub fn prompt_if_tty(label: &str) -> Result<String, io::Error> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("missing required {label}"),
        ));
    }
    eprint!("{label}: ");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

include!("connect_generated.rs");

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn persist_plugin_keeps_sibling_pipeline_and_extra_fields() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        fs::write(
            &path,
            r#"
skippr:
  workspace: demo
pipelines:
  a:
    data_source: data_sources.src_a
  b:
    data_source: data_sources.src_b
data_sources:
  src_a:
    S3:
      s3_bucket: keep-me
      s3_prefix: old
      version: "1"
  src_b:
    S3:
      s3_bucket: other
      s3_prefix: p
"#,
        )
        .unwrap();

        let mut fields = BTreeMap::new();
        fields.insert("s3_prefix".into(), serde_yaml::Value::String("new".into()));
        persist_plugin(&path, "a", ConnectPlugin::DataSinkSnowflake, "wh", {
            let mut sink = BTreeMap::new();
            sink.insert("account".into(), serde_yaml::Value::String("acme".into()));
            sink.insert("user".into(), serde_yaml::Value::String("u".into()));
            sink
        })
        .unwrap();
        persist_plugin(&path, "a", ConnectPlugin::DataSourceS3, "src_a", fields).unwrap();

        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("src_b"));
        assert!(raw.contains("version:"));
        assert!(raw.contains("s3_prefix: new") || raw.contains("s3_prefix: \"new\""));
        assert!(raw.contains("Snowflake") || raw.contains("account:"));
        let doc: serde_yaml::Value = serde_yaml::from_str(&raw).unwrap();
        let b = doc["pipelines"]["b"]["data_source"].as_str().unwrap();
        assert_eq!(b, "data_sources.src_b");
        assert_eq!(
            doc["data_sources"]["src_a"]["S3"]["version"]
                .as_str()
                .unwrap(),
            "1"
        );
        assert_eq!(
            doc["data_sources"]["src_a"]["S3"]["s3_bucket"]
                .as_str()
                .unwrap(),
            "keep-me"
        );
    }

    #[test]
    fn persist_numeric_and_bool_fields_are_yaml_scalars() {
        let mapped = yaml_string_map(BTreeMap::from([
            ("batch_size_bytes".into(), "1048576".into()),
            ("s3_prefix_ordered_depth".into(), "2".into()),
            ("path".into(), "/tmp/input".into()),
            ("token".into(), "${ENV}".into()),
        ]));
        assert_eq!(
            mapped.get("batch_size_bytes"),
            Some(&serde_yaml::Value::Number(1_048_576.into()))
        );
        assert_eq!(
            mapped.get("s3_prefix_ordered_depth"),
            Some(&serde_yaml::Value::Number(2.into()))
        );
        assert_eq!(
            mapped.get("path"),
            Some(&serde_yaml::Value::String("/tmp/input".into()))
        );
        assert_eq!(
            mapped.get("token"),
            Some(&serde_yaml::Value::String("${ENV}".into()))
        );

        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        persist_plugin(
            &path,
            "p",
            ConnectPlugin::DataSourceFile,
            "local",
            yaml_path_fields(
                ConnectPlugin::DataSourceFile,
                &yaml_string_map(BTreeMap::from([
                    ("path".into(), "/tmp/input".into()),
                    ("format".into(), "json".into()),
                    ("batch_size_bytes".into(), "1048576".into()),
                    ("batch_size_seconds".into(), "5".into()),
                ])),
            )
            .unwrap(),
        )
        .unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("batch_size_bytes: 1048576"));
        assert!(!raw.contains("batch_size_bytes: '1048576'"));
        assert!(!raw.contains("batch_size_bytes: \"1048576\""));
    }

    #[test]
    fn persist_schema_sink_attaches_to_data_sink() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        persist_plugin(
            &path,
            "p",
            ConnectPlugin::DataSinkAthena,
            "lake",
            BTreeMap::from([
                ("s3_bucket".into(), serde_yaml::Value::String("out".into())),
                ("s3_prefix".into(), serde_yaml::Value::String("p".into())),
                (
                    "athena_workgroup_name".into(),
                    serde_yaml::Value::String("bikehire".into()),
                ),
                (
                    "athena_results_s3_bucket".into(),
                    serde_yaml::Value::String("out".into()),
                ),
            ]),
        )
        .unwrap();
        persist_plugin(
            &path,
            "p",
            ConnectPlugin::SchemaSinkGlue,
            "glue",
            BTreeMap::from([(
                "glue_database_name".into(),
                serde_yaml::Value::String("bikehire".into()),
            )]),
        )
        .unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        let doc: serde_yaml::Value = serde_yaml::from_str(&raw).unwrap();
        assert_eq!(
            doc["data_sinks"]["lake"]["schema_sink"].as_str().unwrap(),
            "schema_sinks.glue"
        );
        assert!(doc["pipelines"]["p"].get("schema_sink").is_none());
        assert_eq!(
            doc["schema_sinks"]["glue"]["Glue"]["glue_database_name"]
                .as_str()
                .unwrap(),
            "bikehire"
        );
        let err = persist_plugin(
            &path,
            "other",
            ConnectPlugin::SchemaSinkGlue,
            "glue2",
            BTreeMap::new(),
        )
        .unwrap_err();
        assert!(err.contains("requires a data_sink"));
    }

    #[test]
    fn persist_rejects_kind_change_on_same_name() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        persist_plugin(
            &path,
            "p",
            ConnectPlugin::DataSourceS3,
            "src",
            BTreeMap::from([("s3_bucket".into(), serde_yaml::Value::String("b".into()))]),
        )
        .unwrap();
        let err = persist_plugin(
            &path,
            "p",
            ConnectPlugin::DataSourceFile,
            "src",
            BTreeMap::new(),
        )
        .unwrap_err();
        assert!(err.contains("already uses S3"));
    }

    #[test]
    fn persist_rejects_plaintext_secret() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        let err = persist_plugin(
            &path,
            "p",
            ConnectPlugin::DataSinkPostgres,
            "db",
            BTreeMap::from([(
                "password".into(),
                serde_yaml::Value::String("hunter2".into()),
            )]),
        )
        .unwrap_err();
        assert!(err.contains("${ENV}"));
    }

    #[test]
    fn persist_skippr_workspace_and_storage_mode() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        persist_skippr_keys(
            &path,
            Some("bikehire"),
            Some(ElStorageMode::Local),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("workspace: bikehire"));
        assert!(raw.contains("skipprd_el_storage_mode: local"));
    }

    #[test]
    fn persist_nested_secret_path_under_auth() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        persist_plugin(
            &path,
            "p",
            ConnectPlugin::DataSourceHttpClient,
            "http",
            BTreeMap::from([
                ("url".into(), serde_yaml::Value::String("https://ex".into())),
                (
                    "auth.password".into(),
                    serde_yaml::Value::String("${HTTP_PASSWORD}".into()),
                ),
            ]),
        )
        .unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        let doc: serde_yaml::Value = serde_yaml::from_str(&raw).unwrap();
        assert_eq!(
            doc["data_sources"]["http"]["HttpClient"]["url"]
                .as_str()
                .unwrap(),
            "https://ex"
        );
        assert_eq!(
            doc["data_sources"]["http"]["HttpClient"]["auth"]["password"]
                .as_str()
                .unwrap(),
            "${HTTP_PASSWORD}"
        );
    }

    #[test]
    fn session_connect_save_writes_source() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        persist_skippr_keys(
            &path,
            Some("ws"),
            Some(ElStorageMode::Local),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let mut session = crate::api::Session::from_yml(&path, Some("p")).unwrap();
        session
            .connect()
            .unwrap()
            .data_source(DataSource::S3)
            .name("sample")
            .s3_bucket("b")
            .s3_prefix("p")
            .save()
            .unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        let doc: serde_yaml::Value = serde_yaml::from_str(&raw).unwrap();
        assert_eq!(
            doc["pipelines"]["p"]["data_source"].as_str().unwrap(),
            "data_sources.sample"
        );
        assert_eq!(
            doc["data_sources"]["sample"]["S3"]["s3_bucket"]
                .as_str()
                .unwrap(),
            "b"
        );
    }

    #[test]
    fn session_connect_auth_token_path_follows_plugin() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        persist_skippr_keys(
            &path,
            Some("ws"),
            Some(ElStorageMode::Local),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let mut session = crate::api::Session::from_yml(&path, Some("p")).unwrap();
        session
            .connect()
            .unwrap()
            .data_source(DataSource::Otlp)
            .name("otel")
            .auth_token("${OTLP_TOKEN}")
            .save()
            .unwrap();
        let doc: serde_yaml::Value =
            serde_yaml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            doc["data_sources"]["otel"]["Otlp"]["auth_token"]
                .as_str()
                .unwrap(),
            "${OTLP_TOKEN}"
        );
        assert!(doc["data_sources"]["otel"]["Otlp"].get("auth").is_none());

        let err = session
            .connect()
            .unwrap()
            .data_source(DataSource::Otlp)
            .name("otel")
            .auth_token("hunter2")
            .save()
            .unwrap_err();
        assert!(err.contains("${ENV}"));

        session
            .connect()
            .unwrap()
            .data_source(DataSource::HttpClient)
            .name("http")
            .url("https://ex")
            .auth_token("${HTTP_TOKEN}")
            .save()
            .unwrap();
        let doc: serde_yaml::Value =
            serde_yaml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            doc["data_sources"]["http"]["HttpClient"]["auth"]["token"]
                .as_str()
                .unwrap(),
            "${HTTP_TOKEN}"
        );
    }

    #[test]
    fn connect_without_pipeline_is_rejected() {
        let config = Config::new();
        let mut session = crate::api::Session::from_config(config, None).unwrap();
        let err = session.connect().unwrap_err();
        assert!(err.contains("pipeline"));
    }

    #[test]
    fn persist_unresolved_secret_does_not_require_env() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        persist_plugin(
            &path,
            "p",
            ConnectPlugin::DataSinkPostgres,
            "db",
            BTreeMap::from([
                ("user".into(), serde_yaml::Value::String("u".into())),
                ("database".into(), serde_yaml::Value::String("d".into())),
                (
                    "password".into(),
                    serde_yaml::Value::String("${POSTGRES_PASSWORD}".into()),
                ),
            ]),
        )
        .unwrap();
        let mut session = crate::api::Session::from_config(Config::new(), Some("p")).unwrap();
        let doc = load_document(&path).unwrap();
        session.reload_from_document(path.clone(), doc).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("${POSTGRES_PASSWORD}"));
    }
}
