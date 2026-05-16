//! `crate-keeper` template — read-only audit of a Rust workspace.
//!
//! Designed for the `examples/crate-keeper` binary but exposed here so other
//! callers can reuse it.

use harness_core::{Sensor, Tool};
use harness_sensors_rust::CargoCheck;
use harness_tools_fs::{ListDir, ReadFile, WriteFile};
use harness_tools_shell::ShellRead;
use std::sync::Arc;

pub fn tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(ListDir), Arc::new(ReadFile), Arc::new(WriteFile), Arc::new(ShellRead)]
}

pub fn sensors() -> Vec<Arc<dyn Sensor>> {
    vec![Arc::new(CargoCheck::new())]
}
