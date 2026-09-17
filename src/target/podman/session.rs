use super::{
    Config, Error, guard,
    stream::Decoder,
    wire::{self, ExecId, Observed},
};
use crate::target::*;

/// External effect failure, supplied by the future transport. Once start could
/// have been dispatched, no failure can assert that the command never ran.
pub enum Failure {
    Transport,
    Deadline,
    DependencyPanicked,
}

/// Pure owner of one binding. No Clone/reset; replacing it cannot resume a run.
/// Only a complete, correlated observation restores readiness after begin.
pub struct Session {
    config: Config,
    state: SessionState,
}
impl Session {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            state: SessionState::Ready,
        }
    }
    pub fn config(&self) -> &Config {
        &self.config
    }
    pub fn state(&self) -> SessionState {
        self.state
    }

    /// Reservation survives cancellation, including forgetting an unfinished
    /// stage. The borrow prevents overlapping executions at compile time.
    ///
    /// ```compile_fail
    /// use thorough_but_unreliable::target::{podman::Session, CommandRequest};
    /// fn overlap(session: &mut Session, a: CommandRequest, b: CommandRequest) {
    ///     if let Ok(first) = session.begin(a) {
    ///         let second = session.begin(b);
    ///         let _ = first.body();
    ///     }
    /// }
    /// ```
    pub fn begin(&mut self, request: CommandRequest) -> Result<Create<'_>, ExecutionReport> {
        let sequence = request.sequence;
        let rejection = if self.state == SessionState::Unusable {
            Some(StartFailure::SessionUnusable)
        } else if request.command.contains('\0') {
            Some(StartFailure::Rejected(CommandRejection {
                kind: RejectionKind::InvalidCommand,
                diagnostic: None,
            }))
        } else if request.command.len() > self.config.limits.command {
            Some(StartFailure::Rejected(CommandRejection {
                kind: RejectionKind::CommandTooLarge,
                diagnostic: None,
            }))
        } else {
            None
        };
        if let Some(failure) = rejection {
            return Err(ExecutionReport {
                sequence,
                outcome: ExecutionOutcome::NotStarted(failure),
            });
        }
        let body = match wire::create(&self.config, &request.command) {
            Ok(body) => body,
            Err(Error::BodyTooLarge { .. }) => {
                return Err(ExecutionReport {
                    sequence,
                    outcome: ExecutionOutcome::NotStarted(StartFailure::Rejected(
                        CommandRejection {
                            kind: RejectionKind::CommandTooLarge,
                            diagnostic: None,
                        },
                    )),
                });
            }
            Err(error) => {
                self.state = SessionState::Unusable;
                return Err(ExecutionReport {
                    sequence,
                    outcome: ExecutionOutcome::NotStarted(StartFailure::Failed(classify(error))),
                });
            }
        };
        self.state = SessionState::Unusable;
        Ok(Create {
            attempt: Attempt {
                session: self,
                sequence,
            },
            body,
        })
    }
}

struct Attempt<'a> {
    session: &'a mut Session,
    sequence: CommandSequence,
}
impl Attempt<'_> {
    fn not_started(self, error: Error) -> ExecutionReport {
        ExecutionReport {
            sequence: self.sequence,
            outcome: ExecutionOutcome::NotStarted(StartFailure::Failed(classify(error))),
        }
    }
    fn unknown(self, error: Error, output: CommandOutput) -> ExecutionReport {
        ExecutionReport {
            sequence: self.sequence,
            outcome: ExecutionOutcome::Unknown {
                error: classify(error),
                output,
            },
        }
    }
    fn failed(self, failure: Failure, output: CommandOutput) -> ExecutionReport {
        let outcome = match failure {
            Failure::Deadline => ExecutionOutcome::DeadlineExceeded {
                output,
                execution_state: ExecutionState::MayStillBeRunning,
            },
            Failure::Transport => ExecutionOutcome::Unknown {
                output,
                error: ExecutionError {
                    kind: ExecutionErrorKind::Transport,
                    diagnostic: None,
                },
            },
            Failure::DependencyPanicked => ExecutionOutcome::Unknown {
                output,
                error: ExecutionError {
                    kind: ExecutionErrorKind::DependencyPanicked,
                    diagnostic: None,
                },
            },
        };
        ExecutionReport {
            sequence: self.sequence,
            outcome,
        }
    }
}

/// POST path() with body(), requiring HTTP 201 and a bounded JSON body.
pub struct Create<'a> {
    attempt: Attempt<'a>,
    body: Vec<u8>,
}
impl<'a> Create<'a> {
    pub fn path(&self) -> &str {
        &self.attempt.session.config.create_path
    }
    pub fn body(&self) -> &[u8] {
        &self.body
    }
    pub fn created(self, status: u16, body: &[u8]) -> Result<Start<'a>, ExecutionReport> {
        let result = guard(|| {
            require(status, 201)?;
            let exec = wire::created(body, self.attempt.session.config.limits.json)?;
            let start_path = format!("/exec/{}/start", exec.as_str());
            let inspect_path = format!("/exec/{}/json", exec.as_str());
            Ok((exec, start_path, inspect_path))
        });
        match result {
            Ok((exec, start_path, inspect_path)) => Ok(Start {
                attempt: self.attempt,
                exec,
                start_path,
                inspect_path,
            }),
            Err(error) => Err(self.attempt.not_started(error)),
        }
    }
    pub fn fail(self, failure: Failure) -> ExecutionReport {
        let kind = match failure {
            Failure::Transport | Failure::Deadline => ExecutionErrorKind::Transport,
            Failure::DependencyPanicked => ExecutionErrorKind::DependencyPanicked,
        };
        ExecutionReport {
            sequence: self.attempt.sequence,
            outcome: ExecutionOutcome::NotStarted(StartFailure::Failed(ExecutionError {
                kind,
                diagnostic: None,
            })),
        }
    }
}

/// POST with Connection: Upgrade and Upgrade: tcp, then validate the actual
/// upgrade in the IO layer before calling attached. Never retry this effect.
pub struct Start<'a> {
    attempt: Attempt<'a>,
    exec: ExecId,
    start_path: String,
    inspect_path: String,
}
impl<'a> Start<'a> {
    pub fn path(&self) -> &str {
        &self.start_path
    }
    pub fn body(&self) -> &'static [u8] {
        br#"{"Detach":false,"Tty":false}"#
    }
    pub fn attached(self, status: u16) -> Result<Capture<'a>, ExecutionReport> {
        if let Err(error) = require(status, 101) {
            return Err(self.attempt.unknown(error, CommandOutput::default()));
        }
        let limits = &self.attempt.session.config.limits;
        let decoder = Decoder::new(limits.stdout, limits.stderr);
        Ok(Capture {
            attempt: self.attempt,
            exec: self.exec,
            inspect_path: self.inspect_path,
            decoder,
        })
    }
    pub fn fail(self, failure: Failure) -> ExecutionReport {
        self.attempt.failed(failure, CommandOutput::default())
    }
}

/// Feed raw upgraded stream bytes. Capture caps do not stop draining; the
/// transport must enforce the original deadline even after either cap is full.
pub struct Capture<'a> {
    attempt: Attempt<'a>,
    exec: ExecId,
    inspect_path: String,
    decoder: Decoder,
}
impl<'a> Capture<'a> {
    pub fn feed(mut self, bytes: &[u8]) -> Result<Self, ExecutionReport> {
        match guard(|| self.decoder.feed(bytes)) {
            Ok(()) => Ok(self),
            Err(error) => Err(self.attempt.unknown(error, self.decoder.into_output())),
        }
    }
    pub fn eof(self) -> Result<Inspect<'a>, ExecutionReport> {
        if !self.decoder.at_boundary() {
            return Err(self.attempt.unknown(
                Error::InvalidResponse("EOF within stream frame"),
                self.decoder.into_output(),
            ));
        }
        Ok(Inspect {
            attempt: self.attempt,
            exec: self.exec,
            path: self.inspect_path,
            output: self.decoder.into_output(),
        })
    }
    pub fn fail(self, failure: Failure) -> ExecutionReport {
        self.attempt.failed(failure, self.decoder.into_output())
    }
}

/// GET path() after clean stream EOF. A pending observation permits another
/// inspection under the same deadline, never another start. No sleeps here.
pub struct Inspect<'a> {
    attempt: Attempt<'a>,
    exec: ExecId,
    path: String,
    output: CommandOutput,
}
pub enum Inspection<'a> {
    Pending(Inspect<'a>),
    Finished(ExecutionReport),
}
impl<'a> Inspect<'a> {
    pub fn path(&self) -> &str {
        &self.path
    }
    pub fn observed(self, status: u16, bytes: &[u8]) -> Result<Inspection<'a>, ExecutionReport> {
        let config = &self.attempt.session.config;
        let result = require(status, 200)
            .and_then(|()| wire::inspect(bytes, config.limits.json, &self.exec, config));
        match result {
            Ok(Observed::Pending) => Ok(Inspection::Pending(self)),
            Ok(Observed::Stopped(code)) => {
                self.attempt.session.state = SessionState::Ready;
                Ok(Inspection::Finished(ExecutionReport {
                    sequence: self.attempt.sequence,
                    outcome: ExecutionOutcome::Completed {
                        output: self.output,
                        completion: ProcessCompletion::RuntimeStatus {
                            code,
                            source: CompletionSource::ParentObserved,
                        },
                        session_state: SessionState::Ready,
                    },
                }))
            }
            Err(error) => Err(self.attempt.unknown(error, self.output)),
        }
    }
    pub fn fail(self, failure: Failure) -> ExecutionReport {
        self.attempt.failed(failure, self.output)
    }
}

fn require(actual: u16, expected: u16) -> Result<(), Error> {
    if actual == expected {
        Ok(())
    } else {
        Err(Error::HttpStatus(actual))
    }
}
fn classify(error: Error) -> ExecutionError {
    let kind = match error {
        Error::Allocation(_) => ExecutionErrorKind::ResourceExhausted,
        Error::DependencyPanicked => ExecutionErrorKind::DependencyPanicked,
        Error::Configuration(_) | Error::HttpStatus(_) | Error::RuntimeFailure => {
            ExecutionErrorKind::ExecutionMechanism
        }
        Error::BodyTooLarge { .. } | Error::Json(_) | Error::InvalidResponse(_) => {
            ExecutionErrorKind::MalformedResponse
        }
    };
    // Runtime error text and arbitrary response bodies are not exposed to the model.
    ExecutionError {
        kind,
        diagnostic: None,
    }
}
