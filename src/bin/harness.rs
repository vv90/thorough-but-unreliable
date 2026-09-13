use std::{env, ffi::OsString, path::PathBuf, process::ExitCode};

use thorough_but_unreliable::manifest::Manifest;

fn main() -> ExitCode {
    match run(env::args_os().collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("harness config: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(arguments: Vec<OsString>) -> Result<(), String> {
    let [_, command, flag, path] = arguments.as_slice() else {
        return Err(usage());
    };

    if command != "check-config" || flag != "--manifest" {
        return Err(usage());
    }

    let manifest = Manifest::from_path(&PathBuf::from(path)).map_err(|error| error.to_string())?;
    let run_id = serde_json::to_string(&manifest.run_id)
        .expect("serializing a parsed JSON string cannot fail");
    println!("harness config: manifest validated run_id={run_id}");
    Ok(())
}

fn usage() -> String {
    "usage: harness check-config --manifest PATH".into()
}
