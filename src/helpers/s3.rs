use aws_sdk_s3::{Client as S3Client, Error as S3Error};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::operation::get_object::GetObjectError;
use aws_sdk_s3::operation::delete_object::DeleteObjectError;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::OnceCell;
use crate::helpers::configuration::Config;

static S3_CLIENT: OnceCell<Arc<S3Client>> = OnceCell::const_new();

pub async fn get_s3_client() -> Arc<S3Client> {
    S3_CLIENT
        .get_or_init(|| async {
            let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest()).load().await;
            Arc::new(S3Client::new(&aws_config))
        })
        .await
        .clone()
}

fn get_bucket() -> String { Config::get_skippr_s3_bucket() }

pub async fn put_json(key: &str, value: &Value) -> Result<(), S3Error> {
    let client = get_s3_client().await;
    let bucket = get_bucket();
    let body = serde_json::to_vec(value).unwrap();
    client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(ByteStream::from(body))
        .send()
        .await?;
    Ok(())
}

pub async fn get_json(key: &str) -> Result<Value, SdkError<GetObjectError>> {
    let client = get_s3_client().await;
    let bucket = get_bucket();
    let resp = client.get_object().bucket(bucket).key(key).send().await?;
    let bytes = resp.body.collect().await.unwrap().into_bytes();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    Ok(value)
}

pub async fn delete_object(key: &str) -> Result<(), SdkError<DeleteObjectError>> {
    let client = get_s3_client().await;
    let bucket = get_bucket();
    client.delete_object().bucket(bucket).key(key).send().await?;
    Ok(())
}