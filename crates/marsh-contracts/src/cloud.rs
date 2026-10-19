//! Cloud placement transfer contracts: a bounded snapshot/delta archive and
//! the remote execution request (`docs/design/cloud-placement.md` s4).
//!
//! The archive is a private, versioned framing shared by the host daemon and
//! the remote `marsh-local` helper. It carries relative paths only. Readers
//! validate every path before a caller may act on it; validation is not a
//! filesystem authority check, which remains the receiver's job.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{self, Read, Write};

const MAGIC: &[u8; 4] = b"MCA2";
/// The last record: `E`, then the number of entries before it (u64). A
/// stream that ends anywhere else is truncated, never a complete archive.
const END: u8 = b'E';
/// Longest relative path, one component, and per-file content.
pub const MAX_PATH: usize = 4096;
pub const MAX_COMPONENT: usize = 255;
pub const MAX_FILE: u64 = 256 << 20;
/// Root-owned helper path inside a cloud sandbox, installed by `prepare`.
pub const INSTALLED_HELPER: &str = "/usr/local/lib/marsh-cloud/marsh-local";
/// Snapshot uploads and returned deltas.
pub const MAX_ARCHIVE: u64 = 1 << 30;
/// Request JSON carried in a snapshot.
pub const MAX_REQUEST: u64 = 1 << 20;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Directory,
    File,
    Symlink,
    Delete,
    Request,
}

impl Kind {
    const fn byte(self) -> u8 {
        match self {
            Self::Directory => b'D',
            Self::File => b'F',
            Self::Symlink => b'L',
            Self::Delete => b'X',
            Self::Request => b'R',
        }
    }

    fn from_byte(byte: u8) -> Option<Self> {
        Some(match byte {
            b'D' => Self::Directory,
            b'F' => Self::File,
            b'L' => Self::Symlink,
            b'X' => Self::Delete,
            b'R' => Self::Request,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Header {
    pub kind: Kind,
    /// Validated relative path bytes (empty only for [`Kind::Request`]).
    pub path: Vec<u8>,
    /// Permission bits only (`0o777`); never set-id or sticky bits.
    pub mode: u32,
    pub len: u64,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// A relative path of plain components: no empty, `.`, `..`, NUL, or
/// absolute form, within the component and total bounds.
#[must_use]
pub fn valid_relative(path: &[u8]) -> bool {
    !path.is_empty()
        && path.len() <= MAX_PATH
        && !path.contains(&0)
        && path.first() != Some(&b'/')
        && path.split(|byte| *byte == b'/').all(|component| {
            !component.is_empty()
                && component.len() <= MAX_COMPONENT
                && component != b"."
                && component != b".."
        })
}

/// Paths a snapshot never carries and a returned delta never writes: any
/// `.git` component (the caller's or a nested repository's metadata) and the
/// top-level `.marsh` split state. Case-insensitive, for macOS volumes.
#[must_use]
pub fn protected(path: &[u8]) -> bool {
    let mut components = path.split(|byte| *byte == b'/');
    components
        .next()
        .is_some_and(|first| first.eq_ignore_ascii_case(b".marsh"))
        || path
            .split(|byte| *byte == b'/')
            .any(|component| component.eq_ignore_ascii_case(b".git"))
}

pub struct Writer<W: Write> {
    inner: W,
    written: u64,
    limit: u64,
    entries: u64,
}

impl<W: Write> Writer<W> {
    /// # Errors
    /// Returns the underlying write error.
    pub fn new(mut inner: W, limit: u64) -> io::Result<Self> {
        inner.write_all(MAGIC)?;
        Ok(Self {
            inner,
            written: 4,
            limit,
            entries: 0,
        })
    }

    fn header(&mut self, kind: Kind, path: &[u8], mode: u32, len: u64) -> io::Result<()> {
        if (kind == Kind::Request) != path.is_empty()
            || (kind != Kind::Request && !valid_relative(path))
        {
            return Err(invalid("invalid archive path"));
        }
        let size = 1 + 4 + path.len() as u64 + 4 + 8 + len;
        self.written = self.written.saturating_add(size);
        if self.written > self.limit {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "archive exceeds its byte limit",
            ));
        }
        self.entries += 1;
        self.inner.write_all(&[kind.byte()])?;
        self.inner
            .write_all(&u32::try_from(path.len()).unwrap_or(u32::MAX).to_be_bytes())?;
        self.inner.write_all(path)?;
        self.inner.write_all(&(mode & 0o777).to_be_bytes())?;
        self.inner.write_all(&len.to_be_bytes())
    }

    /// # Errors
    /// Rejects invalid paths and limit overflow.
    pub fn directory(&mut self, path: &[u8], mode: u32) -> io::Result<()> {
        self.header(Kind::Directory, path, mode, 0)
    }

    /// # Errors
    /// Rejects invalid paths, oversized files, a short source, and limit overflow.
    pub fn file(
        &mut self,
        path: &[u8],
        mode: u32,
        len: u64,
        data: &mut dyn Read,
    ) -> io::Result<()> {
        if len > MAX_FILE {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "file exceeds the cloud transfer limit",
            ));
        }
        self.header(Kind::File, path, mode, len)?;
        let copied = io::copy(&mut data.take(len), &mut self.inner)?;
        if copied != len {
            return Err(invalid("file changed size while it was archived"));
        }
        Ok(())
    }

    /// # Errors
    /// Rejects invalid paths and limit overflow.
    pub fn symlink(&mut self, path: &[u8], target: &[u8]) -> io::Result<()> {
        if target.is_empty() || target.len() > MAX_PATH || target.contains(&0) {
            return Err(invalid("invalid symlink target"));
        }
        self.header(Kind::Symlink, path, 0o777, target.len() as u64)?;
        self.inner.write_all(target)
    }

    /// # Errors
    /// Rejects invalid paths and limit overflow.
    pub fn delete(&mut self, path: &[u8]) -> io::Result<()> {
        self.header(Kind::Delete, path, 0, 0)
    }

    /// # Errors
    /// Rejects oversized requests and limit overflow.
    pub fn request(&mut self, request: &CloudRequest) -> io::Result<()> {
        let bytes = serde_json::to_vec(request).map_err(io::Error::other)?;
        if bytes.len() as u64 > MAX_REQUEST {
            return Err(invalid("cloud request exceeds its byte limit"));
        }
        self.header(Kind::Request, b"", 0, bytes.len() as u64)?;
        self.inner.write_all(&bytes)
    }

    /// # Errors
    /// Returns the underlying flush error.
    pub fn finish(mut self) -> io::Result<W> {
        self.inner.write_all(&[END])?;
        self.inner.write_all(&self.entries.to_be_bytes())?;
        self.inner.flush()?;
        Ok(self.inner)
    }
}

pub struct Reader<R: Read> {
    inner: R,
    entries: u64,
    ended: bool,
    pending: u64,
    read: u64,
    limit: u64,
}

impl<R: Read> Reader<R> {
    /// # Errors
    /// Rejects a missing or unknown archive version.
    pub fn new(mut inner: R, limit: u64) -> io::Result<Self> {
        let mut magic = [0_u8; 4];
        inner.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(invalid("not a marsh cloud archive"));
        }
        Ok(Self {
            inner,
            entries: 0,
            ended: false,
            pending: 0,
            read: 4,
            limit,
        })
    }

    fn account(&mut self, bytes: u64) -> io::Result<()> {
        self.read = self.read.saturating_add(bytes);
        if self.read > self.limit {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "archive exceeds its byte limit",
            ));
        }
        Ok(())
    }

    /// The next validated header, or `None` at a clean end. Unread data of
    /// the previous entry is skipped.
    ///
    /// # Errors
    /// Rejects truncated, oversized, or invalid entries.
    pub fn next_header(&mut self) -> io::Result<Option<Header>> {
        if self.pending > 0 {
            let pending = self.pending;
            self.pending = 0;
            let skipped = io::copy(&mut (&mut self.inner).take(pending), &mut io::sink())?;
            if skipped != pending {
                return Err(invalid("truncated archive entry"));
            }
        }
        if self.ended {
            return Ok(None);
        }
        let mut kind = [0_u8; 1];
        self.inner
            .read_exact(&mut kind)
            .map_err(|_| invalid("truncated archive: no end record"))?;
        if kind[0] == END {
            let mut count = [0_u8; 8];
            self.inner
                .read_exact(&mut count)
                .map_err(|_| invalid("truncated archive end record"))?;
            if u64::from_be_bytes(count) != self.entries {
                return Err(invalid("archive entry count mismatch"));
            }
            let mut extra = [0_u8; 1];
            if self.inner.read(&mut extra)? != 0 {
                return Err(invalid("data after the archive end record"));
            }
            self.ended = true;
            return Ok(None);
        }
        self.entries += 1;
        let kind = Kind::from_byte(kind[0]).ok_or_else(|| invalid("unknown archive entry"))?;
        let mut word = [0_u8; 4];
        self.inner.read_exact(&mut word)?;
        let path_len = u32::from_be_bytes(word) as usize;
        if path_len > MAX_PATH {
            return Err(invalid("archive path exceeds its bound"));
        }
        let mut path = vec![0_u8; path_len];
        self.inner.read_exact(&mut path)?;
        self.inner.read_exact(&mut word)?;
        let mode = u32::from_be_bytes(word) & 0o777;
        let mut long = [0_u8; 8];
        self.inner.read_exact(&mut long)?;
        let len = u64::from_be_bytes(long);
        self.account(1 + 4 + path_len as u64 + 4 + 8)?;
        let valid = match kind {
            Kind::Request => path.is_empty() && len <= MAX_REQUEST,
            Kind::Directory | Kind::Delete => valid_relative(&path) && len == 0,
            Kind::Symlink => valid_relative(&path) && len > 0 && len <= MAX_PATH as u64,
            Kind::File => valid_relative(&path) && len <= MAX_FILE,
        };
        if !valid {
            return Err(invalid("invalid archive entry"));
        }
        self.account(len)?;
        self.pending = len;
        Ok(Some(Header {
            kind,
            path,
            mode,
            len,
        }))
    }

    /// Copy the current entry's data, exactly once.
    ///
    /// # Errors
    /// Rejects truncated data and propagates the writer's error.
    pub fn copy_data(&mut self, out: &mut dyn Write) -> io::Result<u64> {
        let pending = self.pending;
        self.pending = 0;
        let copied = io::copy(&mut (&mut self.inner).take(pending), out)?;
        if copied != pending {
            return Err(invalid("truncated archive entry"));
        }
        Ok(copied)
    }

    /// The current entry's data in memory (symlink targets and requests).
    ///
    /// # Errors
    /// As [`Self::copy_data`].
    pub fn data(&mut self) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.copy_data(&mut bytes)?;
        Ok(bytes)
    }
}

/// Frames on both directions of the run carrier. The stock exec channel has
/// no flow control of its own: unread stdin breaks the sandbox's exec relay,
/// and output the host reads too slowly is dropped. So both directions are
/// windowed: the helper acknowledges stdin the workload accepted (`ACK`), the
/// host acknowledges output it delivered (`OUTPUT_ACK`), and `END` marks
/// complete output, so a loss is reported, never silent.
pub mod run_frame {
    use std::io::{self, Read, Write};

    pub const STDOUT: u8 = b'O';
    pub const STDERR: u8 = b'E';
    /// Payload: u64 count of stdin bytes the workload accepted.
    pub const ACK: u8 = b'A';
    /// The workload closed its stdin; send no more.
    pub const CLOSED: u8 = b'C';
    /// All workload output was sent.
    pub const END: u8 = b'X';
    /// Host to helper: stdin bytes, stdin end, and delivered-output count (u64).
    pub const DATA: u8 = b'D';
    pub const EOF: u8 = b'Z';
    pub const OUTPUT_ACK: u8 = b'K';
    /// Host to helper: terminal size, rows then columns (u16 each).
    pub const RESIZE: u8 = b'W';
    /// Registered-command calls from the workload (agent Kits in their own
    /// sandboxes): each payload starts with a u32 connection id.
    /// Helper to host: a call (`LinkRequest` JSON), its stdin, its stdin end.
    pub const KIT_OPEN: u8 = b'P';
    pub const KIT_IN: u8 = b'Q';
    pub const KIT_IN_EOF: u8 = b'R';
    /// Host to helper: the call's stdout, stderr, and exit status (i32).
    pub const KIT_OUT: u8 = b'S';
    pub const KIT_ERR: u8 = b'T';
    pub const KIT_EXIT: u8 = b'U';
    /// Helper to host: a call's reply bytes reached its caller (u64), per call.
    pub const KIT_ACK: u8 = b'V';
    /// Per-call reply window (several calls share the stock channel).
    pub const CALL_WINDOW: u64 = 128 << 10;
    /// Helper to a link client: the call's exit status (i32).
    pub const EXIT: u8 = b'Y';
    pub const MAX_PAYLOAD: usize = 64 * 1024;
    /// Live probes: an output burst of 1 MiB ahead of the CLI's reads was
    /// dropped by the service ("requested output no longer retained"); 512 KiB
    /// was delivered. Both windows stay well below that.
    pub const STDIN_WINDOW: u64 = 256 << 10;
    pub const OUTPUT_WINDOW: u64 = 256 << 10;

    /// # Errors
    /// Returns the writer's error; rejects oversized payloads.
    pub fn write(out: &mut dyn Write, tag: u8, payload: &[u8]) -> io::Result<()> {
        if payload.len() > MAX_PAYLOAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "frame too large",
            ));
        }
        let mut frame = Vec::with_capacity(5 + payload.len());
        frame.push(tag);
        frame.extend_from_slice(&u32::try_from(payload.len()).unwrap_or(0).to_be_bytes());
        frame.extend_from_slice(payload);
        out.write_all(&frame)?;
        out.flush()
    }

    /// The next frame, or `None` at a clean end of stream.
    ///
    /// # Errors
    /// Rejects truncated or malformed frames.
    pub fn read(input: &mut dyn Read) -> io::Result<Option<(u8, Vec<u8>)>> {
        let mut header = [0_u8; 5];
        let mut filled = 0;
        while filled < header.len() {
            match input.read(&mut header[filled..])? {
                0 if filled == 0 => return Ok(None),
                0 => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "truncated frame",
                    ));
                }
                count => filled += count,
            }
        }
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        if len > MAX_PAYLOAD
            || !matches!(
                header[0],
                STDOUT
                    | STDERR
                    | ACK
                    | CLOSED
                    | END
                    | DATA
                    | EOF
                    | OUTPUT_ACK
                    | RESIZE
                    | KIT_OPEN
                    | KIT_IN
                    | KIT_IN_EOF
                    | KIT_OUT
                    | KIT_ERR
                    | KIT_EXIT
                    | KIT_ACK
                    | EXIT
            )
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "malformed frame",
            ));
        }
        let mut payload = vec![0_u8; len];
        input.read_exact(&mut payload)?;
        Ok(Some((header[0], payload)))
    }
}

/// What the remote helper runs, carried inside the snapshot archive so no
/// workload bytes cross the stock CLI's UTF-8 command line.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CloudRequest {
    /// Absolute remote working directory (bytes; the caller's natural path).
    pub cwd: Vec<u8>,
    /// The workspace root (project or split fork), for in-sandbox splits.
    #[serde(default)]
    pub root: Vec<u8>,
    /// `Shell`: argv for Brush (`-c SCRIPT [NAME ARG...]`). `Argv`: a program.
    pub mode: CloudMode,
    pub argv: Vec<Vec<u8>>,
    /// Filtered exported variables, applied over the sandbox defaults.
    pub env: BTreeMap<String, Vec<u8>>,
    /// Run on a pseudo-terminal of this size (rows, columns): an
    /// interactive cloud shell. Output then arrives as one stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<(u16, u16)>,
    /// Registered names the workload may call (cloud-bound agent Kits).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kits: Vec<String>,
    /// The caller's stdin is a terminal (relayed as a pipe; see
    /// [`TERMINAL_STDIN`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stdin_terminal: bool,
}

/// Set in a cloud workload whose stdin relays the caller's terminal: the
/// `DEV:INO` of that stdin pipe. A process whose fd 0 is that pipe treats its
/// stdin as a terminal; any other stdin (a pipe inside the block) is data.
pub const TERMINAL_STDIN: &str = "MARSH_TERMINAL_STDIN";

/// Whether this process's stdin is a terminal, or the cloud pipe that relays
/// the caller's terminal ([`TERMINAL_STDIN`]).
#[must_use]
pub fn stdin_is_terminal() -> bool {
    use std::io::IsTerminal as _;
    use std::os::unix::fs::MetadataExt as _;
    if std::io::stdin().is_terminal() {
        return true;
    }
    let Some(expected) = std::env::var_os(TERMINAL_STDIN) else {
        return false;
    };
    std::fs::metadata("/dev/stdin")
        .is_ok_and(|stdin| expected == format!("{}:{}", stdin.dev(), stdin.ino()).as_str())
}

/// One registered-command call from a cloud workload.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LinkRequest {
    pub command: String,
    pub argv: Vec<Vec<u8>>,
    pub cwd: Vec<u8>,
    pub env: BTreeMap<String, Vec<u8>>,
}

/// The socket and command links a cloud workload calls agents through.
pub const LINK_DIR: &str = "/run/marsh-cloud";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CloudMode {
    Shell,
    Argv,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostile_paths_and_truncation_are_rejected() {
        for path in [
            &b"/etc/passwd"[..],
            b"a/../../b",
            b"./a",
            b"a//b",
            b"a\0b",
            b"",
        ] {
            assert!(!valid_relative(path), "{path:?}");
        }
        assert!(protected(b".GIT/config") && protected(b".marsh") && protected(b"sub/.Git/x"));
        assert!(!protected(b"src/.marsh") && !protected(b"a.git"));
        let mut bytes = Vec::new();
        let mut writer = Writer::new(&mut bytes, MAX_ARCHIVE).unwrap();
        writer.file(b"a/b", 0o4755, 3, &mut &b"xyz"[..]).unwrap();
        writer.finish().unwrap();
        let mut reader = Reader::new(&bytes[..bytes.len() - 10], MAX_ARCHIVE).unwrap();
        let header = reader.next_header().unwrap().unwrap();
        assert_eq!(header.mode, 0o755, "set-id bits never cross");
        assert!(reader.data().is_err(), "truncated content is an error");
        // Cut cleanly at an entry boundary: still not a complete archive.
        let mut reader = Reader::new(&bytes[..bytes.len() - 9], MAX_ARCHIVE).unwrap();
        reader.next_header().unwrap().unwrap();
        assert!(
            reader.next_header().is_err(),
            "missing end record is an error"
        );
        let mut reader = Reader::new(&bytes[..], MAX_ARCHIVE).unwrap();
        reader.next_header().unwrap().unwrap();
        assert!(reader.next_header().unwrap().is_none());
        let mut forged = bytes.clone();
        forged[9..12].copy_from_slice(b"../");
        assert!(
            Reader::new(&forged[..], MAX_ARCHIVE)
                .unwrap()
                .next_header()
                .is_err()
        );
    }
}
