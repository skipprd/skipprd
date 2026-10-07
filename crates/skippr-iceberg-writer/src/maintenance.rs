//! Bounded small-file and equality-delete maintenance. A pass runs under the
//! table lane after a grouped commit, reads at most [`PASS_MAX_INPUT_BYTES`] /
//! [`PASS_MAX_INPUT_FILES`] of data, and commits one rewrite that asserts the
//! snapshot it was planned from.

use std::collections::{BTreeMap, BTreeSet};

use iceberg::spec::{DataContentType, DataFile, Summary};

pub(crate) const SMALL_FILE_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const PARTITION_DATA_FILES_TRIGGER: usize = 32;
pub(crate) const PARTITION_DELETE_FILES_TRIGGER: usize = 16;
pub(crate) const PASS_MAX_INPUT_BYTES: u64 = 512 * 1024 * 1024;
pub(crate) const PASS_MAX_INPUT_FILES: usize = 64;

pub(crate) const SNAPSHOT_MAINTENANCE: &str = "skippr.maintenance";

const TOTAL_DATA_FILES: &str = "total-data-files";
const TOTAL_DELETE_FILES: &str = "total-delete-files";
const TOTAL_FILES_SIZE: &str = "total-files-size";

/// File counts from a snapshot summary, used to skip planning cheaply.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TableTotals {
    pub data_files: u64,
    pub delete_files: u64,
    pub bytes: u64,
}

impl TableTotals {
    pub fn of(summary: &Summary) -> Self {
        let count = |key: &str| {
            summary
                .additional_properties
                .get(key)
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0)
        };
        Self {
            data_files: count(TOTAL_DATA_FILES),
            delete_files: count(TOTAL_DELETE_FILES),
            bytes: count(TOTAL_FILES_SIZE),
        }
    }

    /// Whether some partition could cross a trigger. `last_idle` is the totals
    /// of the last plan that found nothing, so a table whose files spread over
    /// many partitions is re-planned only after enough new files arrive.
    pub fn worth_planning(&self, last_idle: Option<TableTotals>) -> bool {
        let files = self.data_files + self.delete_files;
        let small = self.data_files > PARTITION_DATA_FILES_TRIGGER as u64
            && self.bytes / files.max(1) < SMALL_FILE_BYTES;
        let deletes = self.delete_files > PARTITION_DELETE_FILES_TRIGGER as u64;
        if !(small || deletes) {
            return false;
        }
        last_idle.is_none_or(|idle| {
            self.data_files >= idle.data_files + PARTITION_DATA_FILES_TRIGGER as u64
                || self.delete_files >= idle.delete_files + PARTITION_DELETE_FILES_TRIGGER as u64
        })
    }
}

/// One live data or delete file with the partition key it was planned under.
#[derive(Clone, Debug)]
pub(crate) struct LiveFile {
    pub partition_key: String,
    pub file: DataFile,
}

#[derive(Debug, Default)]
pub(crate) struct PartitionRewrite {
    pub inputs: Vec<DataFile>,
    pub removed_deletes: Vec<DataFile>,
}

#[derive(Debug, Default)]
pub(crate) struct MaintenancePlan {
    pub partitions: Vec<PartitionRewrite>,
    /// Partitions past the delete trigger that are too large to rewrite whole.
    pub too_large: Vec<String>,
}

impl MaintenancePlan {
    pub fn is_empty(&self) -> bool {
        self.partitions.is_empty()
    }

    /// Stable across retries of the same plan; names its outputs and receipt.
    pub fn fingerprint(&self, starting_snapshot_id: i64) -> String {
        let mut paths = BTreeSet::new();
        for partition in &self.partitions {
            for file in partition.inputs.iter().chain(&partition.removed_deletes) {
                paths.insert(file.file_path());
            }
        }
        let mut digest = md5::Context::new();
        digest.consume(starting_snapshot_id.to_be_bytes());
        for path in paths {
            digest.consume((path.len() as u64).to_be_bytes());
            digest.consume(path.as_bytes());
        }
        format!("{:x}", digest.compute())
    }
}

/// Plans one bounded pass over the live files of the default partition spec.
pub(crate) fn plan(files: Vec<LiveFile>) -> MaintenancePlan {
    let mut partitions: BTreeMap<String, (Vec<DataFile>, Vec<DataFile>)> = BTreeMap::new();
    for live in files {
        let entry = partitions.entry(live.partition_key).or_default();
        if live.file.content_type() == DataContentType::Data {
            entry.0.push(live.file);
        } else {
            entry.1.push(live.file);
        }
    }
    let mut plan = MaintenancePlan::default();
    let mut budget_bytes = PASS_MAX_INPUT_BYTES;
    let mut budget_files = PASS_MAX_INPUT_FILES;
    for (key, (mut data, deletes)) in partitions {
        if budget_files < 2 {
            break;
        }
        data.sort_by(|left, right| left.file_path().cmp(right.file_path()));
        let data_bytes: u64 = data.iter().map(DataFile::file_size_in_bytes).sum();
        if deletes.len() > PARTITION_DELETE_FILES_TRIGGER {
            if data_bytes <= budget_bytes && data.len() <= budget_files {
                budget_bytes -= data_bytes;
                budget_files -= data.len();
                plan.partitions.push(PartitionRewrite {
                    inputs: data,
                    removed_deletes: deletes,
                });
                continue;
            }
            plan.too_large.push(key);
        }
        let small_partition = data.len() > PARTITION_DATA_FILES_TRIGGER
            && data_bytes / (data.len() as u64) < SMALL_FILE_BYTES;
        if !small_partition {
            continue;
        }
        let mut inputs = Vec::new();
        for file in data {
            let bytes = file.file_size_in_bytes();
            if bytes >= SMALL_FILE_BYTES {
                continue;
            }
            if inputs.len() == budget_files || bytes > budget_bytes {
                break;
            }
            budget_bytes -= bytes;
            inputs.push(file);
        }
        if inputs.len() < 2 {
            budget_bytes += inputs.iter().map(DataFile::file_size_in_bytes).sum::<u64>();
            continue;
        }
        budget_files -= inputs.len();
        plan.partitions.push(PartitionRewrite {
            inputs,
            removed_deletes: Vec::new(),
        });
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use iceberg::spec::{DataFileBuilder, DataFileFormat, Struct};

    fn file(path: &str, content: DataContentType, bytes: u64) -> DataFile {
        let mut builder = DataFileBuilder::default();
        builder
            .content(content)
            .file_path(path.to_string())
            .file_format(DataFileFormat::Parquet)
            .file_size_in_bytes(bytes)
            .record_count(1)
            .partition_spec_id(0)
            .partition(Struct::empty());
        if content == DataContentType::EqualityDeletes {
            builder.equality_ids(Some(vec![1]));
        }
        builder.build().unwrap()
    }

    fn live(partition: &str, path: &str, content: DataContentType, bytes: u64) -> LiveFile {
        LiveFile {
            partition_key: partition.to_string(),
            file: file(path, content, bytes),
        }
    }

    fn data_files(partition: &str, count: usize, bytes: u64) -> Vec<LiveFile> {
        (0..count)
            .map(|i| {
                live(
                    partition,
                    &format!("{partition}/d{i:04}"),
                    DataContentType::Data,
                    bytes,
                )
            })
            .collect()
    }

    fn delete_files(partition: &str, count: usize) -> Vec<LiveFile> {
        (0..count)
            .map(|i| {
                live(
                    partition,
                    &format!("{partition}/x{i:04}"),
                    DataContentType::EqualityDeletes,
                    100,
                )
            })
            .collect()
    }

    #[test]
    fn below_the_triggers_nothing_is_planned() {
        let mut files = data_files("a", PARTITION_DATA_FILES_TRIGGER, 1024);
        files.extend(delete_files("a", PARTITION_DELETE_FILES_TRIGGER));
        assert!(plan(files).is_empty());
    }

    #[test]
    fn small_files_bin_pack_without_touching_deletes() {
        let mut files = data_files("a", 40, 1024);
        files.extend(data_files("a-large", 40, SMALL_FILE_BYTES));
        files.extend(delete_files("a", 3));
        let plan = plan(files);
        assert_eq!(plan.partitions.len(), 1);
        assert_eq!(plan.partitions[0].inputs.len(), 40);
        assert!(plan.partitions[0].removed_deletes.is_empty());
    }

    #[test]
    fn many_deletes_rewrite_the_whole_partition_and_drop_its_deletes() {
        let mut files = data_files("a", 3, 2 * SMALL_FILE_BYTES);
        files.extend(delete_files("a", PARTITION_DELETE_FILES_TRIGGER + 1));
        let plan = plan(files);
        assert_eq!(plan.partitions.len(), 1);
        assert_eq!(plan.partitions[0].inputs.len(), 3);
        assert_eq!(
            plan.partitions[0].removed_deletes.len(),
            PARTITION_DELETE_FILES_TRIGGER + 1
        );
    }

    #[test]
    fn a_partition_too_large_to_rewrite_whole_only_bin_packs() {
        let mut files = data_files("a", 40, 1024);
        files.extend(
            data_files("a-big", 2, 300 * 1024 * 1024)
                .into_iter()
                .map(|mut live| {
                    live.partition_key = "a".to_string();
                    live
                }),
        );
        files.extend(delete_files("a", PARTITION_DELETE_FILES_TRIGGER + 1));
        let plan = plan(files);
        assert_eq!(plan.too_large, vec!["a".to_string()]);
        assert_eq!(plan.partitions.len(), 1);
        assert_eq!(plan.partitions[0].inputs.len(), 40);
        assert!(plan.partitions[0].removed_deletes.is_empty());
    }

    #[test]
    fn a_pass_respects_the_file_and_byte_ceilings() {
        let mut files = data_files("a", 200, 1024);
        files.extend(data_files("b", 200, 1024));
        let by_files = plan(files);
        let inputs: usize = by_files.partitions.iter().map(|p| p.inputs.len()).sum();
        assert_eq!(inputs, PASS_MAX_INPUT_FILES);

        let bytes = 15 * 1024 * 1024;
        let by_bytes = plan(data_files("c", 60, bytes));
        let total: u64 = by_bytes.partitions[0]
            .inputs
            .iter()
            .map(DataFile::file_size_in_bytes)
            .sum();
        assert!(total <= PASS_MAX_INPUT_BYTES);
        assert_eq!(
            by_bytes.partitions[0].inputs.len() as u64,
            PASS_MAX_INPUT_BYTES / bytes
        );
    }

    #[test]
    fn fingerprint_names_the_input_set_and_starting_snapshot() {
        let files = || data_files("a", 40, 1024);
        let one = plan(files());
        let mut reversed = files();
        reversed.reverse();
        assert_eq!(one.fingerprint(7), plan(reversed).fingerprint(7));
        assert_ne!(one.fingerprint(7), one.fingerprint(8));
        assert_ne!(
            one.fingerprint(7),
            plan(data_files("a", 41, 1024)).fingerprint(7)
        );
    }

    #[test]
    fn idle_totals_defer_replanning_until_enough_new_files() {
        let totals = TableTotals {
            data_files: 100,
            delete_files: 0,
            bytes: 100 * 1024,
        };
        assert!(totals.worth_planning(None));
        assert!(!totals.worth_planning(Some(totals)));
        let grown = TableTotals {
            data_files: 100 + PARTITION_DATA_FILES_TRIGGER as u64,
            ..totals
        };
        assert!(grown.worth_planning(Some(totals)));
        let large = TableTotals {
            bytes: 100 * SMALL_FILE_BYTES,
            ..totals
        };
        assert!(!large.worth_planning(None));
    }
}
