use super::dataset_catalog::DatasetId;

/// Shared FQN parsing for warehouse providers.
///
/// Strips surrounding quotes, splits on `.`, and returns a `DatasetId`.
/// When fewer than 3 parts are present, `default_catalog` and `default_database`
/// fill in the missing segments.
pub fn parse_fqn_common(
    raw: &str,
    default_catalog: &str,
    default_database: Option<&str>,
) -> Result<DatasetId, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("dataset id is empty".to_string());
    }
    fn strip_quotes(s: &str) -> &str {
        s.trim_matches('"').trim_matches('`')
    }
    let parts: Vec<&str> = trimmed.split('.').map(strip_quotes).collect();
    match parts.len() {
        3 => Ok(DatasetId {
            catalog: parts[0].to_string(),
            database: parts[1].to_string(),
            table: parts[2].to_string(),
        }),
        2 => Ok(DatasetId {
            catalog: default_catalog.to_string(),
            database: parts[0].to_string(),
            table: parts[1].to_string(),
        }),
        1 => {
            let db = default_database.ok_or_else(|| {
                "dataset id requires at least <schema>.<table> (no default schema configured)"
                    .to_string()
            })?;
            Ok(DatasetId {
                catalog: default_catalog.to_string(),
                database: db.to_string(),
                table: parts[0].to_string(),
            })
        }
        _ => Err(
            "dataset id must be <catalog>.<database>.<table> (or <database>.<table>)".to_string(),
        ),
    }
}

/// Clamp a concurrency value to `[1, cap]`.
pub fn clamp_concurrency(n: usize, cap: usize) -> usize {
    n.max(1).min(cap)
}

/// Clamp a cache TTL in seconds to `[5, 3600]`.
pub fn clamp_cache_ttl_secs(n: u64) -> u64 {
    n.max(5).min(3600)
}

// TODO(item-53): Shared TTL cache wrapper (Instant + RwLock) used identically by
// Athena, BigQuery, and Postgres providers. Extract a generic `TtlCache<K, V>` to
// reduce boilerplate across all warehouse providers.

// TODO(item-73): Dialect-aware stats SQL builder. Athena, BigQuery, and Postgres each
// construct nearly identical stats queries with minor dialect differences (CASE vs IF,
// TRY_CAST vs SAFE_CAST, to_unixtime vs UNIX_SECONDS). A shared builder parameterised
// by dialect would eliminate ~100 lines of duplication.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_fqn_three_parts() {
        let id = parse_fqn_common("cat.db.tbl", "default", None).unwrap();
        assert_eq!(id.catalog, "cat");
        assert_eq!(id.database, "db");
        assert_eq!(id.table, "tbl");
    }

    #[test]
    fn parse_fqn_two_parts() {
        let id = parse_fqn_common("db.tbl", "default_cat", None).unwrap();
        assert_eq!(id.catalog, "default_cat");
        assert_eq!(id.database, "db");
        assert_eq!(id.table, "tbl");
    }

    #[test]
    fn parse_fqn_one_part_with_default() {
        let id = parse_fqn_common("tbl", "cat", Some("db")).unwrap();
        assert_eq!(id.catalog, "cat");
        assert_eq!(id.database, "db");
        assert_eq!(id.table, "tbl");
    }

    #[test]
    fn parse_fqn_one_part_no_default() {
        assert!(parse_fqn_common("tbl", "cat", None).is_err());
    }

    #[test]
    fn parse_fqn_strips_quotes() {
        let id = parse_fqn_common("\"db\".\"tbl\"", "cat", None).unwrap();
        assert_eq!(id.database, "db");
        assert_eq!(id.table, "tbl");
    }

    #[test]
    fn clamp_concurrency_values() {
        assert_eq!(clamp_concurrency(0, 20), 1);
        assert_eq!(clamp_concurrency(25, 20), 20);
        assert_eq!(clamp_concurrency(10, 20), 10);
    }
}
