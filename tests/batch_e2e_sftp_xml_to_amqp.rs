mod support;

use std::fs;
use std::time::Duration;

use support::batch_e2e::{
    assert_success, collect_amqp_messages, fixture_path, prepare_amqp_queue, sftp_write_file,
    BatchE2eHarness, AMQP_CONNECTION_STRING, SFTP_PORT,
};

#[ignore = "requires SFTP and RabbitMQ docker services"]
#[tokio::test]
async fn sftp_xml_to_amqp_publishes_two_messages() {
    let pipeline_name = "batch_sftp_xml_to_amqp";
    let suffix = rand::random::<u32>();
    let exchange = format!("batch_xml_exchange_{}", suffix);
    let routing_key = format!("batch.xml.{}", suffix);
    let queue = format!("batch_xml_queue_{}", suffix);
    let remote_path = format!("/upload/batch_catalog_{}.xml", suffix);
    let input = fs::read_to_string(fixture_path("catalog.xml")).unwrap();

    prepare_amqp_queue(AMQP_CONNECTION_STRING, &exchange, &routing_key, &queue).await;
    sftp_write_file(
        "127.0.0.1",
        SFTP_PORT,
        "testuser",
        "testpass",
        &remote_path,
        &input,
    );

    let config = format!(
        r#"skippr:
  workspace: batch-tests
  storage_mode: local

data_sources:
  sftp_source:
    Sftp:
      host: "127.0.0.1"
      port: {sftp_port}
      username: testuser
      password: testpass
      remote_path: "{remote_path}"
      format: xml

data_sinks:
  amqp_sink:
    Amqp:
      connection_string: "{amqp_connection_string}"
      exchange: "{exchange}"
      routing_key: "{routing_key}"
      exchange_type: direct

pipelines:
  {pipeline_name}:
    data_source: data_sources.sftp_source
    data_sink: data_sinks.amqp_sink
"#,
        sftp_port = SFTP_PORT,
        remote_path = remote_path,
        amqp_connection_string = AMQP_CONNECTION_STRING,
        exchange = exchange,
        routing_key = routing_key,
        pipeline_name = pipeline_name,
    );

    let harness = BatchE2eHarness::new(pipeline_name, &config);
    let output = harness.run_sync(Duration::from_secs(20));
    assert_success(&output);

    let messages =
        collect_amqp_messages(AMQP_CONNECTION_STRING, &queue, 2, Duration::from_secs(20)).await;
    let names = messages
        .iter()
        .map(|message| serde_json::from_str::<serde_json::Value>(message).unwrap())
        .map(|payload| payload["name"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();

    assert!(names.iter().any(|name| name == "Ada Lovelace"));
    assert!(names.iter().any(|name| name == "Grace Hopper"));
}
