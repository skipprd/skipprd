//! Dedicated async DynamoDB client for leases and membership.
//!
//! This crate owns a separate Tokio/AWS client from the blocking offset worker so
//! bulk offset publication cannot starve lease renewal.

mod lease;
mod membership;

pub use lease::DynamoDbLeaseStore;
pub use membership::DynamoDbMembershipStore;
pub use skippr_lease::{MembershipRecord, NodeAd};

pub(crate) fn is_conditional_check_failed(
    err: &impl aws_sdk_dynamodb::error::ProvideErrorMetadata,
) -> bool {
    err.code() == Some("ConditionalCheckFailedException")
}

pub(crate) async fn load_sdk_config() -> aws_config::SdkConfig {
    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
    if let Ok(url) = std::env::var("AWS_ENDPOINT_URL_DYNAMODB") {
        if !url.trim().is_empty() {
            loader = loader.endpoint_url(url);
        }
    }
    loader.load().await
}
