//! Lossless data at the final native exec boundary. Never log a payload.
//!
//! The producer is responsible for command authority (the inspected image's
//! Entrypoint/Cmd, or the fixed shell admission command). This codec grants no
//! authority, launches no interpreter, and never replaces invalid UTF-8.

use marsh_contracts::{ExportedEnvironment, validate_exported_environment};
use std::{
    ffi::{CString, OsString},
    io::{self, Read},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Component, Path},
};

const MAGIC: &[u8; 8] = b"SBXBYT01";
// Bound native decoding without imposing the Kit's 256-tail-word policy on an
// ordinary shell. Linux's exec pointer budget is already tighter at this count.
pub const MAX_ARGUMENTS: usize = 256 * 1024;
// Linux MAX_ARG_STRLEN includes the terminating NUL. JobSpec independently
// keeps the existing stricter 16 KiB caller-tail policy; shell script/OCI words
// must not inherit that Kit-only restriction merely for needing byte transport.
pub const MAX_WORD_BYTES: usize = 128 * 1024 - 1;
// Codec allocation bound, NOT a promise that exec accepts this much. The
// kernel also counts inherited environment/pointers and the current stack
// limit; a valid payload may fail with E2BIG (cannot-execute status 126).
pub const MAX_PAYLOAD_BYTES: usize = 9 * 1024 * 1024;
pub const SHELL_PAYLOAD_DIRECTORY_PREFIX: &str = "/run/.marsh-shell-payload-";

/// Secret-bearing launch data. Intentionally does not implement Debug/Display.
pub struct Payload {
    pub argv: Vec<Vec<u8>>,
    pub environment: ExportedEnvironment,
    /// Empty means inherit cwd. Nonempty is an absolute, non-traversing Unix path.
    pub working_directory: Vec<u8>,
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "invalid or oversized native byte payload",
    )
}

impl Payload {
    /// Validate all data before chdir, environment mutation, or exec.
    ///
    /// # Errors
    /// Rejects size, count, NUL, cwd and exported-environment policy violations.
    pub fn validate(&self) -> io::Result<()> {
        if self.argv.is_empty()
            || self.argv.len() > MAX_ARGUMENTS
            || self.argv[0].is_empty()
            || self
                .argv
                .iter()
                .any(|word| word.len() > MAX_WORD_BYTES || word.contains(&0))
        {
            return Err(invalid());
        }
        let argument_bytes = self.argv.iter().map(|word| word.len() + 4).sum::<usize>();
        if argument_bytes > MAX_PAYLOAD_BYTES - 128 * 1024 {
            return Err(invalid());
        }
        validate_exported_environment(&self.environment).map_err(|_| invalid())?;
        if !self.working_directory.is_empty() {
            let cwd = Path::new(std::ffi::OsStr::from_bytes(&self.working_directory));
            if self.working_directory.len() > 4096
                || self.working_directory.contains(&0)
                || !cwd.is_absolute()
                || cwd
                    .components()
                    .any(|part| matches!(part, Component::CurDir | Component::ParentDir))
            {
                return Err(invalid());
            }
        }
        Ok(())
    }

    /// Encode a validated, versioned, length-delimited binary payload.
    ///
    /// # Errors
    /// Rejects invalid data before producing an executable carrier.
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        self.validate()?;
        let mut output = MAGIC.to_vec();
        put_length(&mut output, self.argv.len())?;
        for word in &self.argv {
            put_blob(&mut output, word)?;
        }
        put_length(&mut output, self.environment.len())?;
        for (name, value) in &self.environment {
            put_blob(&mut output, name.as_bytes())?;
            put_blob(&mut output, value)?;
        }
        put_blob(&mut output, &self.working_directory)?;
        if output.len() > MAX_PAYLOAD_BYTES {
            return Err(invalid());
        }
        Ok(output)
    }

    /// Decode without trusting length/count fields for unbounded allocation.
    ///
    /// # Errors
    /// Rejects malformed, trailing or policy-invalid data. Environment ordering
    /// is not significant. Absolute cwd spellings with interior `/./`, repeated
    /// separators or a trailing slash are accepted as equivalent Unix paths;
    /// parent traversal is rejected. The producer emits sorted env names.
    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() > MAX_PAYLOAD_BYTES || !bytes.starts_with(MAGIC) {
            return Err(invalid());
        }
        let mut input = Input(&bytes[MAGIC.len()..]);
        let count = input.length(MAX_ARGUMENTS)?;
        let mut argv = Vec::with_capacity(count);
        for _ in 0..count {
            argv.push(input.blob(MAX_WORD_BYTES)?.to_vec());
        }
        let count = input.length(256)?;
        let mut environment = ExportedEnvironment::new();
        let mut total = 0_usize;
        for _ in 0..count {
            let name = input.blob(128)?;
            let value = input.blob(16 * 1024)?;
            total += name.len() + value.len();
            if total > 64 * 1024 {
                return Err(invalid());
            }
            let name = std::str::from_utf8(name).map_err(|_| invalid())?.to_owned();
            if environment.insert(name, value.to_vec()).is_some() {
                return Err(invalid());
            }
        }
        let working_directory = input.blob(4096)?.to_vec();
        if !input.0.is_empty() {
            return Err(invalid());
        }
        let payload = Self {
            argv,
            environment,
            working_directory,
        };
        payload.validate()?;
        Ok(payload)
    }

    /// Read at most the fixed payload limit, including a one-byte overflow probe.
    ///
    /// # Errors
    /// Reports a fixed error for malformed data; I/O errors never contain payloads.
    pub fn read(input: impl Read) -> io::Result<Self> {
        let mut bytes = Vec::new();
        input
            .take((MAX_PAYLOAD_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        Self::decode(&bytes)
    }
}

/// Decode only the fixed trusted shell-admission launch, without executing,
/// changing cwd/environment, forking, or recursively dispatching hidden CLI.
/// The caller must retain argv[0], enroll/drop identity with `record_current`,
/// then apply the returned cwd before entering Brush.
///
/// # Errors
/// Rejects a non-admission command, environment overrides, nested byte wrapper,
/// invalid numeric identity or malformed bounded payload, with fixed diagnostics.
/// UID/GID use canonical nonzero decimal, matching the trusted producer.
/// Only the private-file transport is accepted.
pub fn decode_shell_launch(
    arguments: &[OsString],
) -> io::Result<(Vec<OsString>, Option<std::path::PathBuf>)> {
    if arguments.len() != 2 || arguments[0] != "--file" {
        return Err(invalid());
    }
    let payload = read_shell_payload_file(Path::new(&arguments[1]))?;
    validate_shell_payload(&payload)?;
    let cwd = (!payload.working_directory.is_empty())
        .then(|| std::path::PathBuf::from(OsString::from_vec(payload.working_directory)));
    Ok((
        payload.argv.into_iter().map(OsString::from_vec).collect(),
        cwd,
    ))
}

/// Pure shared staging/launch validation. No enrollment, UID/cwd/environment
/// mutation or file effects. The record grammar matches session containment.
///
/// # Errors
/// Rejects non-admission commands, overrides, noncanonical identity/record
/// paths, nested wrappers and invalid/oversized payloads with a fixed error.
pub fn validate_shell_payload(payload: &Payload) -> io::Result<()> {
    payload.validate()?;
    if !payload.environment.is_empty()
        || payload.argv.len() < 4
        || payload.argv[0] != b"--internal-record-session"
        || payload.argv[1].len() > 4096
        || !Path::new(std::ffi::OsStr::from_bytes(&payload.argv[1])).is_absolute()
        || payload.argv[2..4].iter().any(|word| {
            word.is_empty()
                || word[0] == b'0'
                || !word.iter().all(u8::is_ascii_digit)
                || std::str::from_utf8(word)
                    .ok()
                    .and_then(|word| word.parse::<u32>().ok())
                    .is_none_or(|id| id == 0)
        })
        || payload.argv.get(4).is_some_and(|word| {
            word == b"--internal-byte-launch" || word == b"--internal-record-session"
        })
    {
        return Err(invalid());
    }
    let record = payload.argv[1]
        .split(|byte| *byte == b'/')
        .collect::<Vec<_>>();
    if record.len() != 6
        || record[0] != b""
        || record[1] != b"run"
        || record[2] != b"marsh"
        || record[3] != payload.argv[2]
        || record[5] != b"shell"
        || record[4].is_empty()
        || record[4].len() > 128
        || !record[4]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
    {
        return Err(invalid());
    }
    Ok(())
}

// Root's /run ancestry is part of the existing trusted shell VM domain. Pin
// the fresh private child with O_NOFOLLOW, then open only its literal payload
// leaf relative to that descriptor. Reading performs no enrollment, identity,
// cwd or environment mutation. Cleanup belongs to the host session owner.
fn read_shell_payload_file(path: &Path) -> io::Result<Payload> {
    use rustix::fs::{Mode, OFlags, open};
    use std::{fs::File, os::unix::fs::MetadataExt};

    let text = path.to_str().ok_or_else(invalid)?;
    let nonce = text
        .strip_prefix(SHELL_PAYLOAD_DIRECTORY_PREFIX)
        .and_then(|text| text.strip_suffix("/payload"))
        .ok_or_else(invalid)?;
    if nonce.len() != 32
        || !nonce
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid());
    }
    let directory: File = open(
        path.parent().ok_or_else(invalid)?,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?
    .into();
    let parent = directory.metadata()?;
    if !parent.is_dir() || parent.uid() != 0 || parent.mode() & 0o7777 != 0o700 {
        return Err(invalid());
    }
    let file = open_private_payload(&directory, Path::new("payload"), PayloadFilePolicy::Shell)?;
    Payload::read(file)
}

/// The trusted producer owns the source; the container workload UID is not
/// necessarily that owner (local worker is root, stock Cloud worker is not).
#[derive(Clone, Copy)]
pub enum PayloadFilePolicy {
    Shell,
    Worker { owner: u32 },
}

/// Open a bounded immutable regular payload without following a leaf link or
/// blocking on a FIFO. The caller pins/validates ancestry or uses a trusted
/// runtime bind target. This does not grant authority to an arbitrary path.
///
/// # Errors
/// Rejects nonregular, aliased, oversized, wrong-owner/mode files before read.
pub fn open_private_payload(
    directory: impl std::os::fd::AsFd,
    path: &Path,
    policy: PayloadFilePolicy,
) -> io::Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags, openat};
    use std::os::unix::fs::MetadataExt;
    let file: std::fs::File = openat(
        directory,
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )?
    .into();
    let metadata = file.metadata()?;
    let (owner, mode) = match policy {
        PayloadFilePolicy::Shell => (0, 0o400),
        PayloadFilePolicy::Worker { owner } => (owner, 0o444),
    };
    if !metadata.is_file()
        || metadata.uid() != owner
        || metadata.nlink() != 1
        || metadata.mode() & 0o7777 != mode
        || metadata.len() > MAX_PAYLOAD_BYTES as u64
    {
        return Err(invalid());
    }
    Ok(file)
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(char::from(HEX[(byte >> 4) as usize]));
        result.push(char::from(HEX[(byte & 15) as usize]));
    }
    result
}

fn put_length(output: &mut Vec<u8>, length: usize) -> io::Result<()> {
    output.extend(u32::try_from(length).map_err(|_| invalid())?.to_be_bytes());
    Ok(())
}
fn put_blob(output: &mut Vec<u8>, bytes: &[u8]) -> io::Result<()> {
    put_length(output, bytes.len())?;
    output.extend(bytes);
    Ok(())
}
struct Input<'a>(&'a [u8]);
impl<'a> Input<'a> {
    fn length(&mut self, limit: usize) -> io::Result<usize> {
        let bytes: [u8; 4] = self
            .0
            .get(..4)
            .ok_or_else(invalid)?
            .try_into()
            .map_err(|_| invalid())?;
        self.0 = &self.0[4..];
        let length = u32::from_be_bytes(bytes) as usize;
        if length > limit {
            return Err(invalid());
        }
        Ok(length)
    }
    fn blob(&mut self, limit: usize) -> io::Result<&'a [u8]> {
        let length = self.length(limit)?;
        let bytes = self.0.get(..length).ok_or_else(invalid)?;
        self.0 = &self.0[length..];
        Ok(bytes)
    }
}

/// Failure stage is structural: only a native exec failure gets 126/127.
/// Payload, cwd and authority preparation failures remain infrastructure 125.
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("native byte launch failed")]
    Preparation(#[from] io::Error),
    #[error("native byte exec failed (errno {0})")]
    Execute(nix::errno::Errno),
}

impl ExecError {
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::Preparation(_) => 125,
            Self::Execute(nix::errno::Errno::ENOENT | nix::errno::Errno::ENOTDIR) => 127,
            Self::Execute(_) => 126,
        }
    }
}

/// Replace this process with the payload command. No fork, interpreter fallback,
/// stdout/stderr framing, signal disposition, tty, UID or resource changes.
/// Image and trusted Docker environment are inherited, then validated exports
/// override them. PATH search uses the resulting raw PATH; empty elements mean
/// the current raw cwd (execvp-style). There is no Go LookPath/ErrDot policy.
///
/// # Errors
/// Returns a fixed diagnostic if validation/chdir/exec fails. An ENOEXEC image
/// command is NOT fed to /bin/sh (unlike libc `execvp`/`CommandExt::exec` fallback).
pub fn exec(payload: Payload) -> Result<std::convert::Infallible, ExecError> {
    use nix::errno::Errno;
    use std::os::unix::fs::MetadataExt;
    payload.validate()?;
    let helper = std::fs::metadata(std::env::current_exe()?)
        .map_err(|_| io::Error::other("native byte executable identity unavailable"))?;
    let helper_identity = (helper.dev(), helper.ino());
    let argv: Vec<CString> = payload
        .argv
        .iter()
        .map(|word| CString::new(word.clone()).map_err(|_| invalid()))
        .collect::<Result<_, _>>()?;
    // Keep the native image environment in its original order (including any
    // duplicate static names). Only explicitly exported names are replaced.
    let mut environment: Vec<(Vec<u8>, Vec<u8>)> = std::env::vars_os()
        .filter(|(name, _)| {
            !payload
                .environment
                .keys()
                .any(|export| export.as_bytes() == name.as_bytes())
        })
        .map(|(name, value)| (name.into_vec(), value.into_vec()))
        .collect();
    environment.extend(
        payload
            .environment
            .into_iter()
            .map(|(name, value)| (name.into_bytes(), value)),
    );
    let path = environment
        .iter()
        .find(|(name, _)| name == b"PATH")
        .map_or_else(|| b"/bin:/usr/bin".to_vec(), |(_, value)| value.clone());
    let env: Vec<CString> = environment
        .into_iter()
        .map(|(mut name, value)| {
            name.push(b'=');
            name.extend(value);
            CString::new(name).map_err(|_| invalid())
        })
        .collect::<Result<_, _>>()?;
    if !payload.working_directory.is_empty() {
        std::env::set_current_dir(OsString::from_vec(payload.working_directory))
            .map_err(|_| io::Error::other("native byte launch working directory unavailable"))?;
    }
    // Rust startup ignores SIGPIPE. A direct execve would otherwise leak that
    // implementation detail into the image program (yes|head exits 1, not 141).
    // Install a safe caught disposition: execve itself resets caught handlers
    // to SIG_DFL, atomically with replacing this helper. No thread/fork/FD or
    // unsafe application signal handler is introduced, and no shell is used.
    signal_hook::flag::register(
        signal_hook::consts::SIGPIPE,
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .map_err(|_| io::Error::other("native byte signal preparation failed"))?;
    let command = argv[0].as_bytes();
    if command.contains(&b'/') {
        return image_execve(&argv[0], &argv, &env, helper_identity).map_err(exec_error);
    }
    let mut denied = false;
    for directory in path.split(|byte| *byte == b':') {
        let mut candidate = directory.to_vec();
        if !candidate.is_empty() {
            candidate.push(b'/');
        }
        candidate.extend(command);
        let candidate = CString::new(candidate).map_err(|_| invalid())?;
        match image_execve(&candidate, &argv, &env, helper_identity) {
            Ok(never) => return Ok(never),
            Err(Errno::EACCES) => denied = true,
            Err(Errno::ENOENT | Errno::ENOTDIR) => {}
            Err(error) => return Err(exec_error(error)),
        }
    }
    Err(exec_error(if denied {
        Errno::EACCES
    } else {
        Errno::ENOENT
    }))
}

// An injected launcher is transport infrastructure, never the image command.
// Follow PATH/symlink aliases for this identity check, without requiring read
// permission on an execute-only image program. No argv rendering or fallback.
fn image_execve(
    program: &std::ffi::CStr,
    argv: &[CString],
    environment: &[CString],
    helper: (u64, u64),
) -> Result<std::convert::Infallible, nix::errno::Errno> {
    use std::os::unix::fs::MetadataExt;
    let metadata =
        std::fs::metadata(std::ffi::OsStr::from_bytes(program.to_bytes())).map_err(|error| {
            nix::errno::Errno::from_raw(error.raw_os_error().unwrap_or(nix::libc::EIO))
        })?;
    if (metadata.dev(), metadata.ino()) == helper {
        return Err(nix::errno::Errno::EPERM);
    }
    nix::unistd::execve(program, argv, environment)
}

fn exec_error(error: nix::errno::Errno) -> ExecError {
    ExecError::Execute(error)
}
