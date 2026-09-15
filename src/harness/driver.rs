//! Thin synchronous effect layer for in-memory adapters and deterministic tests.

use std::panic::{AssertUnwindSafe, catch_unwind};

use super::{
    model_loop::{Step, start},
    types::{AssistantResponse, CommandCall, CommandClientError, Message, ModelFailure, RunReport},
};

use crate::target::ExecutionReport;

pub trait ModelClient {
    fn respond(&mut self, history: &[Message]) -> Result<AssistantResponse, ModelFailure>;
}

/// The client assigns broker sequences and validates response correlation before
/// returning a report. Transport/decoding failures return CommandClientError.
pub trait CommandExecutor {
    fn execute(&mut self, call: &CommandCall) -> Result<ExecutionReport, CommandClientError>;
}

/// Invokes each requested effect once. Any dependency error ends the run;
/// adapters must not silently retry requests either.
pub fn run(
    system_prompt: String,
    task: String,
    max_model_turns: u32,
    model: &mut impl ModelClient,
    executor: &mut impl CommandExecutor,
) -> RunReport {
    let mut step = start(system_prompt, task, max_model_turns);
    loop {
        step = match step {
            Step::Model(turn) => {
                let result =
                    dependency_call(|| model.respond(turn.history()), ModelFailure::panicked);
                turn.complete(result)
            }
            Step::Command(turn) => {
                let result = dependency_call(
                    || executor.execute(turn.call()),
                    CommandClientError::panicked,
                );
                turn.complete(result)
            }
            Step::Finished(report) => return report,
        };
    }
}

fn dependency_call<T, E>(
    call: impl FnOnce() -> Result<T, E>,
    panicked: impl FnOnce() -> E,
) -> Result<T, E> {
    // Unwinding can leave an adapter in an inconsistent state. The driver
    // terminates immediately in that case; the caller must discard the adapters.
    // This cannot catch aborts, and does not replace the process panic hook.
    match catch_unwind(AssertUnwindSafe(call)) {
        Ok(result) => result,
        Err(payload) => {
            // A foreign panic payload can itself panic in Drop. Retain it rather
            // than risking a second unwind. This leaks only on dependency panic.
            std::mem::forget(payload);
            Err(panicked())
        }
    }
}
