//! Complete Iceberg [`Catalog`] backed by DynamoDB pointers and object-store metadata.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use aws_sdk_dynamodb::types::{AttributeValue, Put, TransactWriteItem, Update};
use aws_sdk_dynamodb::Client;
use iceberg::io::{FileIO, FileIOBuilder};
use iceberg::spec::{TableMetadata, TableMetadataBuilder};
use iceberg::table::Table;
use iceberg::{
    Catalog, Error, ErrorKind, Namespace, NamespaceIdent, Result, TableCommit, TableCreation,
    TableIdent,
};
use skippr_iceberg_catalog::{
    decode_name, encode_name, skippr_catalog_reuses_offset_table, warehouse_hash,
    IcebergCatalogConfig,
};
use uuid::Uuid;

const CATALOG_MAX_RETRIES: usize = 8;

fn is_conditional_check_failed(err: &impl aws_sdk_dynamodb::error::ProvideErrorMetadata) -> bool {
    err.code() == Some("ConditionalCheckFailedException")
}

#[derive(Debug, Clone)]
pub struct DynamoDbCatalog {
    client: Arc<Client>,
    table: String,
    warehouse: String,
    warehouse_pk: String,
    file_io: FileIO,
}

impl DynamoDbCatalog {
    pub async fn new(config: &IcebergCatalogConfig) -> Result<Self> {
        let IcebergCatalogConfig::Skippr {
            table,
            warehouse,
            region,
        } = config
        else {
            return Err(Error::new(
                ErrorKind::DataInvalid,
                "DynamoDbCatalog requires catalog type skippr",
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
        };
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
        if let Some(region) = region {
            loader = loader.region(aws_config::Region::new(region.clone()));
        }
        if let Ok(url) = std::env::var("AWS_ENDPOINT_URL_DYNAMODB") {
            if !url.trim().is_empty() {
                loader = loader.endpoint_url(url);
            }
        }
        let shared = loader.load().await;
        Ok(Self {
            client: Arc::new(Client::new(&shared)),
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
        let out = self
            .client
            .get_item()
            .table_name(&self.table)
            .key("PK", AttributeValue::S(self.table_pk(table.namespace())))
            .key("SK", AttributeValue::S(Self::table_sk(table.name())))
            .consistent_read(true)
            .send()
            .await
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
        let item = out
            .item
            .ok_or_else(|| Error::new(ErrorKind::TableNotFound, format!("{table:?}")))?;
        let location = item
            .get("metadata_location")
            .and_then(|v| v.as_s().ok())
            .cloned()
            .ok_or_else(|| Error::new(ErrorKind::DataInvalid, "missing metadata_location"))?;
        let generation = item
            .get("generation")
            .and_then(|v| v.as_n().ok())
            .and_then(|n| n.parse().ok())
            .ok_or_else(|| Error::new(ErrorKind::DataInvalid, "missing generation"))?;
        Ok((location, generation))
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
impl Catalog for DynamoDbCatalog {
    async fn list_namespaces(
        &self,
        _parent: Option<&NamespaceIdent>,
    ) -> Result<Vec<NamespaceIdent>> {
        let out = self
            .client
            .query()
            .table_name(&self.table)
            .key_condition_expression("PK = :pk AND begins_with(SK, :sk)")
            .expression_attribute_values(":pk", AttributeValue::S(self.warehouse_pk.clone()))
            .expression_attribute_values(":sk", AttributeValue::S("namespace#".into()))
            .send()
            .await
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
        let mut namespaces = Vec::new();
        for item in out.items() {
            if let Some(sk) = item.get("SK").and_then(|v| v.as_s().ok()) {
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
        self.client
            .put_item()
            .table_name(&self.table)
            .item("PK", AttributeValue::S(self.warehouse_pk.clone()))
            .item("SK", AttributeValue::S(Self::namespace_sk(namespace)))
            .item("properties", AttributeValue::S(props))
            .item("table_count", AttributeValue::N("0".into()))
            .condition_expression("attribute_not_exists(PK)")
            .send()
            .await
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
        Ok(Namespace::with_properties(namespace.clone(), properties))
    }

    async fn get_namespace(&self, namespace: &NamespaceIdent) -> Result<Namespace> {
        let out = self
            .client
            .get_item()
            .table_name(&self.table)
            .key("PK", AttributeValue::S(self.warehouse_pk.clone()))
            .key("SK", AttributeValue::S(Self::namespace_sk(namespace)))
            .consistent_read(true)
            .send()
            .await
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
        let item = out.item.ok_or_else(|| {
            Error::new(
                ErrorKind::NamespaceNotFound,
                format!("namespace {namespace:?}"),
            )
        })?;
        let properties = item
            .get("properties")
            .and_then(|v| v.as_s().ok())
            .and_then(|raw| serde_json::from_str(raw).ok())
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
            .update_item()
            .table_name(&self.table)
            .key("PK", AttributeValue::S(self.warehouse_pk.clone()))
            .key("SK", AttributeValue::S(Self::namespace_sk(namespace)))
            .condition_expression("attribute_exists(PK)")
            .update_expression("SET properties = :props")
            .expression_attribute_values(":props", AttributeValue::S(props))
            .send()
            .await
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
        Ok(())
    }

    async fn drop_namespace(&self, namespace: &NamespaceIdent) -> Result<()> {
        self.client
            .delete_item()
            .table_name(&self.table)
            .key("PK", AttributeValue::S(self.warehouse_pk.clone()))
            .key("SK", AttributeValue::S(Self::namespace_sk(namespace)))
            .condition_expression("attribute_exists(PK) AND table_count = :zero")
            .expression_attribute_values(":zero", AttributeValue::N("0".into()))
            .send()
            .await
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
        Ok(())
    }

    async fn list_tables(&self, namespace: &NamespaceIdent) -> Result<Vec<TableIdent>> {
        let out = self
            .client
            .query()
            .table_name(&self.table)
            .key_condition_expression("PK = :pk AND begins_with(SK, :sk)")
            .expression_attribute_values(":pk", AttributeValue::S(self.table_pk(namespace)))
            .expression_attribute_values(":sk", AttributeValue::S("table#".into()))
            .send()
            .await
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
        let mut tables = Vec::new();
        for item in out.items() {
            if let Some(sk) = item.get("SK").and_then(|v| v.as_s().ok()) {
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
        let put = Put::builder()
            .table_name(&self.table)
            .item("PK", AttributeValue::S(self.table_pk(namespace)))
            .item("SK", AttributeValue::S(Self::table_sk(ident.name())))
            .item("table_uuid", AttributeValue::S(metadata.uuid().to_string()))
            .item(
                "metadata_location",
                AttributeValue::S(metadata_location.clone()),
            )
            .item("generation", AttributeValue::N("1".into()))
            .condition_expression("attribute_not_exists(PK)")
            .build()
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
        let update = Update::builder()
            .table_name(&self.table)
            .key("PK", AttributeValue::S(self.warehouse_pk.clone()))
            .key("SK", AttributeValue::S(Self::namespace_sk(namespace)))
            .update_expression("SET table_count = table_count + :one")
            .expression_attribute_values(":one", AttributeValue::N("1".into()))
            .condition_expression("attribute_exists(PK)")
            .build()
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
        self.client
            .transact_write_items()
            .transact_items(TransactWriteItem::builder().put(put).build())
            .transact_items(TransactWriteItem::builder().update(update).build())
            .send()
            .await
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
        self.table_from_location(ident, &metadata_location).await
    }

    async fn load_table(&self, table: &TableIdent) -> Result<Table> {
        let (location, _) = self.pointer(table).await?;
        self.table_from_location(table.clone(), &location).await
    }

    async fn drop_table(&self, table: &TableIdent) -> Result<()> {
        let delete = aws_sdk_dynamodb::types::Delete::builder()
            .table_name(&self.table)
            .key("PK", AttributeValue::S(self.table_pk(table.namespace())))
            .key("SK", AttributeValue::S(Self::table_sk(table.name())))
            .condition_expression("attribute_exists(PK)")
            .build()
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
        let update = Update::builder()
            .table_name(&self.table)
            .key("PK", AttributeValue::S(self.warehouse_pk.clone()))
            .key(
                "SK",
                AttributeValue::S(Self::namespace_sk(table.namespace())),
            )
            .update_expression("SET table_count = table_count - :one")
            .expression_attribute_values(":one", AttributeValue::N("1".into()))
            .condition_expression("attribute_exists(PK) AND table_count > :zero")
            .expression_attribute_values(":zero", AttributeValue::N("0".into()))
            .build()
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
        self.client
            .transact_write_items()
            .transact_items(TransactWriteItem::builder().delete(delete).build())
            .transact_items(TransactWriteItem::builder().update(update).build())
            .send()
            .await
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
        Ok(())
    }

    async fn table_exists(&self, table: &TableIdent) -> Result<bool> {
        let out = self
            .client
            .get_item()
            .table_name(&self.table)
            .key("PK", AttributeValue::S(self.table_pk(table.namespace())))
            .key("SK", AttributeValue::S(Self::table_sk(table.name())))
            .consistent_read(true)
            .send()
            .await
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
        Ok(out.item.is_some())
    }

    async fn rename_table(&self, src: &TableIdent, dest: &TableIdent) -> Result<()> {
        let (location, generation) = self.pointer(src).await?;
        if self.table_exists(dest).await? {
            return Err(Error::new(
                ErrorKind::Unexpected,
                format!("rename destination exists: {dest:?}"),
            ));
        }
        let put = Put::builder()
            .table_name(&self.table)
            .item("PK", AttributeValue::S(self.table_pk(dest.namespace())))
            .item("SK", AttributeValue::S(Self::table_sk(dest.name())))
            .item("metadata_location", AttributeValue::S(location.clone()))
            .item("generation", AttributeValue::N(generation.to_string()))
            .condition_expression("attribute_not_exists(PK)")
            .build()
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
        let delete = aws_sdk_dynamodb::types::Delete::builder()
            .table_name(&self.table)
            .key("PK", AttributeValue::S(self.table_pk(src.namespace())))
            .key("SK", AttributeValue::S(Self::table_sk(src.name())))
            .condition_expression("attribute_exists(PK) AND generation = :gen")
            .expression_attribute_values(":gen", AttributeValue::N(generation.to_string()))
            .build()
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
        let mut tx = self
            .client
            .transact_write_items()
            .transact_items(TransactWriteItem::builder().put(put).build())
            .transact_items(TransactWriteItem::builder().delete(delete).build());
        if src.namespace() != dest.namespace() {
            let dec = Update::builder()
                .table_name(&self.table)
                .key("PK", AttributeValue::S(self.warehouse_pk.clone()))
                .key("SK", AttributeValue::S(Self::namespace_sk(src.namespace())))
                .update_expression("SET table_count = table_count - :one")
                .expression_attribute_values(":one", AttributeValue::N("1".into()))
                .condition_expression("attribute_exists(PK) AND table_count > :zero")
                .expression_attribute_values(":zero", AttributeValue::N("0".into()))
                .build()
                .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
            let inc = Update::builder()
                .table_name(&self.table)
                .key("PK", AttributeValue::S(self.warehouse_pk.clone()))
                .key(
                    "SK",
                    AttributeValue::S(Self::namespace_sk(dest.namespace())),
                )
                .update_expression("SET table_count = table_count + :one")
                .expression_attribute_values(":one", AttributeValue::N("1".into()))
                .condition_expression("attribute_exists(PK)")
                .build()
                .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
            tx = tx
                .transact_items(TransactWriteItem::builder().update(dec).build())
                .transact_items(TransactWriteItem::builder().update(inc).build());
        }
        tx.send()
            .await
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
        Ok(())
    }

    async fn register_table(&self, table: &TableIdent, metadata_location: String) -> Result<Table> {
        let put = Put::builder()
            .table_name(&self.table)
            .item("PK", AttributeValue::S(self.table_pk(table.namespace())))
            .item("SK", AttributeValue::S(Self::table_sk(table.name())))
            .item(
                "metadata_location",
                AttributeValue::S(metadata_location.clone()),
            )
            .item("generation", AttributeValue::N("1".into()))
            .condition_expression("attribute_not_exists(PK)")
            .build()
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
        let update = Update::builder()
            .table_name(&self.table)
            .key("PK", AttributeValue::S(self.warehouse_pk.clone()))
            .key(
                "SK",
                AttributeValue::S(Self::namespace_sk(table.namespace())),
            )
            .update_expression("SET table_count = table_count + :one")
            .expression_attribute_values(":one", AttributeValue::N("1".into()))
            .condition_expression("attribute_exists(PK)")
            .build()
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
        self.client
            .transact_write_items()
            .transact_items(TransactWriteItem::builder().put(put).build())
            .transact_items(TransactWriteItem::builder().update(update).build())
            .send()
            .await
            .map_err(|err| Error::new(ErrorKind::Unexpected, err.to_string()))?;
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
                .update_item()
                .table_name(&self.table)
                .key("PK", AttributeValue::S(self.table_pk(ident.namespace())))
                .key("SK", AttributeValue::S(Self::table_sk(ident.name())))
                .condition_expression(
                    "attribute_exists(PK) AND generation = :gen AND metadata_location = :loc",
                )
                .update_expression(
                    "SET generation = generation + :one, metadata_location = :new, previous_metadata_location = :loc",
                )
                .expression_attribute_values(":gen", AttributeValue::N(generation.to_string()))
                .expression_attribute_values(":loc", AttributeValue::S(expected_location.clone()))
                .expression_attribute_values(":new", AttributeValue::S(new_location.clone()))
                .expression_attribute_values(":one", AttributeValue::N("1".into()))
                .send()
                .await
            {
                Ok(_) => return Ok(staged),
                Err(err) => {
                    let msg = err.to_string();
                    if attempt >= CATALOG_MAX_RETRIES {
                        return Err(Error::new(ErrorKind::Unexpected, msg));
                    }
                    if is_conditional_check_failed(&err) {
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
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use skippr_iceberg_catalog::{decode_name, encode_name};

    #[test]
    fn table_sk_uses_length_prefixed_encoding() {
        let encoded = encode_name("events");
        assert_eq!(decode_name(&encoded).unwrap(), "events");
    }

    #[test]
    fn memory_warehouse_builds_file_io() {
        let _ = super::file_io_for_warehouse("memory://warehouse");
        let _ = super::file_io_for_warehouse("/tmp/warehouse");
    }
}
