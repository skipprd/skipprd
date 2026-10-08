//! Connect persist: read → parse → merge → write unresolved `skippr.yml`.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::Serialize;

use crate::helpers::configuration::{
    is_env_ref, Config, PairedSink, Registry, Skippr, WholeConfigCheck,
};
use crate::helpers::plugin_config::DataSinkEntry;
use crate::helpers::wal_storage::{ElStorageMode, SkipprStore, SkipprStoreKind};

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

    fn registry(self) -> Registry {
        match self {
            Self::DataSource => Registry::DataSources,
            Self::DataSink => Registry::DataSinks,
            Self::SchemaSink => Registry::SchemaSinks,
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

/// Connect and `save` write YAML; a `.json` / `.toml` config is read-only to them.
fn require_yaml_path(path: &Path) -> Result<(), String> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("yml" | "yaml") => Ok(()),
        _ => Err(format!(
            "{} is not a YAML (.yml / .yaml) file; connect and save write YAML only",
            path.display()
        )),
    }
}

/// Registry entry names are referenced as `<registry>.<name>`.
pub fn check_entry_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() || name.contains('.') {
        return Err(format!(
            "entry name {name:?} must be non-empty and contain no '.'"
        ));
    }
    Ok(())
}

pub fn load_document(path: &Path) -> Result<serde_yaml::Value, String> {
    require_yaml_path(path)?;
    if !path.exists() {
        return Ok(serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
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

/// Plugin or root fields keyed by top-level YAML key.
type YamlFields = BTreeMap<String, serde_yaml::Value>;

/// The one field merge: each top-level key replaces that key wholesale (a nested
/// `object_store` or `auth` is one value); keys the caller does not name are kept.
fn merge_fields(map: &mut serde_yaml::Mapping, fields: YamlFields) {
    for (key, value) in fields {
        map.insert(key.into(), value);
    }
}

/// Fold dotted CLI paths (`auth.password`) into top-level nested values.
fn nest_fields(fields: YamlFields) -> Result<YamlFields, String> {
    let mut nested = serde_yaml::Mapping::new();
    for (path, value) in fields {
        insert_path(&mut nested, &path, value)?;
    }
    Ok(nested
        .into_iter()
        .filter_map(|(key, value)| key.as_str().map(|key| (key.to_string(), value)))
        .collect())
}

fn top_level_fields<T: Serialize>(value: &T, what: &str) -> Result<YamlFields, String> {
    match serde_yaml::to_value(value).map_err(|err| format!("failed to serialize {what}: {err}"))? {
        serde_yaml::Value::Mapping(map) => map
            .into_iter()
            .map(|(key, value)| {
                key.as_str()
                    .map(|key| (key.to_string(), value))
                    .ok_or_else(|| format!("{what} has a non-string key"))
            })
            .collect(),
        serde_yaml::Value::Null => Ok(YamlFields::new()),
        _ => Err(format!("{what} must serialize to a mapping")),
    }
}

/// Secret fields hold `${ENV}` references only; plaintext never reaches skippr.yml.
fn validate_secret_refs(plugin: ConnectPlugin, fields: &YamlFields) -> Result<(), String> {
    for key in plugin.secret_fields() {
        let Some(value) = yaml_get_path(fields, key) else {
            continue;
        };
        if value.is_null() || value.as_str().is_some_and(is_env_ref) {
            continue;
        }
        return Err(format!(
            "secret field '{key}' must be an unresolved ${{ENV}} reference like ${{NAME}}"
        ));
    }
    Ok(())
}

/// A registry entry's plugin kind is fixed once written. Kinds match
/// case-insensitively, like the engine's plugin lookup.
fn check_same_plugin(name: &str, existing: &str, plugin_name: &str) -> Result<(), String> {
    if existing.eq_ignore_ascii_case(plugin_name) {
        return Ok(());
    }
    Err(format!(
        "plugin name '{name}' already uses {existing}, not {plugin_name}"
    ))
}

/// Merge one plugin entry into `<registry>.<name>`. The entry's plugin kind is
/// fixed once written; a different kind under the same name is an error.
fn merge_registry_entry(
    root: &mut serde_yaml::Mapping,
    registry: Registry,
    name: &str,
    plugin: ConnectPlugin,
    fields: YamlFields,
    schema_sink: Option<&str>,
) -> Result<(), String> {
    let plugin_name = plugin.plugin_name();
    let entry = child_mapping(child_mapping(root, registry.key())?, name)?;
    if let Some(existing_kind) = plugin_key(entry).map(str::to_string) {
        check_same_plugin(name, &existing_kind, plugin_name)?;
        if let Some(body) = entry.remove(existing_kind.as_str()) {
            entry.insert(plugin_name.into(), body);
        }
    }
    merge_fields(child_mapping(entry, plugin_name)?, fields);
    if let Some(schema_sink) = schema_sink {
        entry.insert(
            "schema_sink".into(),
            Registry::SchemaSinks.reference(schema_sink).into(),
        );
    }
    Ok(())
}

fn connect_plugin_for(
    registry: Registry,
    entry: &crate::helpers::plugin_config::PluginConfigEntry,
) -> Result<ConnectPlugin, String> {
    let name = entry.plugin_name.as_str();
    let plugin = match registry {
        Registry::DataSources => DataSource::parse(name).map(DataSource::plugin),
        Registry::DataSinks | Registry::DeadletterSinks => {
            DataSink::parse(name).map(DataSink::plugin)
        }
        Registry::SchemaSinks => SchemaSink::parse(name).map(SchemaSink::plugin),
    };
    plugin.ok_or_else(|| format!("{name} is not a {} plugin", registry.key()))
}

/// The one config merge, shared by `skipprd connect`, `Config.save`, and
/// in-memory `Config` registration. Every top-level key `update` sets replaces
/// that key; keys and entries it does not set are kept. `store` is replaced
/// whole, local storage drops `skippr_s3_bucket`, a named entry keeps its
/// plugin kind, and secret fields must be `${ENV}` references.
pub fn merge_config(root: &mut serde_yaml::Mapping, update: &Config) -> Result<(), String> {
    if let Some(skippr) = &update.skippr {
        let fields = top_level_fields(skippr, "skippr")?;
        let skippr_map = child_mapping(root, "skippr")?;
        merge_fields(skippr_map, fields);
        let local_without_bucket = skippr.skipprd_el_storage_mode == Some(ElStorageMode::Local)
            && skippr.skippr_s3_bucket.is_none();
        if local_without_bucket {
            skippr_map.remove("skippr_s3_bucket");
        }
        if skippr_map.is_empty() {
            root.remove("skippr");
        }
    }
    let plugin_registries = [
        (Registry::DataSources, update.data_sources.as_ref()),
        (Registry::SchemaSinks, update.schema_sinks.as_ref()),
    ];
    for (registry, entries) in plugin_registries {
        for (name, entry) in entries.into_iter().flatten() {
            let plugin = connect_plugin_for(registry, entry)?;
            let fields = top_level_fields(&entry.config, entry.plugin_name.as_str())?;
            merge_registry_entry(root, registry, name, plugin, fields, None)?;
        }
    }
    let sink_registries = [
        (Registry::DataSinks, update.data_sinks.as_ref()),
        (Registry::DeadletterSinks, update.deadletter_sinks.as_ref()),
    ];
    for (registry, entries) in sink_registries {
        for (name, entry) in entries.into_iter().flatten() {
            let (plugin, fields, schema_sink) = sink_entry_parts(registry, entry)?;
            merge_registry_entry(root, registry, name, plugin, fields, schema_sink.as_deref())?;
        }
    }
    if !update.pipelines.is_empty() {
        let pipelines = child_mapping(root, "pipelines")?;
        for (name, pipeline) in &update.pipelines {
            merge_fields(
                child_mapping(pipelines, name)?,
                top_level_fields(pipeline, name)?,
            );
        }
    }
    if let Some(dbt) = &update.dbt {
        merge_fields(child_mapping(root, "dbt")?, top_level_fields(dbt, "dbt")?);
    }
    if let Some(vector_sources) = &update.vector_sources {
        merge_fields(
            child_mapping(root, "vector_sources")?,
            top_level_fields(vector_sources, "vector_sources")?,
        );
    }
    Ok(())
}

/// What every written or merged config must satisfy: it parses, every entry
/// is a known plugin for its registry, secret fields are `${ENV}` refs, and
/// every pipeline that has a `data_source` validates. A pipeline is built one
/// entry at a time, so one without a source yet is not checked.
fn parse_document(doc: serde_yaml::Value, what: &str) -> Result<Config, String> {
    let config: Config = serde_yaml::from_value(doc)
        .map_err(|err| format!("merged {what} is not a valid config: {err}"))?;
    let plugin_entries = [
        (Registry::DataSources, config.data_sources.as_ref()),
        (Registry::SchemaSinks, config.schema_sinks.as_ref()),
    ]
    .into_iter()
    .flat_map(|(registry, entries)| {
        entries
            .into_iter()
            .flatten()
            .map(move |(n, e)| (registry, n, e))
    });
    let sink_entries = [
        (Registry::DataSinks, config.data_sinks.as_ref()),
        (Registry::DeadletterSinks, config.deadletter_sinks.as_ref()),
    ]
    .into_iter()
    .flat_map(|(registry, entries)| {
        entries
            .into_iter()
            .flatten()
            .map(move |(n, e)| (registry, n, &e.config))
    });
    for (registry, name, entry) in plugin_entries.chain(sink_entries) {
        let plugin = connect_plugin_for(registry, entry)?;
        let fields = top_level_fields(&entry.config, &entry.plugin_name)?;
        validate_secret_refs(plugin, &fields)
            .map_err(|err| format!("{}.{name}: {err}", registry.key()))?;
    }
    for (name, _) in config
        .pipelines
        .iter()
        .filter(|(_, p)| p.data_source.is_some())
    {
        config.validate_pipeline(name)?;
    }
    config.check_whole_config(WholeConfigCheck::Write)?;
    Ok(config)
}

/// `update` merged over `base` in memory: the config `save_config` would write.
pub fn merge(base: &Config, update: &Config) -> Result<Config, String> {
    let mut doc =
        serde_yaml::to_value(base).map_err(|err| format!("failed to serialize config: {err}"))?;
    merge_config(mapping(&mut doc, "config")?, update)?;
    parse_document(doc, "config")
}

/// Merge-write `update` into the YAML at `path` (or a new file). The written
/// document must pass `parse_document`.
pub fn save_config(path: &Path, update: &Config) -> Result<(), String> {
    let mut doc = load_document(path)?;
    merge_config(mapping(&mut doc, "document")?, update)?;
    parse_document(doc.clone(), &path.display().to_string())?;
    write_document(path, &doc)
}

/// Root `skippr:` keys from CLI global flags; `None` when no flag is set.
pub fn skippr_keys(
    workspace: Option<&str>,
    storage_mode: Option<ElStorageMode>,
    wal_s3_bucket: Option<&str>,
    store_type: Option<SkipprStoreKind>,
    store_name: Option<&str>,
    skippr_s3_bucket: Option<&str>,
    tenant: Option<&str>,
) -> Result<Option<Skippr>, String> {
    let store = match (store_type, store_name) {
        (Some(kind), name) => Some(SkipprStore {
            kind,
            name: name.map(str::to_string),
        }),
        (None, None) => None,
        (None, Some(_)) => return Err("--store-name requires --store-type".into()),
    };
    let skippr = Skippr {
        workspace: workspace.map(str::to_string),
        tenant: tenant.map(str::to_string),
        skippr_s3_bucket: skippr_s3_bucket.map(str::to_string),
        skipprd_el_storage_mode: storage_mode,
        wal_s3_bucket: wal_s3_bucket.map(str::to_string),
        store,
    };
    Ok((skippr != Skippr::default()).then_some(skippr))
}

/// The registries a data sink plugin can be written to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SinkRegistry {
    Data,
    Deadletter,
}

impl SinkRegistry {
    const ALL: [Self; 2] = [Self::Data, Self::Deadletter];

    pub fn registry(self) -> Registry {
        match self {
            Self::Data => Registry::DataSinks,
            Self::Deadletter => Registry::DeadletterSinks,
        }
    }
}

/// An entry that can belong to a [`PairedSink`] group.
#[derive(Clone, Debug, Eq, PartialEq)]
enum PairMember {
    Sink(SinkRegistry, String),
    Schema(String),
}

impl PairMember {
    fn of(registry: Registry, name: &str) -> Option<Self> {
        let name = name.to_string();
        match registry {
            Registry::DataSinks => Some(Self::Sink(SinkRegistry::Data, name)),
            Registry::DeadletterSinks => Some(Self::Sink(SinkRegistry::Deadletter, name)),
            Registry::SchemaSinks => Some(Self::Schema(name)),
            Registry::DataSources => None,
        }
    }

    fn registry(&self) -> Registry {
        match self {
            Self::Sink(sinks, _) => sinks.registry(),
            Self::Schema(_) => Registry::SchemaSinks,
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::Sink(_, name) | Self::Schema(name) => name,
        }
    }

    fn label(&self) -> String {
        self.registry().reference(self.name())
    }

    fn plugin_name(&self, paired: PairedSink) -> &'static str {
        match self {
            Self::Sink(..) => paired.data_sink().plugin_name(),
            Self::Schema(_) => paired.schema_sink().plugin_name(),
        }
    }
}

/// An entry's plugin key: its one key other than `schema_sink`.
fn plugin_key(entry: &serde_yaml::Mapping) -> Option<&str> {
    entry
        .keys()
        .filter_map(serde_yaml::Value::as_str)
        .find(|key| *key != "schema_sink")
}

fn registry_entry<'a>(
    root: &'a serde_yaml::Mapping,
    registry: Registry,
    name: &str,
) -> Option<&'a serde_yaml::Mapping> {
    root.get(registry.key())?.get(name)?.as_mapping()
}

fn paired_plugin(plugin: ConnectPlugin) -> Option<PairedSink> {
    match plugin.role() {
        ConnectRole::DataSink => DataSink::parse(plugin.plugin_name()).and_then(PairedSink::of),
        ConnectRole::SchemaSink => {
            SchemaSink::parse(plugin.plugin_name()).and_then(PairedSink::of_schema)
        }
        ConnectRole::DataSource => None,
    }
}

/// A [`PairedSink`] schema sink and every data or deadletter sink of that
/// plugin linking it: one config. `None` when the schema sink is of another
/// plugin, which the whole-config check refuses.
fn pair_group(
    root: &serde_yaml::Mapping,
    paired: PairedSink,
    schema_name: &str,
) -> Option<Vec<PairMember>> {
    let schema_kind = registry_entry(root, Registry::SchemaSinks, schema_name).and_then(plugin_key);
    if schema_kind.is_some_and(|kind| SchemaSink::parse(kind) != Some(paired.schema_sink())) {
        return None;
    }
    let reference = Registry::SchemaSinks.reference(schema_name);
    let mut members = vec![PairMember::Schema(schema_name.to_string())];
    for sinks in SinkRegistry::ALL {
        let entries = root
            .get(sinks.registry().key())
            .and_then(serde_yaml::Value::as_mapping)
            .into_iter()
            .flatten();
        for (name, entry) in entries {
            let (Some(name), Some(entry)) = (name.as_str(), entry.as_mapping()) else {
                continue;
            };
            let linked = entry.get("schema_sink").and_then(serde_yaml::Value::as_str)
                == Some(reference.as_str());
            let kind = plugin_key(entry)
                .and_then(DataSink::parse)
                .and_then(PairedSink::of);
            if linked && kind == Some(paired) {
                members.push(PairMember::Sink(sinks, name.to_string()));
            }
        }
    }
    Some(members)
}

/// The group `written` joins with this write, if it is a [`PairedSink`].
fn written_pair_group(
    root: &serde_yaml::Mapping,
    written: &PairMember,
    plugin: ConnectPlugin,
    schema_sink: Option<&str>,
) -> Result<Option<(PairedSink, Vec<PairMember>)>, String> {
    let Some(paired) = paired_plugin(plugin) else {
        return Ok(None);
    };
    let schema_name = match (written, schema_sink) {
        (PairMember::Schema(name), _) => name.clone(),
        (PairMember::Sink(..), Some(schema)) => schema.to_string(),
        (PairMember::Sink(sinks, name), None) => {
            let reference = registry_entry(root, sinks.registry(), name)
                .and_then(|entry| entry.get("schema_sink"))
                .and_then(serde_yaml::Value::as_str);
            match reference {
                Some(reference) => Config::parse_registry_ref(reference, Registry::SchemaSinks)?,
                None => return Ok(None),
            }
        }
    };
    let Some(mut members) = pair_group(root, paired, &schema_name) else {
        return Ok(None);
    };
    if !members.contains(written) {
        members.push(written.clone());
    }
    Ok(Some((paired, members)))
}

/// The one config a group holds: every member with fields must hold the same
/// fields, and a group with none holds an empty config.
fn group_config(
    root: &serde_yaml::Mapping,
    paired: PairedSink,
    members: &[PairMember],
) -> Result<serde_yaml::Mapping, String> {
    let mut held: Option<(&PairMember, &serde_yaml::Mapping)> = None;
    for member in members {
        let body = registry_entry(root, member.registry(), member.name())
            .and_then(|entry| entry.get(plugin_key(entry)?))
            .and_then(serde_yaml::Value::as_mapping)
            .filter(|body| !body.is_empty());
        match (held, body) {
            (_, None) => {}
            (None, Some(body)) => held = Some((member, body)),
            (Some((first, config)), Some(body)) if config != body => {
                return Err(format!(
                    "{} and {}: paired {} configs must be equal before a write changes them",
                    first.label(),
                    member.label(),
                    paired.plugin_name()
                ));
            }
            (Some(_), Some(_)) => {}
        }
    }
    Ok(held.map(|(_, config)| config.clone()).unwrap_or_default())
}

/// Write one registry entry. A [`PairedSink`] group holds one config: the
/// write is refused if its members already differ, and otherwise every
/// member takes that config with `fields` merged in, so an empty member
/// starts as a copy. Secret fields are checked against the entry named.
fn write_entry(
    root: &mut serde_yaml::Mapping,
    registry: Registry,
    name: &str,
    plugin: ConnectPlugin,
    fields: YamlFields,
    schema_sink: Option<&str>,
) -> Result<(), String> {
    validate_secret_refs(plugin, &fields)
        .map_err(|err| format!("{}: {err}", registry.reference(name)))?;
    let group = match PairMember::of(registry, name) {
        Some(written) => written_pair_group(root, &written, plugin, schema_sink)?,
        None => None,
    };
    let held = group
        .as_ref()
        .map(|(paired, members)| group_config(root, *paired, members))
        .transpose()?;
    merge_registry_entry(root, registry, name, plugin, fields.clone(), schema_sink)?;
    let (Some((paired, members)), Some(mut config)) = (group, held) else {
        return Ok(());
    };
    merge_fields(&mut config, fields);
    for member in members {
        let entry = child_mapping(child_mapping(root, member.registry().key())?, member.name())?;
        if let Some(key) = plugin_key(entry).map(str::to_string) {
            entry.remove(key.as_str());
        }
        entry.insert(member.plugin_name(paired).into(), config.clone().into());
    }
    Ok(())
}

/// Register one data or deadletter sink in memory with the `skipprd connect`
/// pairing rule. Python registration uses this; `merge` does not pair.
pub fn register_sink(
    base: &Config,
    sinks: SinkRegistry,
    name: &str,
    entry: &DataSinkEntry,
) -> Result<Config, String> {
    let mut doc =
        serde_yaml::to_value(base).map_err(|err| format!("failed to serialize config: {err}"))?;
    let root = mapping(&mut doc, "config")?;
    let registry = sinks.registry();
    let (plugin, fields, schema_sink) = sink_entry_parts(registry, entry)?;
    write_entry(root, registry, name, plugin, fields, schema_sink.as_deref())?;
    parse_document(doc, "config")
}

/// A sink entry as a plugin, its fields, and the schema sink it links.
fn sink_entry_parts(
    registry: Registry,
    entry: &DataSinkEntry,
) -> Result<(ConnectPlugin, YamlFields, Option<String>), String> {
    let plugin = connect_plugin_for(registry, &entry.config)?;
    let fields = top_level_fields(&entry.config.config, &entry.config.plugin_name)?;
    let schema_sink = entry
        .schema_sink
        .as_deref()
        .map(|reference| Config::parse_registry_ref(reference, Registry::SchemaSinks))
        .transpose()?;
    Ok((plugin, fields, schema_sink))
}

/// One `skipprd connect`: root flags and the plugin entry land in one write,
/// or nothing is written.
pub fn persist_plugin(
    path: &Path,
    skippr: Option<&Skippr>,
    pipeline: &str,
    plugin: ConnectPlugin,
    name: &str,
    fields: YamlFields,
) -> Result<serde_yaml::Value, String> {
    if pipeline.trim().is_empty() {
        return Err("connect requires --pipeline".into());
    }
    if name.trim().is_empty() {
        return Err("connect requires --name".into());
    }
    check_entry_name(name)?;
    let role = plugin.role();
    let registry = role.registry();
    let fields = nest_fields(fields)?;
    let mut doc = load_document(path)?;
    let root = mapping(&mut doc, "document")?;
    if let Some(skippr) = skippr {
        merge_config(
            root,
            &Config {
                skippr: Some(skippr.clone()),
                ..Config::default()
            },
        )?;
    }
    let schema_sink_target = {
        let pipelines = child_mapping(root, "pipelines")?;
        let pipeline_map = child_mapping(pipelines, pipeline)?;
        if role == ConnectRole::SchemaSink {
            let data_sink_ref = pipeline_map
                .get("data_sink")
                .and_then(|value| value.as_str())
                .ok_or_else(|| {
                    "connect schema-sink requires a data_sink on this pipeline".to_string()
                })?;
            Some(Config::parse_registry_ref(
                data_sink_ref,
                Registry::DataSinks,
            )?)
        } else {
            pipeline_map.insert(role.as_str().into(), registry.reference(name).into());
            None
        }
    };
    if let Some(sink_name) = schema_sink_target {
        let sinks = child_mapping(root, Registry::DataSinks.key())?;
        child_mapping(sinks, &sink_name)?.insert(
            "schema_sink".into(),
            Registry::SchemaSinks.reference(name).into(),
        );
    }
    write_entry(root, registry, name, plugin, fields, None)?;
    parse_document(doc.clone(), &path.display().to_string())?;
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

fn durable_config_sync(file: &fs::File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        // POSIX fsync. Do not use std File::sync_* on Apple: they request a
        // device barrier Darwin CI VMs reject with EIO.
        let rc = unsafe { libc::fsync(file.as_raw_fd()) };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    #[cfg(not(unix))]
    {
        file.sync_data()
    }
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
    durable_config_sync(&file).map_err(|err| format!("failed to sync {}: {err}", tmp.display()))?;
    drop(file);
    fs::rename(&tmp, path).map_err(|err| format!("failed to replace {}: {err}", path.display()))?;
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    crate::helpers::fsync::fsync_dir(parent)
        .map_err(|err| format!("failed to sync {}: {err}", parent.display()))?;
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

fn coerce_yaml_scalars(value: serde_yaml::Value) -> serde_yaml::Value {
    match value {
        serde_yaml::Value::String(s) => yaml_scalar_from_string(s),
        serde_yaml::Value::Mapping(map) => serde_yaml::Value::Mapping(
            map.into_iter()
                .map(|(key, nested)| (key, coerce_yaml_scalars(nested)))
                .collect(),
        ),
        serde_yaml::Value::Sequence(items) => {
            serde_yaml::Value::Sequence(items.into_iter().map(coerce_yaml_scalars).collect())
        }
        other => other,
    }
}

pub fn yaml_map_from_serialize<T: Serialize>(value: &T) -> BTreeMap<String, serde_yaml::Value> {
    let serialized = serde_yaml::to_value(value).expect("plugin config must serialize to YAML");
    let serde_yaml::Value::Mapping(map) = serialized else {
        panic!("plugin config must serialize to a YAML mapping");
    };
    map.into_iter()
        .filter_map(|(key, nested)| {
            let ident = key.as_str()?.to_string();
            Some((ident, coerce_yaml_scalars(nested)))
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq)]
pub struct YamlArg(pub serde_yaml::Value);

impl Serialize for YamlArg {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl FromStr for YamlArg {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        serde_yaml::from_str(s)
            .map(YamlArg)
            .map_err(|err| err.to_string())
    }
}

pub fn yaml_get_path<'a>(
    fields: &'a BTreeMap<String, serde_yaml::Value>,
    path: &str,
) -> Option<&'a serde_yaml::Value> {
    if let Some(value) = fields.get(path) {
        return Some(value);
    }
    let mut parts = path.split('.');
    let first = parts.next()?;
    let mut current = fields.get(first)?;
    for part in parts {
        current = current
            .as_mapping()?
            .get(serde_yaml::Value::String(part.to_string()))?;
    }
    Some(current)
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
    fn write_document_roundtrips_and_drops_tmp() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        let doc = serde_yaml::from_str::<serde_yaml::Value>("skippr:\n  workspace: w\n").unwrap();
        write_document(&path, &doc).unwrap();
        assert!(path.exists());
        assert!(!path.with_extension("yml.tmp").exists());
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("workspace: w"));
    }

    #[cfg(windows)]
    #[test]
    fn write_document_succeeds_without_opening_parent_as_file() {
        write_document_roundtrips_and_drops_tmp();
    }

    #[cfg(unix)]
    #[test]
    fn durable_config_sync_uses_posix_fsync() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml.tmp");
        let file = fs::File::create(&path).unwrap();
        durable_config_sync(&file).unwrap();
    }

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
        persist_plugin(&path, None, "a", ConnectPlugin::DataSinkSnowflake, "wh", {
            let mut sink = BTreeMap::new();
            sink.insert("account".into(), serde_yaml::Value::String("acme".into()));
            sink.insert("user".into(), serde_yaml::Value::String("u".into()));
            sink
        })
        .unwrap();
        persist_plugin(
            &path,
            None,
            "a",
            ConnectPlugin::DataSourceS3,
            "src_a",
            fields,
        )
        .unwrap();

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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
            "p",
            ConnectPlugin::DataSourceS3,
            "src",
            BTreeMap::from([("s3_bucket".into(), serde_yaml::Value::String("b".into()))]),
        )
        .unwrap();
        let err = persist_plugin(
            &path,
            None,
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
            None,
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
        save_config(
            &path,
            &Config {
                skippr: skippr_keys(
                    Some("bikehire"),
                    Some(ElStorageMode::Local),
                    None,
                    None,
                    None,
                    None,
                    None,
                )
                .unwrap(),
                ..Config::default()
            },
        )
        .unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("workspace: bikehire"));
        assert!(raw.contains("skipprd_el_storage_mode: local"));
    }

    #[test]
    fn persist_skippr_store_type_and_name() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        save_config(
            &path,
            &Config {
                skippr: skippr_keys(
                    None,
                    None,
                    None,
                    Some(SkipprStoreKind::DynamoDb),
                    Some("console-skipprd-offsets-prod"),
                    None,
                    None,
                )
                .unwrap(),
                ..Config::default()
            },
        )
        .unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("type: dynamodb"));
        assert!(raw.contains("name: console-skipprd-offsets-prod"));
        assert!(!raw.contains("offset_store:"));
        assert!(!raw.contains("offset_dynamodb_table:"));
    }

    #[test]
    fn persist_nested_secret_path_under_auth() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        persist_plugin(
            &path,
            None,
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
    fn persist_skipprlake_object_store_from_nested_yaml_and_cli_idents() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        let mut object_store = serde_yaml::Mapping::new();
        object_store.insert(
            serde_yaml::Value::String("type".into()),
            serde_yaml::Value::String("r2".into()),
        );
        object_store.insert(
            serde_yaml::Value::String("endpoint".into()),
            serde_yaml::Value::String("${OBJECTS_S3_ENDPOINT}".into()),
        );
        object_store.insert(
            serde_yaml::Value::String("secret_access_key".into()),
            serde_yaml::Value::String("${OBJECTS_SECRET_ACCESS_KEY}".into()),
        );
        persist_plugin(
            &path,
            None,
            "p",
            ConnectPlugin::DataSinkSkipprLake,
            "lake",
            BTreeMap::from([
                (
                    "warehouse".into(),
                    serde_yaml::Value::String("s3://wh/".into()),
                ),
                (
                    "catalog_table".into(),
                    serde_yaml::Value::String("cat".into()),
                ),
                (
                    "object_store".into(),
                    serde_yaml::Value::Mapping(object_store),
                ),
            ]),
        )
        .unwrap();
        let doc: serde_yaml::Value =
            serde_yaml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            doc["data_sinks"]["lake"]["SkipprLake"]["object_store"]["type"]
                .as_str()
                .unwrap(),
            "r2"
        );
        assert_eq!(
            doc["data_sinks"]["lake"]["SkipprLake"]["object_store"]["secret_access_key"]
                .as_str()
                .unwrap(),
            "${OBJECTS_SECRET_ACCESS_KEY}"
        );

        let err = persist_plugin(
            &path,
            None,
            "p",
            ConnectPlugin::DataSinkSkipprLake,
            "lake",
            yaml_path_fields(
                ConnectPlugin::DataSinkSkipprLake,
                &yaml_string_map(BTreeMap::from([
                    ("warehouse".into(), "s3://wh/".into()),
                    ("catalog_table".into(), "cat".into()),
                    ("object_store_type".into(), "r2".into()),
                    ("object_store_secret_access_key".into(), "plaintext".into()),
                ])),
            )
            .unwrap(),
        )
        .unwrap_err();
        assert!(err.contains("${ENV}"), "{err}");
    }

    #[test]
    fn persist_athena_iceberg_required_fields() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        persist_plugin(
            &path,
            None,
            "p",
            ConnectPlugin::DataSinkAthenaIceberg,
            "warehouse",
            BTreeMap::from([
                (
                    "warehouse".into(),
                    serde_yaml::Value::String("s3://wh/".into()),
                ),
                (
                    "glue_database_name".into(),
                    serde_yaml::Value::String("analytics".into()),
                ),
                (
                    "athena_workgroup_name".into(),
                    serde_yaml::Value::String("primary".into()),
                ),
                (
                    "athena_results_s3_bucket".into(),
                    serde_yaml::Value::String("results".into()),
                ),
            ]),
        )
        .unwrap();
        let doc: serde_yaml::Value =
            serde_yaml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let body = &doc["data_sinks"]["warehouse"]["AthenaIceberg"];
        assert_eq!(body["warehouse"].as_str().unwrap(), "s3://wh/");
        assert_eq!(body["glue_database_name"].as_str().unwrap(), "analytics");
        assert_eq!(body["athena_workgroup_name"].as_str().unwrap(), "primary");
        assert_eq!(
            body["athena_results_s3_bucket"].as_str().unwrap(),
            "results"
        );
        assert!(body.get("catalog").is_none());
        assert!(body.get("query_engine").is_none());
        assert!(body.get("table_prefix").is_none());
    }

    #[test]
    fn persist_duckdb_required_fields() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        persist_plugin(
            &path,
            None,
            "p",
            ConnectPlugin::DataSinkDuckdb,
            "lake",
            BTreeMap::from([
                (
                    "warehouse".into(),
                    serde_yaml::Value::String("file:///tmp/lake".into()),
                ),
                (
                    "table_namespace".into(),
                    serde_yaml::Value::String("bronze".into()),
                ),
            ]),
        )
        .unwrap();
        let doc: serde_yaml::Value =
            serde_yaml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let body = &doc["data_sinks"]["lake"]["Duckdb"];
        assert_eq!(body["warehouse"].as_str().unwrap(), "file:///tmp/lake");
        assert_eq!(body["table_namespace"].as_str().unwrap(), "bronze");
        assert!(body.get("catalog").is_none());
        assert!(body.get("query_engine").is_none());
        assert!(body.get("path").is_none());
        assert!(body.get("schema").is_none());
        assert!(body.get("object_store").is_none());
    }

    #[test]
    fn persist_unresolved_secret_does_not_require_env() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        persist_plugin(
            &path,
            None,
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
        let authored = Config::parse_unresolved(&path).unwrap();
        save_config(&path, &authored).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("${POSTGRES_PASSWORD}"));
    }

    fn typed_config(yaml: &str) -> Config {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn save_config_writes_a_fresh_file_that_parses_back_equal() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("skippr.yml");
        let config = typed_config(
            r#"
skippr:
  workspace: ws
  skipprd_el_storage_mode: local
pipelines:
  p:
    data_source: data_sources.src
    data_sink: data_sinks.lake
    transform:
      flatten_events: true
      batch_time_unit: day
      batch_time_fields: time
data_sources:
  src:
    File:
      path: /tmp/in
      batch_size_bytes: 1048576
data_sinks:
  lake:
    Duckdb:
      warehouse: file:///tmp/lake
      table_namespace: bronze
    schema_sink: schema_sinks.lake
schema_sinks:
  lake:
    Duckdb:
      warehouse: file:///tmp/lake
      table_namespace: bronze
"#,
        );
        save_config(&path, &config).unwrap();
        let reparsed = Config::parse_unresolved(&path).unwrap();
        assert_eq!(reparsed.pipelines, config.pipelines);
        assert_eq!(reparsed.data_sources, config.data_sources);
        assert_eq!(reparsed.data_sinks, config.data_sinks);
        assert_eq!(reparsed.schema_sinks, config.schema_sinks);
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("batch_size_bytes: 1048576"), "{raw}");
        assert!(raw.contains("schema_sink: schema_sinks.lake"), "{raw}");
    }

    #[test]
    fn save_config_merges_into_existing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        fs::write(
            &path,
            r#"
skippr:
  workspace: demo
  skippr_s3_bucket: old-lake
  store:
    type: dynamodb
    name: old-table
pipelines:
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
data_sinks:
  lake:
    SkipprLake:
      warehouse: s3://wh/
      catalog_table: cat
      table_namespace: bronze
      object_store:
        type: r2
        endpoint: ${OBJECTS_S3_ENDPOINT}
"#,
        )
        .unwrap();
        let config = typed_config(
            r#"
skippr:
  skipprd_el_storage_mode: local
  store:
    type: sled
pipelines:
  a:
    data_source: data_sources.src_a
data_sources:
  src_a:
    S3:
      s3_bucket: keep-me
      s3_prefix: new
data_sinks:
  lake:
    SkipprLake:
      warehouse: s3://wh/
      catalog_table: cat
      table_namespace: bronze
      object_store:
        type: file
"#,
        );
        save_config(&path, &config).unwrap();
        let doc: serde_yaml::Value =
            serde_yaml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc["skippr"]["workspace"].as_str(), Some("demo"));
        assert!(doc["skippr"].get("skippr_s3_bucket").is_none());
        assert_eq!(doc["skippr"]["store"]["type"].as_str(), Some("sled"));
        assert!(doc["skippr"]["store"].get("name").is_none());
        assert_eq!(
            doc["pipelines"]["b"]["data_source"].as_str(),
            Some("data_sources.src_b")
        );
        assert_eq!(
            doc["pipelines"]["a"]["data_source"].as_str(),
            Some("data_sources.src_a")
        );
        let src_a = &doc["data_sources"]["src_a"]["S3"];
        assert_eq!(src_a["s3_prefix"].as_str(), Some("new"));
        assert_eq!(src_a["version"].as_str(), Some("1"));
        assert_eq!(
            doc["data_sources"]["src_b"]["S3"]["s3_bucket"].as_str(),
            Some("other")
        );
        let object_store = &doc["data_sinks"]["lake"]["SkipprLake"]["object_store"];
        assert_eq!(object_store["type"].as_str(), Some("file"));
        assert!(
            object_store.get("endpoint").is_none(),
            "a replaced nested variant must not keep the old variant's keys"
        );
    }

    #[test]
    fn save_config_rejects_kind_change_and_plaintext_secrets() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        save_config(
            &path,
            &typed_config("data_sources:\n  src:\n    S3:\n      s3_bucket: b\n"),
        )
        .unwrap();
        let err = save_config(
            &path,
            &typed_config("data_sources:\n  src:\n    File:\n      path: /tmp\n"),
        )
        .unwrap_err();
        assert!(err.contains("already uses S3"), "{err}");
        let err = save_config(
            &path,
            &typed_config("data_sinks:\n  db:\n    Postgres:\n      password: hunter2\n"),
        )
        .unwrap_err();
        assert!(err.contains("${ENV}"), "{err}");
        let err = save_config(
            &path,
            &typed_config("deadletter_sinks:\n  dl:\n    Postgres:\n      password: hunter2\n"),
        )
        .unwrap_err();
        assert!(err.contains("${ENV}"), "{err}");
        assert!(!fs::read_to_string(&path).unwrap().contains("hunter2"));
    }

    #[test]
    fn cli_root_flags_use_the_save_merge() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        fs::write(
            &path,
            "skippr:\n  skippr_s3_bucket: lake\n  store:\n    type: dynamodb\n    name: old\n",
        )
        .unwrap();
        save_config(
            &path,
            &Config {
                skippr: skippr_keys(
                    None,
                    Some(ElStorageMode::Local),
                    None,
                    Some(SkipprStoreKind::Sled),
                    None,
                    None,
                    None,
                )
                .unwrap(),
                ..Config::default()
            },
        )
        .unwrap();
        let doc: serde_yaml::Value =
            serde_yaml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(doc["skippr"].get("skippr_s3_bucket").is_none());
        assert_eq!(doc["skippr"]["store"]["type"].as_str(), Some("sled"));
        assert!(doc["skippr"]["store"].get("name").is_none());
        let err = skippr_keys(None, None, None, None, Some("t"), None, None).unwrap_err();
        assert!(err.contains("--store-type"), "{err}");
    }

    #[test]
    fn cli_nested_flags_replace_the_nested_value_like_save() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        let fields = |pairs: &[(&str, &str)]| {
            yaml_path_fields(
                ConnectPlugin::DataSinkSkipprLake,
                &yaml_string_map(
                    pairs
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                ),
            )
            .unwrap()
        };
        persist_plugin(
            &path,
            None,
            "p",
            ConnectPlugin::DataSinkSkipprLake,
            "lake",
            fields(&[
                ("warehouse", "s3://wh/"),
                ("catalog_table", "cat"),
                ("object_store_type", "r2"),
                ("object_store_endpoint", "https://r2"),
            ]),
        )
        .unwrap();
        persist_plugin(
            &path,
            None,
            "p",
            ConnectPlugin::DataSinkSkipprLake,
            "lake",
            fields(&[("object_store_type", "file")]),
        )
        .unwrap();
        let doc: serde_yaml::Value =
            serde_yaml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let body = &doc["data_sinks"]["lake"]["SkipprLake"];
        assert_eq!(body["object_store"]["type"].as_str(), Some("file"));
        assert!(body["object_store"].get("endpoint").is_none());
        assert_eq!(body["catalog_table"].as_str(), Some("cat"));
    }

    #[test]
    fn env_ref_grammar_is_exact() {
        for ok in ["${A}", "${_X1}", "${POSTGRES_PASSWORD}"] {
            assert!(is_env_ref(ok), "{ok}");
        }
        for bad in [
            "${A}hunter2}",
            "${}",
            "${1A}",
            "${A-B}",
            "hunter2",
            "",
            "$A",
            "${A",
        ] {
            assert!(!is_env_ref(bad), "{bad}");
        }
    }

    #[test]
    fn merge_matches_save_and_new_files_start_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        let update = typed_config("skippr:\n  workspace: w\ndbt:\n  target_schema: ts\n");
        save_config(&path, &update).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("skipprd_el_storage_mode"), "{raw}");
        assert!(raw.contains("target_schema: ts"), "{raw}");
        let base = Config::parse_unresolved(&path).unwrap();
        let next = typed_config("data_sources:\n  s:\n    S3:\n      s3_bucket: b\n");
        let merged = merge(&base, &next).unwrap();
        save_config(&path, &next).unwrap();
        assert_eq!(Config::parse_unresolved(&path).unwrap(), merged);
    }

    #[test]
    fn cli_connect_keeps_a_paired_schema_sink_equal_to_its_data_sink() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        let warehouse = format!("file://{}", dir.path().join("wh").display());
        let connect = |plugin, name: &str, pairs: &[(&str, &str)]| {
            persist_plugin(&path, None, "p", plugin, name, plain_fields(pairs))
        };
        let pair = || {
            let doc = load_document(&path).unwrap();
            let sink = doc["data_sinks"]["lake"]["Duckdb"].clone();
            assert_eq!(sink, doc["schema_sinks"]["lake_schema"]["Duckdb"]);
            sink["table_namespace"].as_str().unwrap().to_string()
        };
        connect(ConnectPlugin::DataSourceFile, "src", &[("path", "/tmp/in")]).unwrap();
        connect(
            ConnectPlugin::DataSinkDuckdb,
            "lake",
            &[("warehouse", &warehouse), ("table_namespace", "bronze")],
        )
        .unwrap();
        connect(
            ConnectPlugin::SchemaSinkDuckdb,
            "lake_schema",
            &[("warehouse", &warehouse)],
        )
        .unwrap();
        assert_eq!(pair(), "bronze");
        connect(
            ConnectPlugin::DataSinkDuckdb,
            "lake",
            &[("table_namespace", "silver")],
        )
        .unwrap();
        assert_eq!(pair(), "silver");
        connect(
            ConnectPlugin::SchemaSinkDuckdb,
            "lake_schema",
            &[("table_namespace", "gold")],
        )
        .unwrap();
        assert_eq!(pair(), "gold");
    }

    #[test]
    fn connect_schema_sink_never_takes_fields_from_a_stale_schema_sink() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        let base = "pipelines:\n  p:\n    data_source: data_sources.src\n    data_sink: data_sinks.lake\ndata_sources:\n  src:\n    File:\n      path: /tmp/in\ndata_sinks:\n  lake:\n    Duckdb:\n      warehouse: file:///tmp/live\n      table_namespace: main\nschema_sinks:\n  old:\n    Duckdb:\n      warehouse: file:///tmp/stale\n      table_namespace: main\n";
        fs::write(&path, base).unwrap();
        let err = persist_plugin(
            &path,
            None,
            "p",
            ConnectPlugin::SchemaSinkDuckdb,
            "old",
            plain_fields(&[("table_namespace", "main")]),
        )
        .unwrap_err();
        assert!(err.contains("must be equal"), "{err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), base);
    }

    #[test]
    fn connect_schema_sink_updates_every_data_sink_that_shares_it() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        let ice = "      warehouse: s3://w/\n      glue_database_name: g\n      athena_workgroup_name: primary\n      athena_results_s3_bucket: r\n";
        let base = format!(
            "pipelines:\n  p:\n    data_source: data_sources.src\n    data_sink: data_sinks.a\ndata_sources:\n  src:\n    File:\n      path: /tmp/in\ndata_sinks:\n  a:\n    schema_sink: schema_sinks.ice\n    AthenaIceberg:\n{ice}  b:\n    schema_sink: schema_sinks.ice\n    AthenaIceberg:\n{ice}schema_sinks:\n  ice:\n    AthenaIceberg:\n{ice}"
        );
        fs::write(&path, &base).unwrap();
        let region = |plugin, name: &str| {
            persist_plugin(
                &path,
                None,
                "p",
                plugin,
                name,
                plain_fields(&[("region", "eu-west-1")]),
            )
        };
        for (plugin, name, value) in [
            (ConnectPlugin::DataSinkAthenaIceberg, "a", "eu-west-1"),
            (ConnectPlugin::SchemaSinkAthenaIceberg, "ice", "eu-west-2"),
        ] {
            persist_plugin(
                &path,
                None,
                "p",
                plugin,
                name,
                plain_fields(&[("region", value)]),
            )
            .unwrap();
            let doc = load_document(&path).unwrap();
            for entry in [
                &doc["data_sinks"]["a"],
                &doc["data_sinks"]["b"],
                &doc["schema_sinks"]["ice"],
            ] {
                assert_eq!(entry["AthenaIceberg"]["region"].as_str(), Some(value));
            }
        }
    }

    #[test]
    fn connect_refuses_a_pair_that_already_differs_even_when_the_write_would_cover_it() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        let base = "pipelines:\n  p:\n    data_source: data_sources.src\n    data_sink: data_sinks.d\ndata_sources:\n  src:\n    File:\n      path: /tmp/in\ndata_sinks:\n  d:\n    schema_sink: schema_sinks.s\n    Duckdb:\n      warehouse: file:///tmp/w\n      table_namespace: a\nschema_sinks:\n  s:\n    Duckdb:\n      warehouse: file:///tmp/w\n      table_namespace: b\n";
        fs::write(&path, base).unwrap();
        for (plugin, name) in [
            (ConnectPlugin::DataSinkDuckdb, "d"),
            (ConnectPlugin::SchemaSinkDuckdb, "s"),
        ] {
            let err = persist_plugin(
                &path,
                None,
                "p",
                plugin,
                name,
                plain_fields(&[("table_namespace", "c")]),
            )
            .unwrap_err();
            assert!(err.contains("must be equal"), "{err}");
            assert_eq!(fs::read_to_string(&path).unwrap(), base);
        }
    }

    #[test]
    fn an_empty_pair_member_starts_as_a_copy_and_every_member_is_written_in_canonical_case() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        let head = "pipelines:\n  p:\n    data_source: data_sources.src\n    data_sink: data_sinks.d\ndata_sources:\n  src:\n    File:\n      path: /tmp/in\ndata_sinks:\n  d:\n    schema_sink: schema_sinks.s\n    duckdb:\n      warehouse: file:///tmp/w\n      table_namespace: a\nschema_sinks:\n  s:\n";
        for schema in ["    duckdb:\n", "    Duckdb: {}\n", "    {}\n"] {
            fs::write(&path, format!("{head}{schema}")).unwrap();
            persist_plugin(
                &path,
                None,
                "p",
                ConnectPlugin::SchemaSinkDuckdb,
                "s",
                plain_fields(&[("table_namespace", "c")]),
            )
            .unwrap();
            let doc = load_document(&path).unwrap();
            let expected: serde_yaml::Value =
                serde_yaml::from_str("warehouse: file:///tmp/w\ntable_namespace: c\n").unwrap();
            assert_eq!(doc["data_sinks"]["d"]["Duckdb"], expected, "{schema}");
            assert_eq!(doc["schema_sinks"]["s"]["Duckdb"], expected, "{schema}");
            assert!(doc["data_sinks"]["d"].get("duckdb").is_none());
        }
    }

    #[test]
    fn a_refused_secret_names_the_entry_the_write_named() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        let base = "pipelines:\n  p:\n    data_source: data_sources.src\n    data_sink: data_sinks.lake\ndata_sources:\n  src:\n    File:\n      path: /tmp/in\ndata_sinks:\n  lake:\n    schema_sink: schema_sinks.lake\n    SkipprLake:\n      table_namespace: main\n";
        fs::write(&path, base).unwrap();
        let err = persist_plugin(
            &path,
            None,
            "p",
            ConnectPlugin::DataSinkSkipprLake,
            "lake",
            plain_fields(&[("object_store.secret_access_key", "plain")]),
        )
        .unwrap_err();
        assert!(err.starts_with("data_sinks.lake: secret field"), "{err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), base);
    }

    #[test]
    fn a_refused_connect_writes_nothing_including_root_flags() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        let root = skippr_keys(Some("newws"), None, None, None, None, None, None).unwrap();
        let connect = |password: &str| {
            persist_plugin(
                &path,
                root.as_ref(),
                "p",
                ConnectPlugin::DataSinkPostgres,
                "db",
                plain_fields(&[("user", "u"), ("database", "d"), ("password", password)]),
            )
        };
        let err = connect("hunter2").unwrap_err();
        assert!(err.contains("${ENV}"), "{err}");
        assert!(!path.exists());
        connect("${PG_PASSWORD}").unwrap();
        let doc = load_document(&path).unwrap();
        assert_eq!(doc["skippr"]["workspace"].as_str(), Some("newws"));
        assert_eq!(
            doc["data_sinks"]["db"]["Postgres"]["password"].as_str(),
            Some("${PG_PASSWORD}")
        );
        assert_eq!(
            skippr_keys(None, None, None, None, None, None, None).unwrap(),
            None
        );
    }

    fn plain_fields(pairs: &[(&str, &str)]) -> YamlFields {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), serde_yaml::Value::String(v.to_string())))
            .collect()
    }

    #[test]
    fn connect_refuses_non_yaml_config_paths() {
        let dir = tempdir().unwrap();
        for name in ["skipprd.json", "skipprd.toml"] {
            let path = dir.path().join(name);
            let err = persist_plugin(
                &path,
                None,
                "p",
                ConnectPlugin::DataSourceFile,
                "src",
                plain_fields(&[("path", "/tmp/in")]),
            )
            .unwrap_err();
            assert!(err.contains("YAML"), "{err}");
            assert!(save_config(&path, &Config::default())
                .unwrap_err()
                .contains("YAML"));
            assert!(!path.exists());
        }
    }

    #[test]
    fn connect_and_save_reject_plaintext_secrets_already_in_the_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        let doc = "data_sinks:\n  db:\n    Postgres:\n      user: u\n      database: d\n      password: hunter2\n";
        fs::write(&path, doc).unwrap();
        let cli = persist_plugin(
            &path,
            None,
            "p",
            ConnectPlugin::DataSourceFile,
            "src",
            plain_fields(&[("path", "/tmp/in")]),
        )
        .unwrap_err();
        let save = save_config(&path, &Config::default()).unwrap_err();
        assert!(
            cli.contains("${ENV}") && save.contains("${ENV}"),
            "{cli} / {save}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), doc);
    }

    #[test]
    fn connect_rejects_dotted_entry_names() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        let err = persist_plugin(
            &path,
            None,
            "p",
            ConnectPlugin::DataSourceFile,
            "a.b",
            plain_fields(&[("path", "/tmp/in")]),
        )
        .unwrap_err();
        assert!(err.contains("'.'"), "{err}");
    }

    #[test]
    fn connect_validates_schema_sink_pairing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        persist_plugin(
            &path,
            None,
            "p",
            ConnectPlugin::DataSourceFile,
            "src",
            plain_fields(&[("path", "/tmp/in")]),
        )
        .unwrap();
        persist_plugin(
            &path,
            None,
            "p",
            ConnectPlugin::DataSinkDuckdb,
            "lake",
            plain_fields(&[("warehouse", "file:///tmp/w"), ("table_namespace", "main")]),
        )
        .unwrap();
        let before = fs::read_to_string(&path).unwrap();
        let err = persist_plugin(
            &path,
            None,
            "p",
            ConnectPlugin::SchemaSinkGlue,
            "glue",
            plain_fields(&[("glue_database_name", "g")]),
        )
        .unwrap_err();
        assert!(err.to_lowercase().contains("duckdb"), "{err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn save_and_merge_refuse_a_result_whose_pipelines_do_not_validate() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        let base = r#"
pipelines:
  p:
    data_source: data_sources.src
    data_sink: data_sinks.lake
data_sources:
  src:
    File:
      path: /tmp/in
data_sinks:
  lake:
    schema_sink: schema_sinks.lake
    Duckdb:
      warehouse: file:///tmp/w
      table_namespace: main
schema_sinks:
  lake:
    Duckdb:
      warehouse: file:///tmp/w
      table_namespace: main
"#;
        fs::write(&path, base).unwrap();
        let update: Config = serde_yaml::from_str(
            "schema_sinks:\n  lake:\n    Duckdb:\n      warehouse: file:///tmp/other\n      table_namespace: main\n",
        )
        .unwrap();
        let err = save_config(&path, &update).unwrap_err();
        assert!(err.contains("must be equal"), "{err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), base);
        let loaded: Config = serde_yaml::from_str(base).unwrap();
        let err = merge(&loaded, &update).unwrap_err();
        assert!(err.contains("must be equal"), "{err}");
    }

    #[test]
    fn save_and_merge_refuse_two_sinks_on_one_duckdb_namespace() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        let base = "data_sinks:\n  a:\n    Duckdb:\n      warehouse: file:///tmp/w\n      table_namespace: main\n";
        fs::write(&path, base).unwrap();
        let update = typed_config(
            "data_sinks:\n  b:\n    Duckdb:\n      warehouse: file:///tmp/w\n      table_namespace: main\n",
        );
        let err = save_config(&path, &update).unwrap_err();
        assert!(err.contains("reuses warehouse"), "{err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), base);
        let err = merge(&typed_config(base), &update).unwrap_err();
        assert!(err.contains("reuses warehouse"), "{err}");
    }

    #[test]
    fn lowercase_plugin_keys_merge_into_the_canonical_kind() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skippr.yml");
        fs::write(
            &path,
            "data_sources:\n  src:\n    file:\n      path: /tmp/in\n      format: json\n",
        )
        .unwrap();
        persist_plugin(
            &path,
            None,
            "p",
            ConnectPlugin::DataSourceFile,
            "src",
            plain_fields(&[("path", "/tmp/other")]),
        )
        .unwrap();
        let doc: serde_yaml::Value =
            serde_yaml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let entry = doc["data_sources"]["src"].as_mapping().unwrap();
        assert_eq!(entry.len(), 1);
        assert_eq!(
            doc["data_sources"]["src"]["File"]["path"].as_str(),
            Some("/tmp/other")
        );
        assert_eq!(
            doc["data_sources"]["src"]["File"]["format"].as_str(),
            Some("json")
        );
    }
}
