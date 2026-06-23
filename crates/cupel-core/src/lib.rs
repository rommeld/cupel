//! Provider-neutral inference primitives for cupel.
//!
//! This module only registers protocol adapters. It should not load user config,
//! read environment variables, or execute tools. Those jobs belong to CLI
//! and runtime crates.
pub mod diagnostics;
pub mod error;
pub mod event_stream;
pub mod model;
pub mod provider;
pub mod types;
