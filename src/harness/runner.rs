//! Thin executable orchestration. Configuration checks perform no dispatch;
//! once the driver starts, its terminal report is preserved even on failure.

use std::{
    fmt,
    io::Write,
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
};

use super::{
    async_driver,
    command::{CommandClient, CommandConfig},
    inference::{InferenceClient, InferenceConfig, InferenceError},
    report,
    types::CommandClientError,
};
use crate::manifest::{Manifest, ManifestError};

/// Owns a current-thread runtime; call outside any existing Tokio runtime.
/// Returns true only for submission with a successfully written/flushed report.
/// Cancellation or process death can lose the report and cannot stop a remote
/// command. Never restart the same trial automatically.
pub fn run(path: &Path, mut output: impl Write) -> Result<bool, RunError> {
    guarded(|| {
        let manifest = Manifest::from_path(path).map_err(RunError::Manifest)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(RunError::Runtime)?;
        let result = runtime.block_on(async {
            let inference = manifest.inference();
            let limits = inference.transport();
            let mut model = InferenceClient::new(InferenceConfig {
                completion_url: inference.completion_url().to_string(),
                model: inference.model().to_owned(),
                max_tokens: inference.max_tokens().get(),
                connect_timeout: limits.connect_timeout(),
                request_timeout: limits.request_timeout(),
                max_request_bytes: limits.max_request_bytes().get(),
                max_response_bytes: limits.max_response_bytes().get(),
            })
            .map_err(RunError::Inference)?;
            let command = manifest.command();
            let limits = command.transport();
            let mut executor = CommandClient::new(CommandConfig {
                command_url: command.command_url().to_string(),
                connect_timeout: limits.connect_timeout(),
                request_timeout: limits.request_timeout(),
                max_request_bytes: limits.max_request_bytes().get(),
                max_response_bytes: limits.max_response_bytes().get(),
            })
            .map_err(RunError::Command)?;
            Ok(async_driver::run(
                manifest.system_prompt().to_owned(),
                manifest.task().to_owned(),
                manifest.max_model_turns().get(),
                &mut model,
                &mut executor,
            )
            .await)
        })?;
        write_report(&mut output, &report::view(manifest.run_id(), &result))?;
        Ok(report::submitted(&result.outcome))
    })
}

fn write_report(output: &mut impl Write, value: &serde_json::Value) -> Result<(), RunError> {
    serde_json::to_writer(&mut *output, value).map_err(RunError::Json)?;
    output.write_all(b"\n").map_err(RunError::Output)?;
    output.flush().map_err(RunError::Output)
}

fn guarded<T>(operation: impl FnOnce() -> Result<T, RunError>) -> Result<T, RunError> {
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(result) => result,
        Err(payload) => {
            std::mem::forget(payload);
            Err(RunError::DependencyPanicked)
        }
    }
}

#[derive(Debug)]
pub enum RunError {
    Manifest(ManifestError),
    Runtime(std::io::Error),
    Inference(InferenceError),
    Command(CommandClientError),
    Json(serde_json::Error),
    Output(std::io::Error),
    DependencyPanicked,
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Manifest(error) => write!(f, "{error}"),
            Self::Runtime(error) => write!(f, "could not create runtime: {error}"),
            Self::Inference(error) => write!(f, "could not configure inference client: {error}"),
            Self::Command(error) => write!(f, "could not configure command client: {error}"),
            Self::Json(error) => write!(f, "could not serialize/write final report: {error}"),
            Self::Output(error) => write!(f, "could not write final report: {error}"),
            Self::DependencyPanicked => {
                f.write_str("run dependency panicked; remote execution may still be active")
            }
        }
    }
}
impl std::error::Error for RunError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_output_failures_and_unwinds_are_errors() {
        struct FailedWriter;
        impl Write for FailedWriter {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        struct FailedFlush;
        impl Write for FailedFlush {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
        }
        struct UnwindingWriter;
        impl Write for UnwindingWriter {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                // Fault injection only.
                std::panic::resume_unwind(Box::new("writer unwind"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let value = serde_json::json!({"version":1});
        assert!(matches!(
            guarded(|| write_report(&mut FailedWriter, &value)),
            Err(RunError::Json(_))
        ));
        assert!(matches!(
            guarded(|| write_report(&mut FailedFlush, &value)),
            Err(RunError::Output(_))
        ));
        assert!(matches!(
            guarded(|| write_report(&mut UnwindingWriter, &value)),
            Err(RunError::DependencyPanicked)
        ));
    }
}
