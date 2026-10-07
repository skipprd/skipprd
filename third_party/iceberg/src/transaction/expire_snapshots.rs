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

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;

use crate::spec::MAIN_BRANCH;
use crate::table::Table;
use crate::transaction::action::{ActionCommit, TransactionAction};
use crate::{Error, ErrorKind, Result, TableRequirement, TableUpdate};

/// Removes the named snapshots from table metadata. The commit asserts the
/// `main` snapshot it was planned against, so a concurrent commit aborts it.
/// Files are not deleted here; the caller cleans up after a successful commit.
pub struct ExpireSnapshotsAction {
    snapshot_ids: Vec<i64>,
}

impl ExpireSnapshotsAction {
    pub(crate) fn new(snapshot_ids: Vec<i64>) -> Self {
        Self { snapshot_ids }
    }
}

#[async_trait]
impl TransactionAction for ExpireSnapshotsAction {
    async fn commit(self: Arc<Self>, table: &Table) -> Result<ActionCommit> {
        let metadata = table.metadata();
        let referenced: HashSet<i64> = metadata
            .referenced_snapshot_ids()
            .chain(metadata.current_snapshot_id())
            .collect();
        if let Some(id) = self.snapshot_ids.iter().find(|id| referenced.contains(id)) {
            return Err(Error::new(
                ErrorKind::PreconditionFailed,
                format!("cannot expire snapshot {id}: it is referenced by a branch or tag"),
            ));
        }
        let snapshot_ids: Vec<i64> = self
            .snapshot_ids
            .iter()
            .copied()
            .filter(|id| metadata.snapshot_by_id(*id).is_some())
            .collect();
        if snapshot_ids.is_empty() {
            return Ok(ActionCommit::new(vec![], vec![]));
        }
        Ok(ActionCommit::new(
            vec![TableUpdate::RemoveSnapshots { snapshot_ids }],
            vec![
                TableRequirement::UuidMatch {
                    uuid: metadata.uuid(),
                },
                TableRequirement::RefSnapshotIdMatch {
                    r#ref: MAIN_BRANCH.to_string(),
                    snapshot_id: metadata.current_snapshot_id(),
                },
            ],
        ))
    }
}
