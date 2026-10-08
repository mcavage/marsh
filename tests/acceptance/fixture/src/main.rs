use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[repr(C)]
#[derive(Clone, Copy, Default, Eq, PartialEq)]
struct WindowSize {
    rows: u16,
    columns: u16,
    x_pixels: u16,
    y_pixels: u16,
}

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

unsafe extern "C" {
    fn fork() -> i32;
    fn ioctl(fd: i32, request: usize, value: *mut WindowSize) -> i32;
    fn poll(fds: *mut PollFd, count: usize, timeout_ms: i32) -> i32;
    fn signal(number: i32, handler: extern "C" fn(i32)) -> usize;
}

const TIOCGWINSZ: usize = 0x5413;
const POLLIN: i16 = 0x0001;

fn json_string(value: &str) -> String {
    let mut output = String::from("\"");
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            value if value.is_control() => output.push_str(&format!("\\u{:04x}", value as u32)),
            value => output.push(value),
        }
    }
    output.push('"');
    output
}

fn json_array(values: &[String]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(|value| json_string(value))
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn cgroup_value(name: &str) -> Option<String> {
    fs::read_to_string(Path::new("/sys/fs/cgroup").join(name))
        .ok()
        .map(|value| value.trim().to_owned())
}

fn cpu_usage_usec() -> Result<u64, String> {
    let contents =
        fs::read_to_string("/sys/fs/cgroup/cpu.stat").map_err(|error| error.to_string())?;
    contents
        .lines()
        .find_map(|line| line.strip_prefix("usage_usec "))
        .ok_or_else(|| "cpu.stat lacks usage_usec".to_owned())?
        .parse::<u64>()
        .map_err(|error| error.to_string())
}

fn terminal_size() -> io::Result<WindowSize> {
    let mut size = WindowSize::default();
    // SAFETY: `size` is writable for exactly the kernel's winsize structure and
    // fd 1 is held open for the duration of this call.
    if unsafe { ioctl(1, TIOCGWINSZ, &mut size) } == 0 {
        Ok(size)
    } else {
        Err(io::Error::last_os_error())
    }
}

fn home_file(name: &str) -> Result<PathBuf, String> {
    let home = env::var_os("HOME").ok_or("HOME is unset")?;
    Ok(PathBuf::from(home).join(name))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|value| !value.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    let temporary = parent.join(format!(".marsh-write-{}-{nonce}", process::id()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| error.to_string())?;
        file.write_all(bytes).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        drop(file);
        fs::rename(&temporary, path).map_err(|error| error.to_string())?;
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| error.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

static TRAPPED_SIGNALS: AtomicU32 = AtomicU32::new(0);

extern "C" fn record_signal(number: i32) {
    TRAPPED_SIGNALS.fetch_or(1 << number, Ordering::Relaxed);
}

fn trap_signals() -> Result<(), String> {
    for number in [1, 2, 15] {
        if unsafe { signal(number, record_signal) } == usize::MAX {
            return Err("cannot install fixture signal handler".into());
        }
    }
    println!("READY");
    io::stdout().flush().map_err(|error| error.to_string())?;
    let mut input_closed = false;
    let mut pending = Vec::new();
    loop {
        let signals = TRAPPED_SIGNALS.swap(0, Ordering::Relaxed);
        for (number, name) in [(1, "HUP"), (2, "INT"), (15, "TERM")] {
            if signals & (1 << number) != 0 {
                println!("TRAP:{name}");
                io::stdout().flush().map_err(|error| error.to_string())?;
                if number == 15 {
                    process::exit(23);
                }
            }
        }
        if input_closed {
            thread::sleep(Duration::from_millis(5));
            continue;
        }
        let mut descriptor = PollFd {
            fd: 0,
            events: POLLIN,
            revents: 0,
        };
        let ready = unsafe { poll(&mut descriptor, 1, 10) };
        if ready <= 0 {
            continue;
        }
        let mut bytes = [0u8; 1024];
        let amount = match io::stdin().read(&mut bytes) {
            Ok(amount) => amount,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.to_string()),
        };
        if amount == 0 {
            input_closed = true;
            println!("INPUT-EOF");
        }
        pending.extend_from_slice(&bytes[..amount]);
        while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
            io::stdout()
                .write_all(b"INPUT:")
                .map_err(|error| error.to_string())?;
            io::stdout()
                .write_all(&pending[..=end])
                .map_err(|error| error.to_string())?;
            pending.drain(..=end);
        }
        if pending.len() > 65536 {
            return Err("signal fixture input exceeded bound".into());
        }
        io::stdout().flush().map_err(|error| error.to_string())?;
    }
}

/// `pipeline SEP [--exit-after SECS] ARGV [SEP ARGV]...`: run argv stages
/// (no shell) connected by pipes, as a Kit job would invoke the in-container
/// `marsh` shim. Exits with the first nonzero stage status (pipefail order).
/// `--exit-after` exits 7 after SECS without waiting for or killing stages,
/// so the job (the split creator) ends while its children still run.
fn pipeline(mut args: Vec<String>) -> Result<(), String> {
    use std::process::{Command, Stdio};
    if args.is_empty() {
        return Err("pipeline separator required".into());
    }
    let separator = args.remove(0);
    let mut exit_after = None;
    if args.first().is_some_and(|value| value == "--exit-after") {
        let seconds = args.get(1).ok_or("--exit-after seconds required")?;
        exit_after = Some(seconds.parse::<u64>().map_err(|error| error.to_string())?);
        args.drain(0..2);
    }
    // A job argument cannot be the literal `:::` (workspaces.md s2), so a
    // nested split's branch marker travels as `%%%`; each pipeline level
    // strips one `%` from longer runs so deeper levels nest the same way.
    let args = args
        .into_iter()
        .map(|value| {
            if value.len() >= 3 && value.bytes().all(|byte| byte == b'%') {
                if value.len() == 3 {
                    ":::".to_owned()
                } else {
                    value[1..].to_owned()
                }
            } else {
                value
            }
        })
        .collect::<Vec<_>>();
    let stages = args
        .split(|value| *value == separator)
        .map(<[String]>::to_vec)
        .collect::<Vec<_>>();
    if stages.iter().any(Vec::is_empty) {
        return Err("empty pipeline stage".into());
    }
    let mut children = Vec::new();
    let mut previous: Option<process::ChildStdout> = None;
    for (index, stage) in stages.iter().enumerate() {
        let mut command = Command::new(&stage[0]);
        command.args(&stage[1..]);
        if let Some(stdout) = previous.take() {
            command.stdin(Stdio::from(stdout));
        }
        if index + 1 < stages.len() {
            command.stdout(Stdio::piped());
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("pipeline: {}: {error}", stage[0]))?;
        previous = child.stdout.take();
        children.push(child);
    }
    if let Some(seconds) = exit_after {
        thread::sleep(Duration::from_secs(seconds));
        process::exit(7);
    }
    let mut statuses = Vec::new();
    for mut child in children {
        let status = child.wait().map_err(|error| error.to_string())?;
        statuses.push(status.code().unwrap_or(128));
    }
    eprintln!(
        "pipeline: statuses {}",
        statuses
            .iter()
            .map(i32::to_string)
            .collect::<Vec<_>>()
            .join(" ")
    );
    match statuses.into_iter().find(|status| *status != 0) {
        Some(status) => process::exit(status),
        None => Ok(()),
    }
}

fn main() -> Result<(), String> {
    // Enter before the legacy text-only fixture modes inspect argv. Each field
    // has a big-endian u32 length followed by its exact bytes; the final field
    // is the dedicated synthetic probe value, never the ambient environment.
    let mut raw_arguments = env::args_os().skip(1);
    let raw_mode = raw_arguments.next();
    if raw_mode.as_deref() == Some(std::ffi::OsStr::new("raw-bytes")) {
        use std::os::unix::ffi::OsStrExt as _;
        let probe = env::var_os("PROBE_VALUE").ok_or("PROBE_VALUE is unset")?;
        let mut output = io::stdout().lock();
        for value in raw_arguments.chain(std::iter::once(probe)) {
            let bytes = value.as_bytes();
            let length = u32::try_from(bytes.len()).map_err(|_| "probe value too large")?;
            output.write_all(&length.to_be_bytes()).map_err(|error| error.to_string())?;
            output.write_all(bytes).map_err(|error| error.to_string())?;
        }
        return output.flush().map_err(|error| error.to_string());
    }
    if raw_mode.as_deref() == Some(std::ffi::OsStr::new("raw-cwd")) {
        use std::os::unix::ffi::OsStrExt as _;
        let name = raw_arguments.next().ok_or("relative fixture filename required")?;
        let data = raw_arguments.next().ok_or("fixture data required")?;
        if raw_arguments.next().is_some()
            || name.is_empty()
            || name.as_bytes().contains(&b'/')
            || matches!(name.as_bytes(), b"." | b"..")
        {
            return Err("invalid relative fixture filename or arguments".into());
        }
        let cwd = env::current_dir().map_err(|error| error.to_string())?;
        let mut file = OpenOptions::new().write(true).create_new(true).open(&name)
            .map_err(|error| error.to_string())?;
        file.write_all(data.as_bytes()).map_err(|error| error.to_string())?;
        drop(file);
        let readback = fs::read(&name).map_err(|error| error.to_string())?;
        let mut output = io::stdout().lock();
        for bytes in [cwd.as_os_str().as_bytes(), readback.as_slice()] {
            let length = u32::try_from(bytes.len()).map_err(|_| "fixture field too large")?;
            output.write_all(&length.to_be_bytes()).map_err(|error| error.to_string())?;
            output.write_all(bytes).map_err(|error| error.to_string())?;
        }
        return output.flush().map_err(|error| error.to_string());
    }
    let mut arguments = env::args().skip(1);
    let mut mode = arguments.next().ok_or("fixture mode required")?;
    let mut args = arguments.collect::<Vec<_>>();
    if mode == "sync-gate" {
        mode = args.first().ok_or("gated mode required")?.clone();
        args.remove(0);
        println!("READY");
        io::stdout().flush().map_err(|error| error.to_string())?;
        let mut release = [0_u8; 1];
        io::stdin()
            .read_exact(&mut release)
            .map_err(|error| error.to_string())?;
        if release[0] != b'g' {
            return Err("invalid gate release".into());
        }
    }
    if mode == "gate" {
        let token = args.first().ok_or("gate token required")?.clone();
        mode = args.get(1).ok_or("gated mode required")?.clone();
        args.drain(0..2);
        let ready = PathBuf::from(format!(".marsh-ready-{token}"));
        let release = PathBuf::from(format!(".marsh-go-{token}"));
        fs::write(&ready, b"ready").map_err(|error| error.to_string())?;
        while !release.exists() {
            thread::sleep(Duration::from_millis(10));
        }
        let _ = fs::remove_file(ready);
        let _ = fs::remove_file(release);
    }
    match mode.as_str() {
        "identity" => {
            let cwd = env::current_dir().map_err(|error| error.to_string())?;
            let home = env::var("HOME").map_err(|error| error.to_string())?;
            let user = env::var("USER").map_err(|error| error.to_string())?;
            println!(
                "{{\"cwd\":{},\"home\":{},\"user\":{},\"pid\":{}}}",
                json_string(&cwd.to_string_lossy()),
                json_string(&home),
                json_string(&user),
                process::id()
            );
        }
        "environment" => {
            let value = |name| env::var(name).unwrap_or_default();
            println!(
                "{{\"probe\":{},\"dropped\":{},\"cloudOnly\":{},\"sshSocket\":{}}}",
                json_string(&value("PROBE_VALUE")),
                json_string(&value("DROP_ME")),
                json_string(&value("CLOUD_ONLY")),
                json_string(&value("SSH_AUTH_SOCK")),
            );
        }
        "streams" => {
            let mut stdin = Vec::new();
            io::stdin()
                .read_to_end(&mut stdin)
                .map_err(|error| error.to_string())?;
            let mut stdout = io::stdout().lock();
            stdout
                .write_all(b"OUT\0")
                .map_err(|error| error.to_string())?;
            stdout
                .write_all(json_array(&args).as_bytes())
                .map_err(|error| error.to_string())?;
            stdout.write_all(b"\n").map_err(|error| error.to_string())?;
            stdout
                .write_all(&stdin)
                .map_err(|error| error.to_string())?;
            io::stderr()
                .write_all(b"ERR\0fixture\n")
                .map_err(|error| error.to_string())?;
            process::exit(23);
        }
        "retained-grant" => {
            // Deliberately exceed one supported 512MiB capture section, while
            // remaining below the separate full recovery archive bound.
            if args.len() != 1 { return Err("project file required".into()); }
            let mut file = OpenOptions::new().write(true).create_new(true).open(&args[0]).map_err(|error| error.to_string())?;
            let chunk = vec![0x52; 1024 * 1024];
            for _ in 0..513 { file.write_all(&chunk).map_err(|error| error.to_string())?; }
            file.sync_all().map_err(|error| error.to_string())?;
            drop(file);
            println!("RETAINED-GRANT");
            io::stdout().flush().map_err(|error| error.to_string())?;
            process::exit(7);
        }
        "early-exit-capture" => {
            let project = PathBuf::from(args.first().ok_or("project file required")?);
            let home = home_file(args.get(1).ok_or("home file required")?)?;
            let bytes: u64 = args.get(2).ok_or("byte count required")?.parse().map_err(|_| "invalid byte count")?;
            if args.len() != 3 || bytes == 0 || bytes > 256 * 1024 * 1024 { return Err("capture fixture bound exceeded".into()); }
            let mut first = [0_u8; 1];
            io::stdin().read_exact(&mut first).map_err(|error| error.to_string())?;
            let chunk = vec![0x5a; 1024 * 1024];
            for path in [project, home] {
                let mut file = OpenOptions::new().write(true).create_new(true).open(path).map_err(|error| error.to_string())?;
                let mut remaining = bytes;
                while remaining != 0 {
                    let count = usize::try_from(remaining.min(chunk.len() as u64)).map_err(|_| "chunk size")?;
                    file.write_all(&chunk[..count]).map_err(|error| error.to_string())?;
                    remaining -= count as u64;
                }
                file.sync_all().map_err(|error| error.to_string())?;
            }
            println!("EARLY-EXIT");
            io::stdout().flush().map_err(|error| error.to_string())?;
            process::exit(23);
        }
        "project-write" => {
            let path = Path::new(args.first().ok_or("path required")?);
            atomic_write(path, args.get(1).ok_or("content required")?.as_bytes())?;
            if let Some(mode) = args.get(2) {
                let mode = u32::from_str_radix(mode, 8).map_err(|error| error.to_string())?;
                fs::set_permissions(path, fs::Permissions::from_mode(mode))
                    .map_err(|error| error.to_string())?;
            }
            println!("{{\"fsync\":true,\"closed\":true}}");
        }
        "project-read" => {
            let path = Path::new(args.first().ok_or("path required")?);
            let value = fs::read(path).map_err(|error| error.to_string())?;
            io::stdout()
                .write_all(&value)
                .map_err(|error| error.to_string())?;
        }
        "project-kind" => {
            let path = Path::new(args.first().ok_or("path required")?);
            let kind = match fs::symlink_metadata(path) {
                Ok(metadata) if metadata.file_type().is_dir() => "directory",
                Ok(metadata) if metadata.file_type().is_file() => "file",
                Ok(_) => "other",
                Err(error) if error.kind() == io::ErrorKind::NotFound => "missing",
                Err(error) => return Err(error.to_string()),
            };
            println!("{kind}");
        }
        "project-remove" => {
            let path = Path::new(args.first().ok_or("path required")?);
            fs::remove_file(path).map_err(|error| error.to_string())?;
        }
        "project-chmod" => {
            let path = Path::new(args.first().ok_or("path required")?);
            let mode = u32::from_str_radix(args.get(1).ok_or("mode required")?, 8)
                .map_err(|error| error.to_string())?;
            fs::set_permissions(path, fs::Permissions::from_mode(mode))
                .map_err(|error| error.to_string())?;
        }
        "project-double-write" => {
            if args.len() != 4 {
                return Err("two path/content pairs required".into());
            }
            for pair in args.chunks_exact(2) {
                atomic_write(Path::new(&pair[0]), pair[1].as_bytes())?;
            }
        }
        "recovery-tree-edit" => {
            let project = PathBuf::from(args.first().ok_or("project directory required")?);
            let home = home_file(args.get(1).ok_or("home directory required")?)?;
            for root in [&project, &home] {
                for (name, content, mode) in [
                    ("edited.txt", b"cloud edited\n".as_slice(), 0o640),
                    ("added.txt", b"cloud added\n".as_slice(), 0o600),
                ] {
                    let path = root.join(name);
                    atomic_write(&path, content)?;
                    fs::set_permissions(path, fs::Permissions::from_mode(mode))
                        .map_err(|error| error.to_string())?;
                }
                for name in ["remote-deleted.txt", "remote-deleted-link", "mutable-link"] {
                    fs::remove_file(root.join(name)).map_err(|error| error.to_string())?;
                }
                for (name, target) in [
                    ("mutable-link", "edited.txt"),
                    ("added-link", "missing-cloud-target"),
                ] {
                    std::os::unix::fs::symlink(target, root.join(name))
                        .map_err(|error| error.to_string())?;
                }
                for (name, mode) in [("mode-only.txt", 0o500), ("mode-dir", 0o550)] {
                    fs::set_permissions(root.join(name), fs::Permissions::from_mode(mode))
                        .map_err(|error| error.to_string())?;
                }
            }
            let blocked = project.join("blocked/write.txt");
            atomic_write(&blocked, b"cloud blocked\n")?;
            fs::set_permissions(blocked, fs::Permissions::from_mode(0o644))
                .map_err(|error| error.to_string())?;
            if args.get(2).is_some_and(|value| value == "exit-7") { process::exit(7); }
        }
        "project-write-home-replace" => {
            let project = Path::new(args.first().ok_or("project path required")?);
            atomic_write(
                project,
                args.get(1).ok_or("project content required")?.as_bytes(),
            )?;
            let home = home_file(args.get(2).ok_or("home name required")?)?;
            fs::remove_file(&home).map_err(|error| error.to_string())?;
            fs::create_dir(&home).map_err(|error| error.to_string())?;
            fs::write(home.join("child"), b"Cloud directory content")
                .map_err(|error| error.to_string())?;
        }
        "project-symlink" => {
            let path = Path::new(args.first().ok_or("path required")?);
            let target = args.get(1).ok_or("target required")?;
            std::os::unix::fs::symlink(target, path).map_err(|error| error.to_string())?;
        }
        "home-write" => {
            let path = home_file(args.first().ok_or("name required")?)?;
            atomic_write(&path, args.get(1).ok_or("content required")?.as_bytes())?;
            println!("{{\"fsync\":true,\"closed\":true}}");
        }
        "home-read" => {
            let value = fs::read(home_file(args.first().ok_or("name required")?)?)
                .map_err(|error| error.to_string())?;
            io::stdout()
                .write_all(&value)
                .map_err(|error| error.to_string())?;
        }
        "pipeline" => return pipeline(args),
        // `exec ARGV...`: replace this process (no fork) with ARGV resolved
        // through PATH, as a `#!/bin/sh` entrypoint's `exec claude` does.
        // Run as the entrypoint, the exec'd process keeps docker-init as its
        // parent (docs/design/processes.md section 5, entry case).
        "exec" => {
            use std::os::unix::process::CommandExt as _;
            let program = args.first().ok_or("exec: program required")?;
            let error = process::Command::new(program).args(&args[1..]).exec();
            eprintln!("exec: {program}: {error}");
            process::exit(127);
        }
        // `bench N SEP ARGV [SEP ARGV]...`: run each ARGV N times, interleaved,
        // stdio to /dev/null; print per-ARGV wall times in microseconds.
        "bench" => {
            use std::process::{Command, Stdio};
            let count: usize = args.first().ok_or("bench: count required")?.parse().map_err(|_| "bench: count")?;
            let separator = args.get(1).ok_or("bench: separator required")?.clone();
            let groups = args[2..].split(|value| *value == separator).filter(|g| !g.is_empty()).map(<[String]>::to_vec).collect::<Vec<_>>();
            if count == 0 || count > 1000 || groups.is_empty() { return Err("bench: bad arguments".into()); }
            let mut samples = vec![Vec::new(); groups.len()];
            let mut failures = vec![0_u32; groups.len()];
            for _ in 0..count {
                for (index, group) in groups.iter().enumerate() {
                    let began = Instant::now();
                    let status = Command::new(&group[0]).args(&group[1..]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().map_err(|error| format!("bench: {}: {error}", group[0]))?;
                    samples[index].push(began.elapsed().as_micros().to_string());
                    if !status.success() { failures[index] += 1; }
                }
            }
            let rows = groups.iter().zip(&samples).zip(&failures).map(|((group, us), failed)| format!("{{\"argv\":{},\"us\":[{}],\"failed\":{failed}}}", json_array(group), us.join(","))).collect::<Vec<_>>();
            println!("[{}]", rows.join(","));
        }
        // `cap-flood COUNT [PATH]`: hold COUNT simultaneous idle connections to
        // the job capability socket, then report what each one received
        // (docs/design/processes.md section 3: refusal frame, never a silent drop).
        "cap-flood" => {
            use std::os::unix::net::UnixStream;
            let count: usize = args.first().ok_or("cap-flood: count required")?.parse().map_err(|_| "cap-flood: count")?;
            let path = args.get(1).map_or("/run/marsh/cap.sock", String::as_str);
            if count == 0 || count > 256 { return Err("cap-flood: count out of range".into()); }
            let mut streams = Vec::new();
            for _ in 0..count {
                streams.push(UnixStream::connect(path));
                thread::sleep(Duration::from_millis(20));
            }
            thread::sleep(Duration::from_millis(1500));
            let mut rows = Vec::new();
            for (index, stream) in streams.iter_mut().enumerate() {
                let row = match stream {
                    Err(error) => format!("{{\"i\":{index},\"connect\":{},\"read\":\"\",\"eof\":false}}", json_string(&error.to_string())),
                    Ok(stream) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(300)));
                        let mut bytes = Vec::new();
                        let mut buffer = [0_u8; 4096];
                        let mut eof = false;
                        while bytes.len() < 65536 {
                            match stream.read(&mut buffer) {
                                Ok(0) => { eof = true; break; }
                                Ok(amount) => bytes.extend_from_slice(&buffer[..amount]),
                                Err(_) => break,
                            }
                        }
                        format!("{{\"i\":{index},\"connect\":\"ok\",\"read\":{},\"eof\":{eof}}}", json_string(&String::from_utf8_lossy(&bytes)))
                    }
                };
                rows.push(row);
            }
            println!("[{}]", rows.join(","));
        }
        "trap-signals" => return trap_signals(),
        "hold" => {
            println!("READY");
            io::stdout().flush().map_err(|error| error.to_string())?;
            let seconds = args
                .first()
                .ok_or("seconds required")?
                .parse::<u64>()
                .map_err(|error| error.to_string())?;
            thread::sleep(Duration::from_secs(seconds));
        }
        "limits" => println!(
            "{{\"cpu.max\":{},\"memory.max\":{},\"pids.max\":{}}}",
            cgroup_value("cpu.max")
                .as_deref()
                .map_or("null".into(), json_string),
            cgroup_value("memory.max")
                .as_deref()
                .map_or("null".into(), json_string),
            cgroup_value("pids.max")
                .as_deref()
                .map_or("null".into(), json_string)
        ),
        "memory-pressure" => {
            let mut blocks = Vec::new();
            loop {
                blocks.push(vec![0xa5_u8; 8 * 1024 * 1024]);
                std::hint::black_box(&blocks);
            }
        }
        "cpu-pressure" => {
            let seconds = args
                .first()
                .ok_or("seconds required")?
                .parse::<u64>()
                .map_err(|error| error.to_string())?;
            let before = cpu_usage_usec()?;
            let deadline = Instant::now() + Duration::from_secs(seconds);
            let mut value = 0_u64;
            while Instant::now() < deadline {
                value = value.wrapping_mul(6364136223846793005).wrapping_add(1);
                std::hint::black_box(value);
            }
            println!("{}", cpu_usage_usec()?.saturating_sub(before));
        }
        "pid-pressure" => loop {
            // SAFETY: the child immediately sleeps without touching shared
            // synchronization state; this single-threaded fixture has no locks.
            let pid = unsafe { fork() };
            if pid == 0 {
                thread::sleep(Duration::from_secs(60));
                process::exit(0);
            }
            if pid < 0 {
                return Err(io::Error::last_os_error().to_string());
            }
        },
        "output-pressure" => {
            let block = [b'x'; 64 * 1024];
            let mut stdout = io::stdout().lock();
            loop {
                stdout
                    .write_all(&block)
                    .map_err(|error| error.to_string())?;
            }
        }
        "writable-pressure" => {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open("/tmp/marsh-writable-pressure")
                .map_err(|error| error.to_string())?;
            let block = [b'w'; 1024 * 1024];
            loop {
                file.write_all(&block).map_err(|error| error.to_string())?;
                file.sync_data().map_err(|error| error.to_string())?;
            }
        }
        "authority" => {
            let sockets = [
                "/var/run/docker.sock",
                "/run/docker.sock",
                "/run/containerd/containerd.sock",
                "/run/marsh/daemon.sock",
                "/run/sbx/sbx.sock",
            ]
            .into_iter()
            .filter(|path| Path::new(path).exists())
            .map(str::to_owned)
            .collect::<Vec<_>>();
            let authority = env::vars()
                .map(|(key, _)| key)
                .filter(|key| {
                    key.starts_with("DOCKER_")
                        || key.starts_with("CONTAINERD_")
                        || key.starts_with("SBX_")
                        || (key.starts_with("MARSH_")
                            && ["SOCKET", "TOKEN", "ENDPOINT"]
                                .iter()
                                .any(|word| key.contains(word)))
                })
                .collect::<Vec<_>>();
            println!(
                "{{\"sockets\":{},\"authority_env\":{}}}",
                json_array(&sockets),
                json_array(&authority)
            );
        }
        "tty" => {
            if !(io::stdin().is_terminal()
                && io::stdout().is_terminal()
                && io::stderr().is_terminal())
            {
                process::exit(70);
            }
            let mut previous = terminal_size().map_err(|error| error.to_string())?;
            println!("\x1b[32mCOLOR\x1b[0m");
            io::stdout().flush().map_err(|error| error.to_string())?;
            loop {
                let current = terminal_size().map_err(|error| error.to_string())?;
                if current != previous {
                    println!("SIZE={}x{}", current.rows, current.columns);
                    io::stdout().flush().map_err(|error| error.to_string())?;
                    previous = current;
                }
                let mut descriptor = PollFd {
                    fd: 0,
                    events: POLLIN,
                    revents: 0,
                };
                // SAFETY: `descriptor` is valid for one pollfd and remains
                // writable for the duration of this bounded call.
                let result = unsafe { poll(&mut descriptor, 1, 50) };
                if result < 0 {
                    return Err(io::Error::last_os_error().to_string());
                }
                if result > 0 && descriptor.revents & POLLIN != 0 {
                    let mut input = [0_u8; 256];
                    let length = io::stdin()
                        .read(&mut input)
                        .map_err(|error| error.to_string())?;
                    if length == 0 {
                        println!("EOF");
                        break;
                    }
                    io::stdout()
                        .write_all(b"PASTE:")
                        .and_then(|()| io::stdout().write_all(&input[..length]))
                        .and_then(|()| io::stdout().flush())
                        .map_err(|error| error.to_string())?;
                }
            }
        }
        "wall" => thread::sleep(Duration::from_secs(3600)),
        other => return Err(format!("unknown fixture mode: {other}")),
    }
    Ok(())
}
