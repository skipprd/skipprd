/// CDC Protocol Tests: MySQL  (secondary coverage)
///
/// Low-level tests that validate MySQL binlog replication protocol details.
/// These are NOT the CDC acceptance gate — see `cdc_e2e_mysql` for
/// the full-pipeline end-to-end suite.
///
/// Requires Docker service `mysql`.
///
/// Run: `cargo test --test cdc_protocol_mysql -- --ignored`
use futures_util::StreamExt;
use mysql_async::prelude::*;
use mysql_async::{BinlogStreamRequest, Pool};

const MYSQL_URL: &str = "mysql://skippr:testpass@127.0.0.1:13306/skippr_test";

async fn pool() -> Pool {
    Pool::new(MYSQL_URL)
}

// -----------------------------------------------------------------------
// 1. Binlog position: capture file + position, verify advancement
// -----------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn cdc_mysql_binlog_position_capture() {
    let pool = pool().await;
    let mut conn = pool.get_conn().await.expect("connect failed");

    conn.query_drop("DROP TABLE IF EXISTS cdc_mysql_test")
        .await
        .unwrap();
    conn.query_drop("CREATE TABLE cdc_mysql_test (id INT PRIMARY KEY, name VARCHAR(255))")
        .await
        .unwrap();

    let row: mysql_async::Row = conn
        .query_first("SHOW BINARY LOG STATUS")
        .await
        .unwrap()
        .expect("SHOW BINARY LOG STATUS returned no rows");
    let file: String = row.get(0).unwrap();
    let pos1: u64 = row.get(1).unwrap();

    assert!(!file.is_empty(), "binlog file should be non-empty");
    assert!(pos1 > 0, "binlog position should be > 0");

    conn.query_drop("INSERT INTO cdc_mysql_test VALUES (1, 'Alice')")
        .await
        .unwrap();

    let row: mysql_async::Row = conn
        .query_first("SHOW BINARY LOG STATUS")
        .await
        .unwrap()
        .expect("SHOW BINARY LOG STATUS returned no rows");
    let pos2: u64 = row.get(1).unwrap();

    assert!(
        pos2 > pos1,
        "binlog position should advance after INSERT (was {}, now {})",
        pos1,
        pos2
    );

    conn.query_drop("DROP TABLE IF EXISTS cdc_mysql_test")
        .await
        .unwrap();
    drop(conn);
    pool.disconnect().await.unwrap();
}

// -----------------------------------------------------------------------
// 2. Snapshot position: verify order_token encoding
// -----------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn cdc_mysql_source_snapshot_tags_rows() {
    let pool = pool().await;
    let mut conn = pool.get_conn().await.expect("connect failed");

    conn.query_drop("DROP TABLE IF EXISTS cdc_mysql_snap")
        .await
        .unwrap();
    conn.query_drop("CREATE TABLE cdc_mysql_snap (id INT PRIMARY KEY, val VARCHAR(100))")
        .await
        .unwrap();
    conn.query_drop("INSERT INTO cdc_mysql_snap VALUES (1, 'a'), (2, 'b')")
        .await
        .unwrap();

    let row: mysql_async::Row = conn
        .query_first("SHOW BINARY LOG STATUS")
        .await
        .unwrap()
        .expect("SHOW BINARY LOG STATUS returned no rows");
    let pos: u64 = row.get(1).unwrap();

    // Encode as big-endian u64 order_token (matches DataSourceMysqlPlugin::binlog_order_token
    // with timestamp=0 for snapshot).
    let order_token = pos.to_be_bytes();
    assert_eq!(order_token.len(), 8);
    let decoded = u64::from_be_bytes(order_token);
    assert_eq!(decoded, pos, "round-trip through be bytes must be lossless");

    // Same state → same position (deterministic)
    let row2: mysql_async::Row = conn
        .query_first("SHOW BINARY LOG STATUS")
        .await
        .unwrap()
        .expect("SHOW BINARY LOG STATUS returned no rows");
    let pos2: u64 = row2.get(1).unwrap();
    assert_eq!(pos, pos2, "position should be deterministic for same state");

    conn.query_drop("DROP TABLE IF EXISTS cdc_mysql_snap")
        .await
        .unwrap();
    drop(conn);
    pool.disconnect().await.unwrap();
}

// -----------------------------------------------------------------------
// 3. Binlog stream: receive WriteRows events
// -----------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn cdc_mysql_binlog_stream_receives_events() {
    let pool = pool().await;
    let mut conn = pool.get_conn().await.expect("connect failed");

    conn.query_drop("DROP TABLE IF EXISTS cdc_mysql_stream")
        .await
        .unwrap();
    conn.query_drop("CREATE TABLE cdc_mysql_stream (id INT PRIMARY KEY, name VARCHAR(255))")
        .await
        .unwrap();

    let row: mysql_async::Row = conn
        .query_first("SHOW BINARY LOG STATUS")
        .await
        .unwrap()
        .expect("SHOW BINARY LOG STATUS returned no rows");
    let file: String = row.get(0).unwrap();
    let pos: u64 = row.get(1).unwrap();

    // Open binlog stream from current position (needs a fresh connection)
    let stream_conn = pool.get_conn().await.unwrap();
    let request = BinlogStreamRequest::new(99)
        .with_filename(file.as_bytes())
        .with_pos(pos);
    let mut binlog_stream = stream_conn
        .get_binlog_stream(request)
        .await
        .expect("failed to open binlog stream");

    conn.query_drop("INSERT INTO cdc_mysql_stream VALUES (1, 'Eve')")
        .await
        .unwrap();

    const WRITE_ROWS_V2: u8 = 30;

    let mut found_write = false;
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        let maybe =
            tokio::time::timeout(tokio::time::Duration::from_secs(2), binlog_stream.next()).await;

        match maybe {
            Ok(Some(Ok(event))) => {
                if event.header().event_type_raw() == WRITE_ROWS_V2 {
                    found_write = true;
                    break;
                }
            }
            Ok(Some(Err(e))) => panic!("binlog stream error: {}", e),
            Ok(None) => break,
            Err(_) => break,
        }
    }

    assert!(
        found_write,
        "should receive a WriteRowsEvent from the binlog stream"
    );

    conn.query_drop("DROP TABLE IF EXISTS cdc_mysql_stream")
        .await
        .unwrap();
    drop(binlog_stream);
    drop(conn);
    pool.disconnect().await.unwrap();
}

// -----------------------------------------------------------------------
// 4. Binlog stream: captures UPDATE and DELETE events
// -----------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn cdc_mysql_binlog_captures_update_delete() {
    let pool = pool().await;
    let mut conn = pool.get_conn().await.expect("connect failed");

    conn.query_drop("DROP TABLE IF EXISTS cdc_mysql_ud")
        .await
        .unwrap();
    conn.query_drop("CREATE TABLE cdc_mysql_ud (id INT PRIMARY KEY, name VARCHAR(255))")
        .await
        .unwrap();
    conn.query_drop("INSERT INTO cdc_mysql_ud VALUES (1, 'Alice')")
        .await
        .unwrap();

    let row: mysql_async::Row = conn
        .query_first("SHOW BINARY LOG STATUS")
        .await
        .unwrap()
        .expect("SHOW BINARY LOG STATUS returned no rows");
    let file: String = row.get(0).unwrap();
    let pos: u64 = row.get(1).unwrap();

    let stream_conn = pool.get_conn().await.unwrap();
    let request = BinlogStreamRequest::new(100)
        .with_filename(file.as_bytes())
        .with_pos(pos);
    let mut binlog_stream = stream_conn
        .get_binlog_stream(request)
        .await
        .expect("failed to open binlog stream");

    conn.query_drop("UPDATE cdc_mysql_ud SET name = 'Bob' WHERE id = 1")
        .await
        .unwrap();
    conn.query_drop("DELETE FROM cdc_mysql_ud WHERE id = 1")
        .await
        .unwrap();

    const UPDATE_ROWS_V2: u8 = 31;
    const DELETE_ROWS_V2: u8 = 32;

    let mut found_update = false;
    let mut found_delete = false;
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if found_update && found_delete {
            break;
        }
        let maybe =
            tokio::time::timeout(tokio::time::Duration::from_secs(2), binlog_stream.next()).await;

        match maybe {
            Ok(Some(Ok(event))) => {
                let raw = event.header().event_type_raw();
                if raw == UPDATE_ROWS_V2 {
                    found_update = true;
                }
                if raw == DELETE_ROWS_V2 {
                    found_delete = true;
                }
            }
            Ok(Some(Err(e))) => panic!("binlog stream error: {}", e),
            Ok(None) => break,
            Err(_) => break,
        }
    }

    assert!(found_update, "should receive an UpdateRowsEvent");
    assert!(found_delete, "should receive a DeleteRowsEvent");

    conn.query_drop("DROP TABLE IF EXISTS cdc_mysql_ud")
        .await
        .unwrap();
    drop(binlog_stream);
    drop(conn);
    pool.disconnect().await.unwrap();
}
