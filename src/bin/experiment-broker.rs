use std::{
    env,
    io::{self, Write},
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    process::ExitCode,
};
use thorough_but_unreliable::broker::service;

fn main() -> ExitCode {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let arguments: Vec<_> = env::args_os().collect();
        let [_, flag, path] = arguments.as_slice() else {
            return Err("usage: experiment-broker --config PATH".into());
        };
        if flag != "--config" {
            return Err("usage: experiment-broker --config PATH".into());
        }
        service::run(Path::new(path))
    }));
    let error = match result {
        Ok(Ok(())) => return ExitCode::SUCCESS,
        Ok(Err(error)) => error,
        Err(payload) => {
            std::mem::forget(payload);
            "broker dependency panicked; target cleanup is required".into()
        }
    };
    let _ = writeln!(io::stderr().lock(), "experiment-broker: {error}");
    ExitCode::FAILURE
}
