//! Shared helpers for API/SaaS runtime source plugins.

pub mod auth;
pub mod checkpoint;
pub mod date_window;
pub mod json_extract;
pub mod openai;
pub mod pagination;
pub mod retry;

pub use auth::{
    AppleAdsClientCredentialsAuth, BearerAuth, OAuth2RefreshTokenAuth, ServiceAccountAuth,
    StaticBearerAuth,
};
pub use checkpoint::{CheckpointPayload, JsonCheckpoint};
pub use date_window::{DateWindow, DateWindowPlanner};
pub use json_extract::json_rows_from_response;
pub use openai::{OpenAiChatClient, OpenAiError};
pub use pagination::{OffsetPagination, PageNumberPagination, TokenPagination};
pub use retry::{RetryConfig, RetryDecision, RetryableHttpClient};
