//! Sequential async effects around the pure state machine.

use std::{future::Future, panic::AssertUnwindSafe};

use futures_util::FutureExt;

use super::{
    model_loop::{Step, start},
    types::{AssistantResponse, CommandCall, CommandResult, DependencyError, Message, RunReport},
};

pub trait ModelClient {
    fn respond(
        &mut self,
        history: &[Message],
    ) -> impl Future<Output = Result<AssistantResponse, DependencyError>> + Send;
}

pub trait CommandExecutor {
    fn execute(
        &mut self,
        call: &CommandCall,
    ) -> impl Future<Output = Result<CommandResult, DependencyError>> + Send;
}

/// Await each requested effect once, in order, with no driver retries.
/// Adapters must also disable retries. Discard adapters after a caught panic.
/// Dropping this future cancels driving without producing a report; an effect
/// already dispatched may still complete remotely. Do not restart it blindly.
pub async fn run(
    system_prompt: String,
    task: String,
    max_model_turns: u32,
    model: &mut (impl ModelClient + Send),
    executor: &mut (impl CommandExecutor + Send),
) -> RunReport {
    let mut step = start(system_prompt, task, max_model_turns);
    loop {
        step = match step {
            Step::Model(turn) => {
                // Calling the adapter inside the guarded async block also
                // catches panics during future construction, before its await.
                let result = dependency_call(async { model.respond(turn.history()).await }).await;
                turn.complete(result)
            }
            Step::Command(turn) => {
                let result = dependency_call(async { executor.execute(turn.call()).await }).await;
                turn.complete(result)
            }
            Step::Finished(report) => return report,
        };
    }
}

async fn dependency_call<T>(
    future: impl Future<Output = Result<T, DependencyError>>,
) -> Result<T, DependencyError> {
    match AssertUnwindSafe(future).catch_unwind().await {
        Ok(result) => result,
        Err(payload) => {
            // Foreign payload destructors can panic. Retain the payload, as in
            // the synchronous driver. Aborts and the global hook are unaffected.
            std::mem::forget(payload);
            Err(DependencyError::Panicked)
        }
    }
}
