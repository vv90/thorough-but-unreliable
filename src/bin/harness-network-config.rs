use std::{
    env,
    fs::File,
    io::{self, Read, Write},
    panic::{AssertUnwindSafe, catch_unwind},
    process::ExitCode,
};
use thorough_but_unreliable::network::{DEFAULT, MAX_BYTES, Network};

fn run() -> Result<(), String> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    let bytes = match args.as_slice() {
        [] => DEFAULT.as_bytes().to_vec(),
        [path] => {
            let mut bytes = Vec::new();
            File::open(path)
                .map_err(|e| e.to_string())?
                .take((MAX_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .map_err(|e| e.to_string())?;
            bytes
        }
        _ => return Err("usage: harness-network-config [network.json]".into()),
    };
    let network = Network::parse(&bytes)?;
    serde_json::to_writer(io::stdout().lock(), &network).map_err(|e| e.to_string())
}
fn main() -> ExitCode {
    let result = match catch_unwind(AssertUnwindSafe(run)) {
        Ok(result) => result,
        Err(payload) => {
            std::mem::forget(payload);
            Err("network configuration dependency panicked".into())
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "harness network: {error}");
            ExitCode::FAILURE
        }
    }
}
