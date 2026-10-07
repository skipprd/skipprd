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
use crate::spec::{DataContentType, DataFile, ManifestFile, Operation};
use crate::table::Table;
use crate::transaction::snapshot::{SnapshotProduceOperation, SnapshotProducer};
use crate::transaction::{ActionCommit, TransactionAction};

/// Replaces live files with rewritten data files in one snapshot, asserting the
/// `main` snapshot the rewrite was planned from.
///
/// The added files get this snapshot's sequence number, so equality deletes
/// older than it no longer apply to them: callers MUST have applied every live
/// delete while rewriting. Removing delete files is `Overwrite`; a data-only
/// rewrite is `Replace`.
pub struct RewriteFilesAction {
    commit_uuid: Option<Uuid>,
    snapshot_properties: HashMap<String, String>,
    added_data_files: Vec<DataFile>,
    removed_files: Vec<DataFile>,
}

impl RewriteFilesAction {
    pub(crate) fn new() -> Self {
        Self {
            commit_uuid: None,
            snapshot_properties: HashMap::default(),
            added_data_files: vec![],
            removed_files: vec![],
        }
    }

    /// Add rewritten data files.
    pub fn add_data_files(mut self, data_files: impl IntoIterator<Item = DataFile>) -> Self {
        self.added_data_files.extend(data_files);
        self
    }

    /// Remove live data or delete files.
    pub fn remove_files(mut self, files: impl IntoIterator<Item = DataFile>) -> Self {
        self.removed_files.extend(files);
        self
    }

    /// Set commit UUID for the snapshot; it names the manifests this commit writes.
    pub fn set_commit_uuid(mut self, commit_uuid: Uuid) -> Self {
        self.commit_uuid = Some(commit_uuid);
        self
    }

    /// Set snapshot summary properties.
    pub fn set_snapshot_properties(mut self, snapshot_properties: HashMap<String, String>) -> Self {
        self.snapshot_properties = snapshot_properties;
        self
    }
}

#[async_trait]
impl TransactionAction for RewriteFilesAction {
    async fn commit(self: Arc<Self>, table: &Table) -> Result<ActionCommit> {
        if self.removed_files.is_empty() {
            return Err(crate::Error::new(
                crate::ErrorKind::PreconditionFailed,
                "Rewrite commit requires at least one removed file",
            ));
        }
        let operation = if self
            .removed_files
            .iter()
            .any(|file| file.content_type() != DataContentType::Data)
        {
            Operation::Overwrite
        } else {
            Operation::Replace
        };
        let snapshot_producer = SnapshotProducer::new(
            table,
            self.commit_uuid.unwrap_or_else(Uuid::now_v7),
            None,
            self.snapshot_properties.clone(),
            self.added_data_files.clone(),
        )
        .with_removed_files(self.removed_files.clone());
        if !self.added_data_files.is_empty() {
            snapshot_producer.validate_added_data_files()?;
            snapshot_producer.validate_duplicate_files().await?;
        }
        snapshot_producer
            .commit(RewriteOperation { operation })
            .await
    }
}

struct RewriteOperation {
    operation: Operation,
}

impl SnapshotProduceOperation for RewriteOperation {
    fn operation(&self) -> Operation {
        self.operation.clone()
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
        Ok(manifest_list.entries().to_vec())
    }
}
