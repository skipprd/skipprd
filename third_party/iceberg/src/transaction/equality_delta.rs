// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use crate::error::Result;
use crate::spec::{DataFile, ManifestFile, Operation};
use crate::table::Table;
use crate::transaction::snapshot::{
    DefaultManifestProcess, SnapshotProduceOperation, SnapshotProducer,
};
use crate::transaction::{ActionCommit, TransactionAction};

/// Commits new data files and equality-delete files in a single MoR snapshot.
pub struct EqualityDeltaAppendAction {
    check_duplicate: bool,
    commit_uuid: Option<Uuid>,
    key_metadata: Option<Vec<u8>>,
    snapshot_properties: HashMap<String, String>,
    added_data_files: Vec<DataFile>,
    added_delete_files: Vec<DataFile>,
}

impl EqualityDeltaAppendAction {
    pub(crate) fn new() -> Self {
        Self {
            check_duplicate: true,
            commit_uuid: None,
            key_metadata: None,
            snapshot_properties: HashMap::default(),
            added_data_files: vec![],
            added_delete_files: vec![],
        }
    }

    /// Set whether to check duplicate files.
    pub fn with_check_duplicate(mut self, v: bool) -> Self {
        self.check_duplicate = v;
        self
    }

    /// Add data files to the snapshot.
    pub fn add_data_files(mut self, data_files: impl IntoIterator<Item = DataFile>) -> Self {
        self.added_data_files.extend(data_files);
        self
    }

    /// Add equality-delete files to the snapshot.
    pub fn add_delete_files(mut self, delete_files: impl IntoIterator<Item = DataFile>) -> Self {
        self.added_delete_files.extend(delete_files);
        self
    }

    /// Set commit UUID for the snapshot.
    pub fn set_commit_uuid(mut self, commit_uuid: Uuid) -> Self {
        self.commit_uuid = Some(commit_uuid);
        self
    }

    /// Set key metadata for manifest files.
    pub fn set_key_metadata(mut self, key_metadata: Vec<u8>) -> Self {
        self.key_metadata = Some(key_metadata);
        self
    }

    /// Set snapshot summary properties.
    pub fn set_snapshot_properties(mut self, snapshot_properties: HashMap<String, String>) -> Self {
        self.snapshot_properties = snapshot_properties;
        self
    }
}

#[async_trait]
impl TransactionAction for EqualityDeltaAppendAction {
    async fn commit(self: Arc<Self>, table: &Table) -> Result<ActionCommit> {
        if self.added_data_files.is_empty() && self.added_delete_files.is_empty() {
            return Err(crate::Error::new(
                crate::ErrorKind::PreconditionFailed,
                "Equality delta commit requires at least one data or delete file",
            ));
        }

        let snapshot_producer = SnapshotProducer::new_with_deletes(
            table,
            self.commit_uuid.unwrap_or_else(Uuid::now_v7),
            self.key_metadata.clone(),
            self.snapshot_properties.clone(),
            self.added_data_files.clone(),
            self.added_delete_files.clone(),
        );

        snapshot_producer.validate_added_data_files(&self.added_data_files)?;
        snapshot_producer.validate_added_delete_files(&self.added_delete_files)?;

        if self.check_duplicate && !self.added_data_files.is_empty() {
            snapshot_producer
                .validate_duplicate_files(&self.added_data_files)
                .await?;
        }

        snapshot_producer
            .commit(EqualityDeltaOperation, DefaultManifestProcess)
            .await
    }
}

struct EqualityDeltaOperation;

impl SnapshotProduceOperation for EqualityDeltaOperation {
    fn operation(&self) -> Operation {
        Operation::Overwrite
    }

    async fn delete_entries(
        &self,
        _snapshot_produce: &SnapshotProducer<'_>,
    ) -> Result<Vec<crate::spec::ManifestEntry>> {
        Ok(vec![])
    }

    async fn existing_manifest(
        &self,
        snapshot_produce: &SnapshotProducer<'_>,
    ) -> Result<Vec<ManifestFile>> {
        let Some(snapshot) = snapshot_produce.table.metadata().current_snapshot() else {
            return Ok(vec![]);
        };

        let manifest_list = snapshot
            .load_manifest_list(
                snapshot_produce.table.file_io(),
                &snapshot_produce.table.metadata_ref(),
            )
            .await?;

        Ok(manifest_list.entries().iter().cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use crate::spec::{
        DataContentType, DataFileBuilder, DataFileFormat, Literal, Operation, Struct,
    };
    use crate::transaction::tests::make_v2_minimal_table;
    use crate::transaction::{Transaction, TransactionAction};
    use crate::{TableRequirement, TableUpdate};

    fn sample_data_file(path: &str) -> crate::spec::DataFile {
        let table = make_v2_minimal_table();
        DataFileBuilder::default()
            .content(DataContentType::Data)
            .file_path(path.to_string())
            .file_format(DataFileFormat::Parquet)
            .file_size_in_bytes(100)
            .record_count(1)
            .partition_spec_id(table.metadata().default_partition_spec_id())
            .partition(Struct::from_iter([Some(Literal::long(300))]))
            .build()
            .unwrap()
    }

    fn sample_delete_file(path: &str) -> crate::spec::DataFile {
        let table = make_v2_minimal_table();
        DataFileBuilder::default()
            .content(DataContentType::EqualityDeletes)
            .file_path(path.to_string())
            .file_format(DataFileFormat::Parquet)
            .file_size_in_bytes(50)
            .record_count(1)
            .partition_spec_id(table.metadata().default_partition_spec_id())
            .partition(Struct::from_iter([Some(Literal::long(300))]))
            .equality_ids(vec![1])
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn test_empty_equality_delta_action_fails() {
        let table = make_v2_minimal_table();
        let tx = Transaction::new(&table);
        let action = tx.equality_delta_append();
        assert!(Arc::new(action).commit(&table).await.is_err());
    }

    #[tokio::test]
    async fn test_equality_delta_commit_writes_delete_and_data_manifests() {
        let table = make_v2_minimal_table();
        let tx = Transaction::new(&table);
        let action = tx
            .equality_delta_append()
            .add_delete_files(vec![sample_delete_file("test/delete-1.parquet")])
            .add_data_files(vec![sample_data_file("test/data-1.parquet")]);
        let mut action_commit = Arc::new(action).commit(&table).await.unwrap();
        let updates = action_commit.take_updates();
        let new_snapshot = if let TableUpdate::AddSnapshot { snapshot } = &updates[0] {
            snapshot
        } else {
            unreachable!()
        };
        assert_eq!(new_snapshot.summary().operation, Operation::Overwrite);
        assert_eq!(
            new_snapshot
                .summary()
                .additional_properties
                .get("added-delete-files")
                .map(String::as_str),
            Some("1")
        );
        assert_eq!(
            new_snapshot
                .summary()
                .additional_properties
                .get("added-data-files")
                .map(String::as_str),
            Some("1")
        );
    }

    #[tokio::test]
    async fn test_equality_delta_snapshot_properties_flow_to_summary() {
        let table = make_v2_minimal_table();
        let tx = Transaction::new(&table);
        let mut snapshot_properties = HashMap::new();
        snapshot_properties.insert("skippr-policy".to_string(), "replace-partition".to_string());
        let action = tx
            .equality_delta_append()
            .set_snapshot_properties(snapshot_properties)
            .add_delete_files(vec![sample_delete_file("test/delete-2.parquet")])
            .add_data_files(vec![sample_data_file("test/data-2.parquet")]);
        let mut action_commit = Arc::new(action).commit(&table).await.unwrap();
        let updates = action_commit.take_updates();
        let new_snapshot = if let TableUpdate::AddSnapshot { snapshot } = &updates[0] {
            snapshot
        } else {
            unreachable!()
        };
        assert_eq!(
            new_snapshot
                .summary()
                .additional_properties
                .get("skippr-policy")
                .map(String::as_str),
            Some("replace-partition")
        );
        let requirements = action_commit.take_requirements();
        assert_eq!(requirements.len(), 2);
        assert!(matches!(
            requirements[1],
            TableRequirement::RefSnapshotIdMatch { .. }
        ));
    }
}
