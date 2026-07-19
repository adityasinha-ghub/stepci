//! `stepci` — a native, Dockerless debugger for GitHub Actions workflows.
//!
//! The pipeline is: parse a workflow YAML into a validated [`model::Workflow`] →
//! (next milestones) evaluate `${{ }}` expressions → run each step natively →
//! diff what the step changed → drive it all from an interactive debugger loop.
//!
//! This crate is deliberately split into small, single-responsibility modules so
//! the fiddly pure logic (parsing, expression evaluation) can be unit-tested in
//! isolation.

pub mod diff;
pub mod envfile;
pub mod exec;
pub mod expr;
pub mod fetch;
pub mod model;
pub mod parse;
pub mod secrets;
pub mod value;
pub mod wfcmd;
