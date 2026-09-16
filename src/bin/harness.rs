use std::{
    env,
    ffi::OsString,
    io::{self, Write},
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    process::ExitCode,
};

use thorough_but_unreliable::{harness::runner, manifest::Manifest};

fn main() -> ExitCode {
    let result = match catch_unwind(AssertUnwindSafe(|| run(env::args_os().collect()))) {
        Ok(result) => result,
        Err(payload) => {
            std::mem::forget(payload);
            Err("executable dependency panicked; remote execution may still be active".into())
        }
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            // The exit status still signals failure if stderr is unavailable.
            let _ = writeln!(io::stderr().lock(), "harness: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(arguments: Vec<OsString>) -> Result<bool, String> {
    let [_, command, flag, path] = arguments.as_slice() else {
        return Err(usage());
    };

    if flag != "--manifest" {
        return Err(usage());
    }
    if command == "run" {
        return runner::run(&PathBuf::from(path), io::stdout().lock())
            .map_err(|error| error.to_string());
    }
    if command != "check-config" {
        return Err(usage());
    }

    let manifest = Manifest::from_path(&PathBuf::from(path)).map_err(|error| error.to_string())?;
    let run_id = serde_json::to_string(manifest.run_id())
        .map_err(|error| format!("could not serialize run_id: {error}"))?;
    writeln!(
        io::stdout().lock(),
        "harness config: manifest validated run_id={run_id}"
    )
    .map_err(|error| format!("could not write validation result: {error}"))?;
    Ok(true)
}

fn usage() -> String {
    "usage: harness <check-config|run> --manifest PATH".into()
}
