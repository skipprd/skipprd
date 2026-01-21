//! Suites for the ReAct runtime.
//!
//! This crate is where all usecase/business logic lives: prompts, tool selection,
//! and suite orchestration. It depends on `react-core` (the generic runner).

pub mod registry;
pub mod suite;
pub mod flow_frame;
pub mod kb;
pub mod config;
pub mod data_engineer_shared;
pub mod data_engineer;
pub mod preflight;
pub mod dbt;
pub mod prompts;
pub mod util;

pub use registry::{SuiteRegistry, default_registry};
pub use suite::{Suite, SuiteCtx, DynSuite};
pub use flow_frame::FlowFrame;
pub use config::ReactResolvedConfig;

