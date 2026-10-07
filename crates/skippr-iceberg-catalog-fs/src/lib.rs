//! Filesystem Iceberg catalog for the Duckdb sink.
//!
//! iceberg-rust metadata is `{version:05}-{uuid}.metadata.json`. DuckDB 1.5
//! `iceberg_scan` without version guessing reads `version-hint.text` as Hadoop
//! `v{N}.metadata.json`. OCC is `PutMode::Create` on that Hadoop alias.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use iceberg::io::FileIO;
use iceberg::spec::TableMetadataBuilder;
use iceberg::table::Table;
use iceberg::{
    Catalog, Error, ErrorKind, MetadataLocation, Namespace, NamespaceIdent, Result, TableCommit,
    TableCreation, TableIdent,
};
use object_store::local::LocalFileSystem;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload};
use skippr_iceberg_catalog::{CachedMetadata, MetadataCache};

#[derive(Clone)]
pub struct FsCatalog {
    warehouse_uri: String,
    warehouse_fs: PathBuf,
    file_io: FileIO,
    store: Arc<LocalFileSystem>,
    metadata: MetadataCache,
}

impl fmt::Debug for FsCatalog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FsCatalog")
            .field("warehouse_uri", &self.warehouse_uri)
            .finish_non_exhaustive()
    }
}

impl FsCatalog {
    pub fn new(warehouse: impl Into<String>, file_io: FileIO) -> Result<Self> {
        let warehouse_uri = file_uri(&warehouse.into())?;
        let warehouse_fs = PathBuf::from(strip_file(&warehouse_uri));
        std::fs::create_dir_all(&warehouse_fs).map_err(|err| {
            Error::new(
                ErrorKind::Unexpected,
                format!("create warehouse {}: {err}", warehouse_fs.display()),
            )
        })?;
        let store = LocalFileSystem::new_with_prefix(&warehouse_fs).map_err(|err| {
            Error::new(ErrorKind::Unexpected, format!("object_store prefix: {err}"))
        })?;
        Ok(Self {
            warehouse_uri,
            warehouse_fs,
            file_io,
            store: Arc::new(store),
            metadata: MetadataCache::default(),
        })
    }

    fn ns_rel(ns: &NamespaceIdent) -> String {
        ns.join("/")
    }

    fn table_rel(table: &TableIdent) -> String {
        format!("{}/{}", Self::ns_rel(table.namespace()), table.name())
    }

    fn table_uri(&self, table: &TableIdent) -> String {
        format!("{}/{}", self.warehouse_uri, Self::table_rel(table))
    }

    fn ns_fs(&self, ns: &NamespaceIdent) -> PathBuf {
        self.warehouse_fs.join(Self::ns_rel(ns))
    }

    fn table_fs(&self, table: &TableIdent) -> PathBuf {
        self.warehouse_fs.join(Self::table_rel(table))
    }

    fn hint_uri(table_uri: &str) -> String {
        format!("{table_uri}/metadata/version-hint.text")
    }

    fn hadoop_rel(version: i32) -> String {
        format!("v{version}.metadata.json")
    }

    fn object_path(table: &TableIdent, file: &str) -> ObjectPath {
        ObjectPath::from(format!("{}/metadata/{file}", Self::table_rel(table)))
    }

    async fn put_if_absent(&self, path: &ObjectPath, bytes: Bytes) -> Result<bool> {
        match self
            .store
            .put_opts(
                path,
                PutPayload::from_bytes(bytes),
                PutOptions {
                    mode: PutMode::Create,
                    ..PutOptions::default()
                },
            )
            .await
        {
            Ok(_) => Ok(true),
            Err(object_store::Error::AlreadyExists { .. }) => Ok(false),
            Err(err) => Err(Error::new(ErrorKind::Unexpected, err.to_string())),
        }
    }

    async fn write_hint(&self, table_uri: &str, version: i32) -> Result<()> {
        let hint = Self::hint_uri(table_uri);
        self.file_io
            .new_output(&hint)?
            .write(Bytes::from(version.to_string()))
            .await
    }

    async fn publish_hadoop_alias(
        &self,
        table: &TableIdent,
        table_uri: &str,
        uuid_location: &str,
        version: i32,
    ) -> Result<()> {
        let bytes = self.file_io.new_input(uuid_location)?.read().await?;
        // Superseded aliases are deleted after commit, so `Create` alone cannot
        // reject a writer whose target version has already been passed.
        let passed = self
            .max_hadoop_version(table)
            .is_some_and(|max| max >= version);
        let created = !passed
            && self
                .put_if_absent(&Self::object_path(table, &Self::hadoop_rel(version)), bytes)
                .await?;
        if !created {
            skippr_iceberg_catalog::record_cas_conflict();
            return Err(Error::new(
                ErrorKind::CatalogCommitConflicts,
                format!("Hadoop metadata v{version} already exists for {table:?}"),
            ));
        }
        self.write_hint(table_uri, version).await
    }

    async fn read_hint_version(&self, table: &TableIdent) -> Result<Option<i32>> {
        let listed = self.max_hadoop_version(table);
        let hint = Self::hint_uri(&self.table_uri(table));
        if !self.file_io.exists(&hint).await? {
            return Ok(listed);
        }
        let bytes = self.file_io.new_input(&hint)?.read().await?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|err| Error::new(ErrorKind::DataInvalid, err.to_string()))?
            .trim();
        let hinted: i32 = text.parse().map_err(|err| {
            Error::new(
                ErrorKind::DataInvalid,
                format!("version-hint {text}: {err}"),
            )
        })?;
        Ok(Some(listed.map_or(hinted, |listed| listed.max(hinted))))
    }

    fn max_hadoop_version(&self, table: &TableIdent) -> Option<i32> {
        let dir = self.table_fs(table).join("metadata");
        let entries = std::fs::read_dir(&dir).ok()?;
        let mut max = None;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(rest) = name.strip_prefix('v') {
                if let Some(num) = rest.strip_suffix(".metadata.json") {
                    if let Ok(v) = num.parse::<i32>() {
                        max = Some(max.map_or(v, |m: i32| m.max(v)));
                    }
                }
            }
        }
        max
    }

    async fn table_from_uuid(&self, table: TableIdent, uuid_location: &str) -> Result<Table> {
        let metadata = self.metadata.read(&self.file_io, uuid_location).await?;
        Self::build_table(&self.file_io, table, uuid_location, metadata)
    }

    fn build_table(
        file_io: &FileIO,
        table: TableIdent,
        uuid_location: &str,
        metadata: iceberg::spec::TableMetadataRef,
    ) -> Result<Table> {
        Table::builder()
            .file_io(file_io.clone())
            .metadata_location(uuid_location.to_string())
            .metadata(metadata)
            .identifier(table)
            .build()
    }

    /// Hadoop `v{N}` is create-only, but drop + recreate reuses `N`; the stat
    /// identity keeps a recreated alias from hitting the old entry.
    fn hadoop_alias_key(&self, table: &TableIdent, version: i32) -> Option<String> {
        let path = self
            .table_fs(table)
            .join("metadata")
            .join(Self::hadoop_rel(version));
        let stat = std::fs::metadata(&path).ok()?;
        let modified = stat
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_nanos();
        Some(format!("{}#{}#{modified}", path.display(), stat.len()))
    }

    fn remember_published(
        &self,
        table: &TableIdent,
        version: i32,
        uuid_location: &str,
        metadata: iceberg::spec::TableMetadataRef,
    ) {
        let entry = CachedMetadata {
            location: uuid_location.to_string(),
            metadata,
        };
        if let Some(key) = self.hadoop_alias_key(table, version) {
            self.metadata.insert(key, entry.clone());
        }
        self.metadata.insert(uuid_location, entry);
    }

    fn uuid_location_for_version(&self, table: &TableIdent, version: i32) -> Result<String> {
        let dir = self.table_fs(table).join("metadata");
        let hadoop = dir.join(Self::hadoop_rel(version));
        let hadoop_bytes = std::fs::read(&hadoop).map_err(|err| {
            Error::new(
                ErrorKind::TableNotFound,
                format!("{}: {err}", hadoop.display()),
            )
        })?;
        let prefix = format!("{version:05}-");
        let entries = std::fs::read_dir(&dir).map_err(|err| {
            Error::new(
                ErrorKind::TableNotFound,
                format!("metadata dir {}: {err}", dir.display()),
            )
        })?;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(&prefix) && name.ends_with(".metadata.json") {
                if std::fs::read(entry.path()).ok().as_deref() == Some(hadoop_bytes.as_slice()) {
                    return Ok(format!("{}/metadata/{name}", self.table_uri(table)));
                }
            }
        }
        Err(Error::new(
            ErrorKind::TableNotFound,
            format!("no iceberg-rust metadata matching Hadoop v{version} for {table:?}"),
        ))
    }
}

fn file_uri(warehouse: &str) -> Result<String> {
    match skippr_iceberg_catalog::require_file_warehouse(warehouse) {
        Ok(uri) => Ok(uri),
        Err(err) => {
            let trimmed = skippr_iceberg_catalog::warehouse_key(warehouse);
            if trimmed.starts_with("s3://") || trimmed.starts_with("s3a://") {
                Err(Error::new(
                    ErrorKind::FeatureUnsupported,
                    "FsCatalog is file:// only",
                ))
            } else {
                Err(Error::new(ErrorKind::DataInvalid, err))
            }
        }
    }
}

fn strip_file(uri: &str) -> String {
    uri.trim_start_matches("file://").to_string()
}

fn uuid_metadata_version(location: &str) -> Result<i32> {
    let name = location.rsplit('/').next().ok_or_else(|| {
        Error::new(
            ErrorKind::DataInvalid,
            format!("metadata location has no file name: {location}"),
        )
    })?;
    let (ver, _) = name
        .strip_suffix(".metadata.json")
        .and_then(|s| s.split_once('-'))
        .ok_or_else(|| {
            Error::new(
                ErrorKind::DataInvalid,
                format!("not an iceberg-rust metadata location: {location}"),
            )
        })?;
    ver.parse()
        .map_err(|err| Error::new(ErrorKind::DataInvalid, format!("{ver}: {err}")))
}

#[async_trait]
impl Catalog for FsCatalog {
    async fn list_namespaces(
        &self,
        parent: Option<&NamespaceIdent>,
    ) -> Result<Vec<NamespaceIdent>> {
        let dir = match parent {
            None => self.warehouse_fs.clone(),
            Some(ns) => self.ns_fs(ns),
        };
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Ok(out);
        };
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "metadata" {
                continue;
            }
            let ident = match parent {
                None => NamespaceIdent::new(name),
                Some(parent) => {
                    let mut parts = parent.as_ref().clone();
                    parts.push(name);
                    NamespaceIdent::from_vec(parts)?
                }
            };
            out.push(ident);
        }
        Ok(out)
    }

    async fn create_namespace(
        &self,
        namespace: &NamespaceIdent,
        properties: HashMap<String, String>,
    ) -> Result<Namespace> {
        let path = self.ns_fs(namespace);
        if path.exists() {
            return Err(Error::new(
                ErrorKind::NamespaceAlreadyExists,
                format!("{namespace:?}"),
            ));
        }
        std::fs::create_dir_all(&path)
            .map_err(|err| Error::new(ErrorKind::Unexpected, format!("create namespace: {err}")))?;
        Ok(Namespace::with_properties(namespace.clone(), properties))
    }

    async fn get_namespace(&self, namespace: &NamespaceIdent) -> Result<Namespace> {
        if !self.namespace_exists(namespace).await? {
            return Err(Error::new(
                ErrorKind::NamespaceNotFound,
                format!("{namespace:?}"),
            ));
        }
        Ok(Namespace::new(namespace.clone()))
    }

    async fn namespace_exists(&self, namespace: &NamespaceIdent) -> Result<bool> {
        Ok(self.ns_fs(namespace).is_dir())
    }

    async fn update_namespace(
        &self,
        _namespace: &NamespaceIdent,
        _properties: HashMap<String, String>,
    ) -> Result<()> {
        Err(Error::new(
            ErrorKind::FeatureUnsupported,
            "FsCatalog does not store namespace properties",
        ))
    }

    async fn drop_namespace(&self, namespace: &NamespaceIdent) -> Result<()> {
        let path = self.ns_fs(namespace);
        if !path.is_dir() {
            return Err(Error::new(
                ErrorKind::NamespaceNotFound,
                format!("{namespace:?}"),
            ));
        }
        std::fs::remove_dir_all(&path)
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))
    }

    async fn list_tables(&self, namespace: &NamespaceIdent) -> Result<Vec<TableIdent>> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(self.ns_fs(namespace)) else {
            return Ok(out);
        };
        for entry in entries.flatten() {
            let ident = TableIdent::new(
                namespace.clone(),
                entry.file_name().to_string_lossy().into_owned(),
            );
            if self.max_hadoop_version(&ident).is_some() {
                out.push(ident);
            }
        }
        Ok(out)
    }

    async fn create_table(
        &self,
        namespace: &NamespaceIdent,
        creation: TableCreation,
    ) -> Result<Table> {
        let ident = TableIdent::new(namespace.clone(), creation.name.clone());
        if self.table_exists(&ident).await? {
            return Err(Error::new(
                ErrorKind::TableAlreadyExists,
                format!("{ident:?}"),
            ));
        }
        if !self.namespace_exists(namespace).await? {
            return Err(Error::new(
                ErrorKind::NamespaceNotFound,
                format!("{namespace:?}"),
            ));
        }
        let location = creation
            .location
            .clone()
            .unwrap_or_else(|| self.table_uri(&ident));
        let metadata = TableMetadataBuilder::from_table_creation(TableCreation {
            location: Some(location.clone()),
            ..creation
        })?
        .build()?
        .metadata;
        let uuid_location = MetadataLocation::new_with_table_location(&location).to_string();
        metadata.write_to(&self.file_io, &uuid_location).await?;
        let version = uuid_metadata_version(&uuid_location)?;
        self.publish_hadoop_alias(&ident, &location, &uuid_location, version)
            .await?;
        self.table_from_uuid(ident, &uuid_location).await
    }

    async fn load_table(&self, table: &TableIdent) -> Result<Table> {
        let version = self
            .read_hint_version(table)
            .await?
            .ok_or_else(|| Error::new(ErrorKind::TableNotFound, format!("{table:?}")))?;
        let alias = self.hadoop_alias_key(table, version);
        if let Some(hit) = alias.as_deref().and_then(|key| self.metadata.get(key)) {
            return Self::build_table(&self.file_io, table.clone(), &hit.location, hit.metadata);
        }
        let uuid_location = self.uuid_location_for_version(table, version)?;
        let loaded = self.table_from_uuid(table.clone(), &uuid_location).await?;
        if let Some(key) = alias {
            self.metadata.insert(
                key,
                CachedMetadata {
                    location: uuid_location,
                    metadata: loaded.metadata_ref(),
                },
            );
        }
        Ok(loaded)
    }

    async fn drop_table(&self, table: &TableIdent) -> Result<()> {
        if !self.table_exists(table).await? {
            return Err(Error::new(ErrorKind::TableNotFound, format!("{table:?}")));
        }
        self.metadata
            .remove_prefix(&self.table_fs(table).join("metadata").display().to_string());
        std::fs::remove_dir_all(&self.table_fs(table))
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))
    }

    async fn table_exists(&self, table: &TableIdent) -> Result<bool> {
        Ok(self.max_hadoop_version(table).is_some())
    }

    async fn rename_table(&self, _src: &TableIdent, _dest: &TableIdent) -> Result<()> {
        Err(Error::new(
            ErrorKind::FeatureUnsupported,
            "FsCatalog does not rename tables",
        ))
    }

    async fn register_table(
        &self,
        _table: &TableIdent,
        _metadata_location: String,
    ) -> Result<Table> {
        Err(Error::new(
            ErrorKind::FeatureUnsupported,
            "FsCatalog does not register external tables",
        ))
    }

    async fn update_table(&self, commit: TableCommit) -> Result<Table> {
        let ident = commit.identifier().clone();
        let current = self.load_table(&ident).await?;
        let staged = commit.apply(current.clone())?;
        let uuid_location = staged.metadata_location_result()?.to_string();
        staged
            .metadata()
            .write_to(staged.file_io(), &uuid_location)
            .await?;
        let version = uuid_metadata_version(&uuid_location)?;
        let table_uri = staged.metadata().location().to_string();
        self.publish_hadoop_alias(&ident, &table_uri, &uuid_location, version)
            .await?;
        self.remember_published(&ident, version, &uuid_location, staged.metadata_ref());
        let superseded = skippr_iceberg_catalog::delete_superseded_metadata(
            &self.file_io,
            current.metadata(),
            current.metadata_location_result()?,
            staged.metadata(),
            &uuid_location,
        )
        .await;
        for location in superseded {
            if let Ok(old) = uuid_metadata_version(&location) {
                let _ = self
                    .store
                    .delete(&Self::object_path(&ident, &Self::hadoop_rel(old)))
                    .await;
            }
        }
        Ok(staged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iceberg::spec::{NestedField, PrimitiveType, Schema, Type};
    use iceberg::transaction::{ApplyTransactionAction, Transaction};
    use std::str::FromStr;

    fn schema() -> Schema {
        Schema::builder()
            .with_fields(vec![NestedField::required(
                1,
                "foo",
                Type::Primitive(PrimitiveType::Int),
            )
            .into()])
            .build()
            .unwrap()
    }

    async fn catalog() -> (FsCatalog, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let warehouse = format!("file://{}", dir.path().display());
        let file_io = FileIO::new_with_fs();
        (FsCatalog::new(&warehouse, file_io).unwrap(), dir)
    }

    async fn create_orders(catalog: &FsCatalog) -> TableIdent {
        let ns = NamespaceIdent::new("bronze".into());
        catalog.create_namespace(&ns, HashMap::new()).await.unwrap();
        let ident = TableIdent::new(ns.clone(), "orders".into());
        catalog
            .create_table(
                &ns,
                TableCreation::builder()
                    .name("orders".into())
                    .schema(schema())
                    .build(),
            )
            .await
            .unwrap();
        ident
    }

    #[tokio::test]
    async fn create_load_roundtrip_writes_hadoop_alias_and_hint() {
        let (catalog, _dir) = catalog().await;
        let ident = create_orders(&catalog).await;
        let loaded = catalog.load_table(&ident).await.unwrap();
        assert!(
            loaded
                .metadata_location()
                .unwrap()
                .contains("/metadata/00000-"),
            "{}",
            loaded.metadata_location().unwrap()
        );
        let table_fs = PathBuf::from(strip_file(loaded.metadata().location()));
        let hint = std::fs::read_to_string(table_fs.join("metadata/version-hint.text")).unwrap();
        assert_eq!(hint, "0");
        assert!(table_fs.join("metadata/v0.metadata.json").is_file());
        let uuid_files: Vec<_> = std::fs::read_dir(table_fs.join("metadata"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".metadata.json") && n.contains('-'))
            .collect();
        assert_eq!(uuid_files.len(), 1);
        assert!(MetadataLocation::from_str(&format!(
            "{}/metadata/{}",
            loaded.metadata().location(),
            uuid_files[0]
        ))
        .is_ok());
    }

    #[tokio::test]
    async fn hadoop_alias_create_has_one_winner() {
        let (catalog, _dir) = catalog().await;
        let ident = create_orders(&catalog).await;
        let table = catalog.load_table(&ident).await.unwrap();
        let loc = table.metadata().location().to_string();
        let uuid = table.metadata_location().unwrap().to_string();
        catalog
            .publish_hadoop_alias(&ident, &loc, &uuid, 1)
            .await
            .unwrap();
        let lost = catalog
            .publish_hadoop_alias(&ident, &loc, &uuid, 1)
            .await
            .unwrap_err();
        assert_eq!(lost.kind(), ErrorKind::CatalogCommitConflicts);
    }

    #[tokio::test]
    async fn concurrent_hadoop_alias_create_has_one_winner() {
        let (catalog, _dir) = catalog().await;
        let ident = create_orders(&catalog).await;
        let table = catalog.load_table(&ident).await.unwrap();
        let loc = table.metadata().location().to_string();
        let uuid = table.metadata_location().unwrap().to_string();
        let a_cat = catalog.clone();
        let b_cat = catalog.clone();
        let a_ident = ident.clone();
        let b_ident = ident.clone();
        let a_loc = loc.clone();
        let b_loc = loc;
        let a_uuid = uuid.clone();
        let b_uuid = uuid;
        let (a, b) = tokio::join!(
            async move {
                a_cat
                    .publish_hadoop_alias(&a_ident, &a_loc, &a_uuid, 1)
                    .await
            },
            async move {
                b_cat
                    .publish_hadoop_alias(&b_ident, &b_loc, &b_uuid, 1)
                    .await
            },
        );
        let wins = a.is_ok() as u8 + b.is_ok() as u8;
        assert_eq!(wins, 1, "a={a:?} b={b:?}");
        let lost = if a.is_err() {
            a.unwrap_err()
        } else {
            b.unwrap_err()
        };
        assert_eq!(lost.kind(), ErrorKind::CatalogCommitConflicts);
    }

    #[tokio::test]
    async fn delete_after_commit_keeps_only_the_metadata_log_window() {
        let (catalog, _dir) = catalog().await;
        let ident = create_orders(&catalog).await;
        for i in 0..6 {
            let table = catalog.load_table(&ident).await.unwrap();
            let tx = Transaction::new(&table);
            let tx = tx
                .update_table_properties()
                .set(
                    "write.metadata.delete-after-commit.enabled".into(),
                    "true".into(),
                )
                .set("write.metadata.previous-versions-max".into(), "2".into())
                .set("k".into(), i.to_string())
                .apply(tx)
                .unwrap();
            tx.commit_without_rebase(&catalog).await.unwrap();
        }

        let loaded = catalog.load_table(&ident).await.unwrap();
        assert_eq!(loaded.metadata().properties().get("k").unwrap(), "5");
        let metadata_dir = PathBuf::from(strip_file(loaded.metadata().location())).join("metadata");
        let names: Vec<String> = std::fs::read_dir(&metadata_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".metadata.json"))
            .collect();
        let uuid_files = names.iter().filter(|n| !n.starts_with('v')).count();
        let aliases = names.iter().filter(|n| n.starts_with('v')).count();
        assert_eq!(uuid_files, 3, "{names:?}");
        assert_eq!(aliases, 3, "{names:?}");
    }

    #[tokio::test]
    async fn alias_below_the_latest_version_is_a_conflict_even_when_deleted() {
        let (catalog, _dir) = catalog().await;
        let ident = create_orders(&catalog).await;
        let table = catalog.load_table(&ident).await.unwrap();
        let tx = Transaction::new(&table);
        let tx = tx
            .update_table_properties()
            .set("k".into(), "v".into())
            .apply(tx)
            .unwrap();
        tx.commit_without_rebase(&catalog).await.unwrap();
        let table_fs = catalog.table_fs(&ident);
        std::fs::remove_file(table_fs.join("metadata/v0.metadata.json")).unwrap();

        let err = catalog
            .publish_hadoop_alias(
                &ident,
                table.metadata().location(),
                table.metadata_location().unwrap(),
                0,
            )
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::CatalogCommitConflicts);
        assert!(!table_fs.join("metadata/v0.metadata.json").exists());
    }

    #[tokio::test]
    async fn update_table_writes_next_hadoop_alias() {
        let (catalog, _dir) = catalog().await;
        let ident = create_orders(&catalog).await;
        let table = catalog.load_table(&ident).await.unwrap();
        let tx = Transaction::new(&table);
        let tx = tx
            .update_table_properties()
            .set("k".into(), "v".into())
            .apply(tx)
            .unwrap();
        tx.commit(&catalog).await.unwrap();
        let loaded = catalog.load_table(&ident).await.unwrap();
        assert!(
            loaded
                .metadata_location()
                .unwrap()
                .contains("/metadata/00001-"),
            "{}",
            loaded.metadata_location().unwrap()
        );
        let table_fs = PathBuf::from(strip_file(loaded.metadata().location()));
        assert_eq!(
            std::fs::read_to_string(table_fs.join("metadata/version-hint.text")).unwrap(),
            "1"
        );
        assert!(table_fs.join("metadata/v1.metadata.json").is_file());
    }

    #[tokio::test]
    async fn committed_metadata_is_served_without_rereading_or_scanning() {
        let (catalog, _dir) = catalog().await;
        let ident = create_orders(&catalog).await;
        let table = catalog.load_table(&ident).await.unwrap();
        let tx = Transaction::new(&table);
        let tx = tx
            .update_table_properties()
            .set("k".into(), "v".into())
            .apply(tx)
            .unwrap();
        let committed = tx.commit(&catalog).await.unwrap();
        let uuid_location = committed.metadata_location().unwrap().to_string();
        std::fs::remove_file(strip_file(&uuid_location)).unwrap();

        let loaded = catalog.load_table(&ident).await.unwrap();
        assert_eq!(loaded.metadata_location().unwrap(), uuid_location);
        assert_eq!(
            loaded.metadata().properties().get("k").map(String::as_str),
            Some("v")
        );
    }

    #[tokio::test]
    async fn recreated_table_does_not_reuse_the_dropped_alias() {
        let (catalog, _dir) = catalog().await;
        let ident = create_orders(&catalog).await;
        let first = catalog.load_table(&ident).await.unwrap();
        catalog.drop_table(&ident).await.unwrap();
        catalog
            .create_table(
                ident.namespace(),
                TableCreation::builder()
                    .name("orders".into())
                    .schema(schema())
                    .build(),
            )
            .await
            .unwrap();
        let second = catalog.load_table(&ident).await.unwrap();
        assert_ne!(first.metadata().uuid(), second.metadata().uuid());
        assert_ne!(first.metadata_location(), second.metadata_location());
    }

    #[tokio::test]
    async fn load_recovers_when_hint_is_missing() {
        let (catalog, _dir) = catalog().await;
        let ident = create_orders(&catalog).await;
        let table_fs = catalog.table_fs(&ident);
        std::fs::remove_file(table_fs.join("metadata/version-hint.text")).unwrap();
        let loaded = catalog.load_table(&ident).await.unwrap();
        assert!(
            loaded
                .metadata_location()
                .unwrap()
                .contains("/metadata/00000-"),
            "{}",
            loaded.metadata_location().unwrap()
        );
    }

    #[tokio::test]
    async fn load_recovers_when_hint_is_stale() {
        let (catalog, _dir) = catalog().await;
        let ident = create_orders(&catalog).await;
        let table = catalog.load_table(&ident).await.unwrap();
        let tx = Transaction::new(&table);
        let tx = tx
            .update_table_properties()
            .set("k".into(), "v".into())
            .apply(tx)
            .unwrap();
        tx.commit(&catalog).await.unwrap();
        let table_fs = catalog.table_fs(&ident);
        std::fs::write(table_fs.join("metadata/version-hint.text"), "0").unwrap();
        let loaded = catalog.load_table(&ident).await.unwrap();
        assert!(
            loaded
                .metadata_location()
                .unwrap()
                .contains("/metadata/00001-"),
            "{}",
            loaded.metadata_location().unwrap()
        );
    }

    #[test]
    fn rejects_s3_warehouse() {
        let err = FsCatalog::new("s3://bucket/wh", FileIO::new_with_fs()).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::FeatureUnsupported);
    }

    #[test]
    fn rejects_bare_absolute_warehouse() {
        let err = FsCatalog::new("/tmp/lake", FileIO::new_with_fs()).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::DataInvalid);
    }

    #[tokio::test]
    async fn load_table_uses_hadoop_alias_not_orphan_uuid() {
        let (catalog, _dir) = catalog().await;
        let ident = create_orders(&catalog).await;
        let loaded = catalog.load_table(&ident).await.unwrap();
        let winner = loaded.metadata_location().unwrap().to_string();
        let table_fs = catalog.table_fs(&ident);
        std::fs::write(
            table_fs.join("metadata/00000-00000000-0000-0000-0000-000000000000.metadata.json"),
            b"orphan",
        )
        .unwrap();
        let again = catalog.load_table(&ident).await.unwrap();
        assert_eq!(again.metadata_location().unwrap(), winner);
    }

    #[tokio::test]
    async fn table_exists_is_hadoop_alias_not_metadata_dir() {
        let (catalog, _dir) = catalog().await;
        let ident = create_orders(&catalog).await;
        assert!(catalog.table_exists(&ident).await.unwrap());
        let table_fs = catalog.table_fs(&ident);
        for entry in std::fs::read_dir(table_fs.join("metadata"))
            .unwrap()
            .flatten()
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('v') && name.ends_with(".metadata.json") {
                std::fs::remove_file(entry.path()).unwrap();
            }
        }
        std::fs::remove_file(table_fs.join("metadata/version-hint.text")).ok();
        assert!(!catalog.table_exists(&ident).await.unwrap());
        assert!(catalog.load_table(&ident).await.is_err());
        let listed = catalog.list_tables(ident.namespace()).await.unwrap();
        assert!(
            !listed.iter().any(|t| t.name() == ident.name()),
            "{listed:?}"
        );
        let drop_err = catalog.drop_table(&ident).await.unwrap_err();
        assert_eq!(drop_err.kind(), ErrorKind::TableNotFound);
    }

    #[tokio::test]
    async fn create_table_requires_existing_namespace() {
        let (catalog, _dir) = catalog().await;
        let ns = NamespaceIdent::new("bronze".into());
        let err = catalog
            .create_table(
                &ns,
                TableCreation::builder()
                    .name("orders".into())
                    .schema(schema())
                    .build(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NamespaceNotFound);
    }
}
