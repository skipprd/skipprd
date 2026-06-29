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

use std::sync::Arc;

use async_trait::async_trait;

use crate::spec::Schema;
use crate::table::Table;
use crate::transaction::action::{ActionCommit, TransactionAction};
use crate::{Result, TableRequirement, TableUpdate};

/// A transactional action that adds a replacement schema and makes it current.
pub struct ReplaceSchemaAction {
    schema: Schema,
}

/// A transactional action that removes all non-current schema history.
pub struct RemoveOldSchemasAction;

impl ReplaceSchemaAction {
    pub fn new(schema: Schema) -> Self {
        Self { schema }
    }
}

impl RemoveOldSchemasAction {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl TransactionAction for ReplaceSchemaAction {
    async fn commit(self: Arc<Self>, table: &Table) -> Result<ActionCommit> {
        let updates = vec![
            TableUpdate::AddSchema {
                schema: self.schema.clone(),
            },
            TableUpdate::SetCurrentSchema { schema_id: -1 },
        ];
        let requirements = vec![TableRequirement::CurrentSchemaIdMatch {
            current_schema_id: table.metadata().current_schema_id(),
        }];

        Ok(ActionCommit::new(updates, requirements))
    }
}

#[async_trait]
impl TransactionAction for RemoveOldSchemasAction {
    async fn commit(self: Arc<Self>, table: &Table) -> Result<ActionCommit> {
        let current_schema_id = table.metadata().current_schema_id();
        let schema_ids = table
            .metadata()
            .schemas_iter()
            .map(|schema| schema.schema_id())
            .filter(|schema_id| *schema_id != current_schema_id)
            .collect::<Vec<_>>();

        if schema_ids.is_empty() {
            return Ok(ActionCommit::new(Vec::new(), Vec::new()));
        }

        Ok(ActionCommit::new(
            vec![TableUpdate::RemoveSchemas { schema_ids }],
            Vec::new(),
        ))
    }
}
