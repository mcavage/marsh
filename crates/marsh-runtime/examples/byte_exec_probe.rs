//! Root-run real Docker driver. This exercises the production runtime, not an
//! `OsString` snapshot. It is supporting evidence, not the final public marsh UAT.
use marsh_contracts::JobSpec;
use marsh_runtime::{
    Attachment, CommandOutput, CommandRunner, DockerCliRuntime, Invocation, JobRuntime,
    SystemCommandRunner,
};
use std::{
    io::{self, Write},
    path::Path,
    time::Duration,
};

struct Runner {
    system: SystemCommandRunner,
    lose_create_reply: bool,
    create_conflict: bool,
    fail_delete: bool,
}
impl CommandRunner for Runner {
    fn run(&self, invocation: &Invocation) -> io::Result<CommandOutput> {
        if self.fail_delete
            && invocation
                .arguments
                .first()
                .is_some_and(|word| word == "rm")
        {
            return Err(io::Error::other("injected unavailable delete transport"));
        }
        let output = self.system.run(invocation)?;
        if self.create_conflict
            && invocation
                .arguments
                .first()
                .is_some_and(|word| word == "create")
            && output.succeeded()
        {
            // Fault injection only: a real duplicate-name daemon error after
            // an actual stopped create. Never starts/replays image user code.
            return self.system.run(invocation);
        }
        if self.lose_create_reply
            && invocation
                .arguments
                .first()
                .is_some_and(|word| word == "create")
            && output.succeeded()
        {
            return Err(io::Error::other("injected loss AFTER actual Docker create"));
        }
        Ok(output)
    }
    fn run_bounded(&self, invocation: &Invocation, timeout: Duration) -> io::Result<CommandOutput> {
        self.system.run_bounded(invocation, timeout)
    }
    fn spawn_attached(&self, invocation: &Invocation) -> io::Result<Attachment> {
        self.system.spawn_attached(invocation)
    }
    fn spawn_pty_sized(
        &self,
        invocation: &Invocation,
        size: marsh_contracts::TerminalSize,
    ) -> io::Result<Attachment> {
        self.system.spawn_pty_sized(invocation, size)
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.len() != 4 {
        return Err(
            "usage: byte_exec_probe SPEC_JSON EVIDENCE_DIRECTORY normal|lost-create|create-conflict|failed-delete|orphan|reclaim"
                .into(),
        );
    }
    let root = Path::new(&args[2]);
    std::fs::create_dir(root)?;
    // Use the actual shared JobSpec wire, including its byte-valued cwd. No
    // native-driver override bypasses serde or admission for raw directories.
    let spec: JobSpec = serde_json::from_slice(&std::fs::read(&args[1])?)?;
    spec.validate()?;
    let runner = Runner {
        system: SystemCommandRunner::new("/root"),
        lose_create_reply: args[3] == "lost-create",
        create_conflict: args[3] == "create-conflict",
        fail_delete: args[3] == "failed-delete",
    };
    let runtime = DockerCliRuntime::new(runner, "/usr/bin/docker");
    if args[3] == "reclaim" {
        let reclaimed = runtime.reclaim_byte_carriers()?;
        std::fs::write(
            root.join("result.json"),
            serde_json::to_vec_pretty(&serde_json::json!({"reclaimed": reclaimed}))?,
        )?;
        return Ok(());
    }
    let container = match runtime.create(&spec) {
        Ok(container) => container,
        Err(error) => {
            std::fs::write(
                root.join("result.json"),
                serde_json::to_vec_pretty(
                    &serde_json::json!({"create": false, "error": error.to_string()}),
                )?,
            )?;
            return Err("create did not yield a container identity; ownership retained".into());
        }
    };
    std::fs::write(root.join("container-id"), container.as_str())?;
    let inspected = SystemCommandRunner::new("/root").run_bounded(
        &Invocation {
            program: "/usr/bin/docker".into(),
            arguments: [
                "container",
                "inspect",
                "--format",
                "{\"Id\":{{json .Id}},\"Mounts\":{{json .Mounts}},\"Tty\":{{json .Config.Tty}}}",
                container.as_str(),
            ]
            .map(Into::into)
            .to_vec(),
            working_directory: None,
            environment: Vec::new(),
        },
        Duration::from_secs(10),
    )?;
    if !inspected.succeeded() {
        return Err("container observation failed; ownership retained".into());
    }
    std::fs::write(root.join("container-inspect.json"), &inspected.stdout)?;
    if args[3] == "orphan" {
        std::fs::write(
            root.join("result.json"),
            serde_json::to_vec_pretty(
                &serde_json::json!({"create": true, "container": container.as_str(), "orphan": true}),
            )?,
        )?;
        return Ok(());
    }
    let mut attached = runtime.attach(&container, spec.terminal_size)?;
    let mut stdout = attached.stdout;
    let mut stderr = attached.stderr;
    let outpath = root.join("stdout.bin");
    let errpath = root.join("stderr.bin");
    let output =
        std::thread::spawn(move || io::copy(&mut stdout, &mut std::fs::File::create(outpath)?));
    let errors =
        std::thread::spawn(move || io::copy(&mut stderr, &mut std::fs::File::create(errpath)?));
    // Input is a fixed harmless binary marker, never a script/implementation.
    if !spec.terminal {
        attached.stdin.write_all(b"stdin\0\xff\xfe\n")?;
    }
    drop(attached.stdin);
    let attached_exit = attached.process.wait()?;
    output.join().map_err(|_| "stdout thread panic")??;
    errors.join().map_err(|_| "stderr thread panic")??;
    let exit = runtime.wait(&container);
    let deletion = runtime.delete(&container);
    let result = serde_json::json!({
        "create": true, "container": container.as_str(), "attached_exit": attached_exit,
        "execution": exit.as_ref().ok().map(|value| value.code),
        "wait_error": exit.err().map(|error| error.to_string()),
        "deleted": deletion.is_ok(), "delete_error": deletion.err().map(|error| error.to_string())
    });
    std::fs::write(
        root.join("result.json"),
        serde_json::to_vec_pretty(&result)?,
    )?;
    Ok(())
}

fn main() {
    if run().is_err() {
        eprintln!(
            "byte execution probe failed; inspect structural result and retain owned sources"
        );
        std::process::exit(125);
    }
}
