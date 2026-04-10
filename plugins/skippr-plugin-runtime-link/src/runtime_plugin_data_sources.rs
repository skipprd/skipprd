//! Source connector implementations for standalone `skippr-plugin-data-source-*` binaries.

#[cfg(feature = "runtime-plugin-source-amqp")]
pub mod runtime_source_amqp {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/plugins/data_source/amqp.rs"
    ));
}

#[cfg(feature = "runtime-plugin-source-clickhouse")]
pub mod runtime_source_clickhouse {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/plugins/data_source/clickhouse.rs"
    ));
}

#[cfg(feature = "runtime-plugin-source-delta-lake")]
pub mod runtime_source_delta_lake {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/plugins/data_source/delta_lake.rs"
    ));
}

#[cfg(feature = "runtime-plugin-source-dynamodb")]
pub mod runtime_source_dynamodb {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/plugins/data_source/dynamodb.rs"
    ));
}

#[cfg(feature = "runtime-plugin-source-eventbridge")]
pub mod runtime_source_eventbridge {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/plugins/data_source/eventbridge.rs"
    ));
}

#[cfg(feature = "runtime-plugin-source-file")]
pub mod runtime_source_file {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/plugins/data_source/file.rs"
    ));
}

#[cfg(feature = "runtime-plugin-source-http-client")]
pub mod runtime_source_http_client {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/plugins/data_source/http_client.rs"
    ));
}

#[cfg(feature = "runtime-plugin-source-http-server")]
pub mod runtime_source_http_server {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/plugins/data_source/http_server.rs"
    ));
}

#[cfg(feature = "runtime-plugin-source-kafka")]
pub mod runtime_source_kafka {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/plugins/data_source/kafka.rs"
    ));
}

#[cfg(feature = "runtime-plugin-source-kinesis")]
pub mod runtime_source_kinesis {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/plugins/data_source/kinesis.rs"
    ));
}

#[cfg(feature = "runtime-plugin-source-mongodb")]
pub mod runtime_source_mongodb {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/plugins/data_source/mongodb.rs"
    ));
}

#[cfg(feature = "runtime-plugin-source-motherduck")]
pub mod runtime_source_motherduck {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/plugins/data_source/motherduck.rs"
    ));
}

#[cfg(feature = "runtime-plugin-source-mqtt")]
pub mod runtime_source_mqtt {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/plugins/data_source/mqtt.rs"
    ));
}

#[cfg(feature = "runtime-plugin-source-mssql")]
pub mod runtime_source_mssql {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/plugins/data_source/mssql.rs"
    ));
}

#[cfg(feature = "runtime-plugin-source-mysql")]
pub mod runtime_source_mysql {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/plugins/data_source/mysql.rs"
    ));
}

#[cfg(feature = "runtime-plugin-source-pcap")]
pub mod runtime_source_pcap {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/plugins/data_source/pcap.rs"
    ));
}

#[cfg(feature = "runtime-plugin-source-redshift")]
pub mod runtime_source_redshift {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/plugins/data_source/redshift.rs"
    ));
}

#[cfg(feature = "runtime-plugin-source-s3")]
pub mod runtime_source_s3 {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/plugins/data_source/s3.rs"
    ));
}

#[cfg(feature = "runtime-plugin-source-sftp")]
pub mod runtime_source_sftp {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/plugins/data_source/sftp.rs"
    ));
}

#[cfg(feature = "runtime-plugin-source-sns")]
pub mod runtime_source_sns {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/plugins/data_source/sns.rs"
    ));
}

#[cfg(feature = "runtime-plugin-source-sqs")]
pub mod runtime_source_sqs {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/plugins/data_source/sqs.rs"
    ));
}

#[cfg(feature = "runtime-plugin-source-statsd")]
pub mod runtime_source_statsd {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/plugins/data_source/statsd.rs"
    ));
}

#[cfg(feature = "runtime-plugin-source-stdin")]
pub mod runtime_source_stdin {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/plugins/data_source/stdin.rs"
    ));
}

#[cfg(feature = "runtime-plugin-source-socket")]
pub mod runtime_source_socket {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/plugins/data_source/socket.rs"
    ));
}

#[cfg(feature = "runtime-plugin-source-websocket")]
pub mod runtime_source_websocket {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/plugins/data_source/websocket.rs"
    ));
}
