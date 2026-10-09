//! Real native control caller used by the isolated Linux cleanup acceptance probe.
use marsh_runtime::{CommandRunner, Invocation, SystemCommandRunner};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments.len() != 4 {
        return Err("expected FIXTURE MODE DIRECTORY TIMEOUT_MS".into());
    }
    let directory = PathBuf::from(&arguments[2]);
    let timeout = Duration::from_millis(arguments[3].to_str().ok_or("timeout encoding")?.parse()?);
    let runner = SystemCommandRunner::new(&directory);
    let invocation = Invocation {
        program: "/usr/bin/python3".into(),
        arguments: vec![
            "-I".into(),
            "-S".into(),
            arguments[0].clone(),
            "leader".into(),
            arguments[1].clone(),
            arguments[2].clone(),
        ],
        working_directory: Some(directory.clone()),
        environment: Vec::new(),
    };
    let started = Instant::now();
    let result = runner.run_bounded(&invocation, timeout);
    let elapsed_ms = started.elapsed().as_millis();
    // This gate is written ONLY AFTER the actual bounded caller returns. A
    // descendant marker after this point causally demonstrates retained effects.
    std::fs::write(directory.join("caller-returned"), b"returned\n")?;
    let document = match result {
        Ok(output) => serde_json::json!({"ok":true, "exit_code":output.exit_code,
            "stdout":output.stdout, "stderr":output.stderr, "elapsed_ms":elapsed_ms}),
        Err(error) => serde_json::json!({"ok":false, "error":error.to_string(),
            "error_kind":format!("{:?}",error.kind()), "elapsed_ms":elapsed_ms}),
    };
    println!("{document}");
    Ok(())
}
