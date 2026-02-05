//! Suites for the ReAct runtime.
//!
//! This crate is where all usecase/business logic lives: prompts, tool selection,
//! and suite orchestration. It depends on `react-core` (the generic runner).

pub mod config;
pub mod data_engineer;
pub mod data_engineer_shared;
pub mod dbt;
pub mod flow_frame;
pub mod kb;
pub mod preflight;
pub mod prompts;
pub mod registry;
pub mod suite;
pub mod util;

pub use config::ReactResolvedConfig;
pub use flow_frame::FlowFrame;
pub use registry::{default_registry, SuiteRegistry};
pub use suite::{DynSuite, Suite, SuiteCtx};
