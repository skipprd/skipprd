//! Iceberg catalog pointers on Cloud Tables. Uses skippr-cloud Client::from_env().

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use iceberg::io::{FileIO, FileIOBuilder};
use iceberg::spec::{TableMetadata, TableMetadataBuilder};
use iceberg::table::Table;
use iceberg::{
    Catalog, Error, ErrorKind, Namespace, NamespaceIdent, Result, TableCommit, TableCreation,
    TableIdent,
};
use serde_json::json;
use skippr_cloud::{attr_n, attr_s, n, s, Client};
use skippr_iceberg_catalog::{
    decode_name, encode_name, skippr_catalog_reuses_offset_table, warehouse_hash,
    IcebergCatalogConfig,
};
use uuid::Uuid;

const CATALOG_MAX_RETRIES: usize = 8;

#[derive(Clone)]
pub struct CloudTablesCatalog {
    client: Arc<Client>,
    table: String,
    warehouse: String,
    warehouse_pk: String,
    file_io: FileIO,
}

impl std::fmt::Debug for CloudTablesCatalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudTablesCatalog")
            .field("table", &self.table)
            .field("warehouse", &self.warehouse)
            .finish_non_exhaustive()
    }
}

impl CloudTablesCatalog {
    pub async fn new(config: &IcebergCatalogConfig) -> Result<Self> {
        let IcebergCatalogConfig::Skippr {
            table, warehouse, ..
        } = config
        else {
            return Err(Error::new(
                ErrorKind::DataInvalid,
                "CloudTablesCatalog requires catalog type skippr",
            ));
        };
        let offset_table = std::env::var("SKIPPR_OFFSET_DYNAMODB_TABLE").unwrap_or_default();
        if skippr_catalog_reuses_offset_table(table, &offset_table) {
            return Err(Error::new(
                ErrorKind::DataInvalid,
                format!(
                    "Iceberg catalog.table '{table}' must not be SKIPPR_OFFSET_DYNAMODB_TABLE; create a separate catalog table"
                ),
            ));
        }
        let client =
            Client::from_env().map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
        Ok(Self {
            client: Arc::new(client),
            table: table.clone(),
            warehouse: warehouse.clone(),
            warehouse_pk: format!("catalog#{}", warehouse_hash(warehouse)),
            file_io: file_io_for_warehouse(warehouse),
        })
    }

    pub fn with_file_io(mut self, file_io: FileIO) -> Self {
        self.file_io = file_io;
        self
    }

    fn namespace_sk(ns: &NamespaceIdent) -> String {
        format!("namespace#{}", encode_name(&ns.to_url_string()))
    }

    fn table_pk(&self, ns: &NamespaceIdent) -> String {
        format!(
            "{}#namespace#{}",
            self.warehouse_pk,
            encode_name(&ns.to_url_string())
        )
    }

    fn table_sk(name: &str) -> String {
        format!("table#{}", encode_name(name))
    }

    async fn table_from_location(
        &self,
        ident: TableIdent,
        metadata_location: &str,
    ) -> Result<Table> {
        let metadata = TableMetadata::read_from(&self.file_io, metadata_location).await?;
        Table::builder()
            .file_io(self.file_io.clone())
            .metadata_location(metadata_location.to_string())
            .metadata(metadata)
            .identifier(ident)
            .build()
    }

    async fn pointer(&self, table: &TableIdent) -> Result<(String, i64)> {
        let item = self
            .client
            .get_item(
                &self.table,
                &self.table_pk(table.namespace()),
                &Self::table_sk(table.name()),
                true,
            )
            .await
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?
            .ok_or_else(|| Error::new(ErrorKind::TableNotFound, format!("{table:?}")))?;
        let location = attr_s(&item, "metadata_location")
            .ok_or_else(|| Error::new(ErrorKind::DataInvalid, "missing metadata_location"))?;
        let generation = attr_n(&item, "generation")
            .ok_or_else(|| Error::new(ErrorKind::DataInvalid, "missing generation"))?
            as i64;
        Ok((location, generation))
    }

    async fn bump_table_count(&self, namespace: &NamespaceIdent, delta: i64) -> Result<()> {
        let expr = if delta >= 0 {
            "SET table_count = table_count + :one"
        } else {
            "SET table_count = table_count - :one"
        };
        let mut values = json!({ ":one": n(1) });
        let condition = if delta >= 0 {
            "attribute_exists(PK)"
        } else {
            values[":zero"] = n(0);
            "attribute_exists(PK) AND table_count > :zero"
        };
        self.client
            .update_item(
                &self.table,
                &self.warehouse_pk,
                &Self::namespace_sk(namespace),
                expr,
                values,
                Some(condition),
                false,
            )
            .await
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
        Ok(())
    }
}

pub fn file_io_for_warehouse(warehouse: &str) -> FileIO {
    if warehouse.starts_with("memory:") || warehouse.starts_with("memory://") {
        FileIO::new_with_memory()
    } else if warehouse.starts_with("s3://") || warehouse.starts_with("s3a://") {
        FileIOBuilder::new(Arc::new(
            iceberg_storage_opendal::OpenDalStorageFactory::S3 {
                configured_scheme: "s3".to_string(),
                customized_credential_load: None,
            },
        ))
        .build()
    } else {
        FileIO::new_with_fs()
    }
}

#[async_trait]
impl Catalog for CloudTablesCatalog {
    async fn list_namespaces(
        &self,
        _parent: Option<&NamespaceIdent>,
    ) -> Result<Vec<NamespaceIdent>> {
        let items = self
            .client
            .query(
                &self.table,
                "PK = :pk AND begins_with(SK, :sk)",
                json!({
                    ":pk": s(self.warehouse_pk.clone()),
                    ":sk": s("namespace#"),
                }),
            )
            .await
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
        let mut namespaces = Vec::new();
        for item in items {
            if let Some(sk) = attr_s(&item, "SK") {
                if let Some(encoded) = sk.strip_prefix("namespace#") {
                    if let Ok(name) = decode_name(encoded) {
                        if let Ok(ident) = NamespaceIdent::from_strs(name.split('\u{1f}')) {
                            namespaces.push(ident);
                        }
                    }
                }
            }
        }
        Ok(namespaces)
    }

    async fn create_namespace(
        &self,
        namespace: &NamespaceIdent,
        properties: HashMap<String, String>,
    ) -> Result<Namespace> {
        let props = serde_json::to_string(&properties)
            .map_err(|err| Error::new(ErrorKind::DataInvalid, err.to_string()))?;
        let item = json!({
            "PK": s(self.warehouse_pk.clone()),
            "SK": s(Self::namespace_sk(namespace)),
            "properties": s(props),
            "table_count": n(0),
        });
        self.client
            .put_item(&self.table, item, Some("attribute_not_exists(PK)"), None)
            .await
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
        Ok(Namespace::with_properties(namespace.clone(), properties))
    }

    async fn get_namespace(&self, namespace: &NamespaceIdent) -> Result<Namespace> {
        let item = self
            .client
            .get_item(
                &self.table,
                &self.warehouse_pk,
                &Self::namespace_sk(namespace),
                true,
            )
            .await
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::NamespaceNotFound,
                    format!("namespace {namespace:?}"),
                )
            })?;
        let properties = attr_s(&item, "properties")
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        Ok(Namespace::with_properties(namespace.clone(), properties))
    }

    async fn namespace_exists(&self, namespace: &NamespaceIdent) -> Result<bool> {
        Ok(self.get_namespace(namespace).await.is_ok())
    }

    async fn update_namespace(
        &self,
        namespace: &NamespaceIdent,
        properties: HashMap<String, String>,
    ) -> Result<()> {
        let props = serde_json::to_string(&properties)
            .map_err(|err| Error::new(ErrorKind::DataInvalid, err.to_string()))?;
        self.client
            .update_item(
                &self.table,
                &self.warehouse_pk,
                &Self::namespace_sk(namespace),
                "SET properties = :props",
                json!({ ":props": s(props) }),
                Some("attribute_exists(PK)"),
                false,
            )
            .await
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
        Ok(())
    }

    async fn drop_namespace(&self, namespace: &NamespaceIdent) -> Result<()> {
        self.client
            .delete_item(
                &self.table,
                &self.warehouse_pk,
                &Self::namespace_sk(namespace),
                Some("attribute_exists(PK) AND table_count = :zero"),
                Some(json!({ ":zero": n(0) })),
            )
            .await
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
        Ok(())
    }

    async fn list_tables(&self, namespace: &NamespaceIdent) -> Result<Vec<TableIdent>> {
        let items = self
            .client
            .query(
                &self.table,
                "PK = :pk AND begins_with(SK, :sk)",
                json!({
                    ":pk": s(self.table_pk(namespace)),
                    ":sk": s("table#"),
                }),
            )
            .await
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
        let mut tables = Vec::new();
        for item in items {
            if let Some(sk) = attr_s(&item, "SK") {
                if let Some(encoded) = sk.strip_prefix("table#") {
                    if let Ok(name) = decode_name(encoded) {
                        tables.push(TableIdent::new(namespace.clone(), name));
                    }
                }
            }
        }
        Ok(tables)
    }

    async fn create_table(
        &self,
        namespace: &NamespaceIdent,
        creation: TableCreation,
    ) -> Result<Table> {
        let ident = TableIdent::new(namespace.clone(), creation.name.clone());
        let location = creation.location.clone().unwrap_or_else(|| {
            format!("{}/{}", self.warehouse.trim_end_matches('/'), ident.name())
        });
        let metadata = TableMetadataBuilder::from_table_creation(TableCreation {
            location: Some(location.clone()),
            ..creation
        })?
        .build()?
        .metadata;
        let metadata_location = format!(
            "{}/metadata/00000-{}.metadata.json",
            location.trim_end_matches('/'),
            Uuid::new_v4()
        );
        metadata.write_to(&self.file_io, &metadata_location).await?;
        let transact = json!([
            {
                "put": {
                    "tableName": self.table,
                    "item": {
                        "PK": s(self.table_pk(namespace)),
                        "SK": s(Self::table_sk(ident.name())),
                        "table_uuid": s(metadata.uuid().to_string()),
                        "metadata_location": s(metadata_location.clone()),
                        "generation": n(1),
                    },
                    "conditionExpression": "attribute_not_exists(PK)"
                }
            },
            {
                "conditionCheck": {
                    "tableName": self.table,
                    "key": {
                        "PK": s(self.warehouse_pk.clone()),
                        "SK": s(Self::namespace_sk(namespace)),
                    },
                    "conditionExpression": "attribute_exists(PK)"
                }
            }
        ]);
        self.client
            .transact_write(transact)
            .await
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
        self.bump_table_count(namespace, 1).await?;
        self.table_from_location(ident, &metadata_location).await
    }

    async fn load_table(&self, table: &TableIdent) -> Result<Table> {
        let (location, _) = self.pointer(table).await?;
        self.table_from_location(table.clone(), &location).await
    }

    async fn drop_table(&self, table: &TableIdent) -> Result<()> {
        let transact = json!([
            {
                "delete": {
                    "tableName": self.table,
                    "key": {
                        "PK": s(self.table_pk(table.namespace())),
                        "SK": s(Self::table_sk(table.name())),
                    },
                    "conditionExpression": "attribute_exists(PK)"
                }
            },
            {
                "conditionCheck": {
                    "tableName": self.table,
                    "key": {
                        "PK": s(self.warehouse_pk.clone()),
                        "SK": s(Self::namespace_sk(table.namespace())),
                    },
                    "conditionExpression": "attribute_exists(PK) AND table_count > :zero",
                    "expressionAttributeValues": { ":zero": n(0) }
                }
            }
        ]);
        self.client
            .transact_write(transact)
            .await
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
        self.bump_table_count(table.namespace(), -1).await?;
        Ok(())
    }

    async fn table_exists(&self, table: &TableIdent) -> Result<bool> {
        let item = self
            .client
            .get_item(
                &self.table,
                &self.table_pk(table.namespace()),
                &Self::table_sk(table.name()),
                true,
            )
            .await
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
        Ok(item.is_some())
    }

    async fn rename_table(&self, src: &TableIdent, dest: &TableIdent) -> Result<()> {
        let (location, generation) = self.pointer(src).await?;
        if self.table_exists(dest).await? {
            return Err(Error::new(
                ErrorKind::Unexpected,
                format!("rename destination exists: {dest:?}"),
            ));
        }
        let transact = json!([
            {
                "put": {
                    "tableName": self.table,
                    "item": {
                        "PK": s(self.table_pk(dest.namespace())),
                        "SK": s(Self::table_sk(dest.name())),
                        "metadata_location": s(location),
                        "generation": n(generation),
                    },
                    "conditionExpression": "attribute_not_exists(PK)"
                }
            },
            {
                "delete": {
                    "tableName": self.table,
                    "key": {
                        "PK": s(self.table_pk(src.namespace())),
                        "SK": s(Self::table_sk(src.name())),
                    },
                    "conditionExpression": "attribute_exists(PK) AND generation = :gen",
                    "expressionAttributeValues": { ":gen": n(generation) }
                }
            }
        ]);
        self.client
            .transact_write(transact)
            .await
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
        if src.namespace() != dest.namespace() {
            self.bump_table_count(src.namespace(), -1).await?;
            self.bump_table_count(dest.namespace(), 1).await?;
        }
        Ok(())
    }

    async fn register_table(&self, table: &TableIdent, metadata_location: String) -> Result<Table> {
        let transact = json!([
            {
                "put": {
                    "tableName": self.table,
                    "item": {
                        "PK": s(self.table_pk(table.namespace())),
                        "SK": s(Self::table_sk(table.name())),
                        "metadata_location": s(metadata_location.clone()),
                        "generation": n(1),
                    },
                    "conditionExpression": "attribute_not_exists(PK)"
                }
            },
            {
                "conditionCheck": {
                    "tableName": self.table,
                    "key": {
                        "PK": s(self.warehouse_pk.clone()),
                        "SK": s(Self::namespace_sk(table.namespace())),
                    },
                    "conditionExpression": "attribute_exists(PK)"
                }
            }
        ]);
        self.client
            .transact_write(transact)
            .await
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
        self.bump_table_count(table.namespace(), 1).await?;
        self.table_from_location(table.clone(), &metadata_location)
            .await
    }

    async fn update_table(&self, commit: TableCommit) -> Result<Table> {
        let ident = commit.identifier().clone();
        let (expected_location, mut generation) = self.pointer(&ident).await?;
        let current = self.load_table(&ident).await?;
        let staged = commit.apply(current)?;
        let new_location = staged.metadata_location_result()?.to_string();
        staged
            .metadata()
            .write_to(staged.file_io(), &new_location)
            .await?;
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self
                .client
                .update_item(
                    &self.table,
                    &self.table_pk(ident.namespace()),
                    &Self::table_sk(ident.name()),
                    "SET generation = generation + :one, metadata_location = :new, previous_metadata_location = :loc",
                    json!({
                        ":gen": n(generation),
                        ":loc": s(expected_location.clone()),
                        ":new": s(new_location.clone()),
                        ":one": n(1),
                    }),
                    Some("attribute_exists(PK) AND generation = :gen AND metadata_location = :loc"),
                    false,
                )
                .await
            {
                Ok(_) => return Ok(staged),
                Err(err) => {
                    let msg = err.to_string();
                    if attempt >= CATALOG_MAX_RETRIES {
                        return Err(Error::new(ErrorKind::Unexpected, msg));
                    }
                    if err.is_conditional_check_failed() {
                        skippr_iceberg_catalog::record_cas_conflict();
                        match self.pointer(&ident).await {
                            Ok((loc, _)) if loc == new_location => return Ok(staged),
                            Ok((loc, gen)) if loc == expected_location => {
                                generation = gen;
                                continue;
                            }
                            Ok(_) => {
                                return Err(Error::new(
                                    ErrorKind::Unexpected,
                                    format!("Iceberg catalog pointer CAS conflict: {msg}"),
                                ));
                            }
                            Err(err) => return Err(err),
                        }
                    } else {
                        return Err(Error::new(ErrorKind::Unexpected, msg));
                    }
                }
            }
        }
    }
}
