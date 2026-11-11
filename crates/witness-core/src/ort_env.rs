//! The process's one ONNX Runtime environment.
//!
//! Since ort 2.0.0-rc.14 every session is built in an explicit
//! [`Environment`], and ort allows only one per process: once one has been
//! built, `ort::init().build()` fails for good, even after it is dropped.
//! Parakeet, the diarizer and the speaker embedder therefore all share this
//! one, created on first use and kept until exit.

use anyhow::{Context, Result};
use ort::environment::Environment;
use std::sync::Mutex;

static ENV: Mutex<Option<Environment>> = Mutex::new(None);

/// The shared environment, created on first call. A failed creation is
/// retried by the next caller.
pub fn environment() -> Result<Environment> {
    let mut env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(env) = env.as_ref() {
        return Ok(env.clone());
    }
    let created = ort::init()
        .with_name("witness")
        // ORT telemetry is ETW events on Windows; nothing about our models
        // or meetings needs to leave the process.
        .with_telemetry(false)
        .build()
        .context("creating the ONNX Runtime environment")?;
    *env = Some(created.clone());
    Ok(created)
}

#[cfg(test)]
mod tests {
    #[test]
    fn environment_is_shared() {
        // A second build would fail ("only one environment is allowed per
        // process"), so repeated calls must hand out the same one.
        let a = super::environment().unwrap();
        let b = super::environment().unwrap();
        drop(a);
        drop(b);
        super::environment().unwrap();
    }
}
