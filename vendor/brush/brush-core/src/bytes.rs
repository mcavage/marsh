//! Lossless text for byte strings.
//!
//! Shell words, values and input are arbitrary bytes, but the shell holds
//! them as Rust strings. At every boundary where bytes enter the shell
//! (arguments, scripts, input, command output, the environment, file names)
//! they are decoded with [`decode`]: valid UTF-8 stays as it is, and each
//! byte that is not part of valid UTF-8 becomes one character from the end
//! of Unicode's last private-use plane, `U+10FF80` (byte `0x80`) to
//! `U+10FFFF` (byte `0xFF`). Where text leaves the shell as bytes (command
//! arguments and environment, file names, output) [`encode`] turns those
//! characters back into their bytes. Valid input that already contains one
//! of those characters is decoded byte by byte, so `encode(decode(b)) == b`
//! for every byte string `b`.
//!
//! Inside the shell a raw byte is one character: it matches `?`, counts one
//! in `${#v}`, and is left alone by case conversion, as in Bash.

use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

const RAW_FIRST: u32 = 0x10_FF80;

/// The character standing for `byte` (0x80 through 0xFF) in decoded text.
pub fn raw_char(byte: u8) -> char {
    char::from_u32(RAW_FIRST - 0x80 + u32::from(byte)).unwrap_or(char::REPLACEMENT_CHARACTER)
}

/// The byte a decoded character stands for, if it is one.
pub fn raw_byte(c: char) -> Option<u8> {
    let code = u32::from(c);
    (code >= RAW_FIRST)
        .then(|| u8::try_from(code - RAW_FIRST + 0x80).ok())
        .flatten()
}

/// Whether `bytes` is valid UTF-8 holding no character from the raw range.
fn is_plain(bytes: &[u8]) -> bool {
    // The raw characters are U+10FF80..=U+10FFFF: F4 8F BE/BF xx in UTF-8.
    std::str::from_utf8(bytes).is_ok()
        && !bytes
            .windows(3)
            .any(|w| w[0] == 0xF4 && w[1] == 0x8F && w[2] >= 0xBE)
}

/// Decodes bytes as shell text (see the module documentation).
pub fn decode(bytes: &[u8]) -> Cow<'_, str> {
    if is_plain(bytes) {
        // SAFETY-free: is_plain checked UTF-8 validity.
        return Cow::Borrowed(std::str::from_utf8(bytes).unwrap_or_default());
    }
    let mut text = String::with_capacity(bytes.len() + 8);
    let mut rest = bytes;
    while !rest.is_empty() {
        let (valid, invalid) = match std::str::from_utf8(rest) {
            Ok(valid) => (valid, &[][..]),
            Err(error) => {
                let (valid, after) = rest.split_at(error.valid_up_to());
                let bad = error.error_len().unwrap_or(after.len());
                (
                    std::str::from_utf8(valid).unwrap_or_default(),
                    &after[..bad],
                )
            }
        };
        for c in valid.chars() {
            if raw_byte(c).is_some() {
                let mut buffer = [0; 4];
                text.extend(c.encode_utf8(&mut buffer).bytes().map(raw_char));
            } else {
                text.push(c);
            }
        }
        text.extend(invalid.iter().copied().map(raw_char));
        rest = &rest[valid.len() + invalid.len()..];
    }
    Cow::Owned(text)
}

/// Decodes owned bytes as shell text.
pub fn decode_vec(bytes: Vec<u8>) -> String {
    match decode(&bytes) {
        Cow::Borrowed(_) => String::from_utf8(bytes).unwrap_or_default(),
        Cow::Owned(text) => text,
    }
}

/// Encodes shell text as the bytes it stands for.
pub fn encode(text: &str) -> Cow<'_, [u8]> {
    if !text.chars().any(|c| raw_byte(c).is_some()) {
        return Cow::Borrowed(text.as_bytes());
    }
    let mut bytes = Vec::with_capacity(text.len());
    for c in text.chars() {
        match raw_byte(c) {
            Some(byte) => bytes.push(byte),
            None => bytes.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
        }
    }
    Cow::Owned(bytes)
}

/// Shell text as an OS string (an argument, environment entry or path).
pub fn to_os_string(text: &str) -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt as _;
        OsString::from_vec(encode(text).into_owned())
    }
    #[cfg(not(unix))]
    {
        OsString::from(text)
    }
}

/// An OS string as shell text.
pub fn from_os_str(os: &OsStr) -> Cow<'_, str> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        decode(os.as_bytes())
    }
    #[cfg(not(unix))]
    {
        os.to_string_lossy()
    }
}

/// Shell text as a path.
pub fn to_path(text: &str) -> PathBuf {
    PathBuf::from(to_os_string(text))
}

/// A path as shell text.
pub fn from_path(path: &Path) -> Cow<'_, str> {
    from_os_str(path.as_os_str())
}

/// A path the shell built from text: one holding raw characters names the
/// bytes they stand for.
pub fn shell_path(path: &Path) -> Cow<'_, Path> {
    match path.to_str() {
        Some(text) if text.chars().any(|c| raw_byte(c).is_some()) => Cow::Owned(to_path(text)),
        _ => Cow::Borrowed(path),
    }
}

/// Paths and OS strings as shell text (in place of `to_string_lossy`).
pub trait ToShellText {
    /// Decodes the bytes as shell text, losslessly.
    fn to_shell_text(&self) -> Cow<'_, str>;
}

impl ToShellText for OsStr {
    fn to_shell_text(&self) -> Cow<'_, str> {
        from_os_str(self)
    }
}

impl ToShellText for Path {
    fn to_shell_text(&self) -> Cow<'_, str> {
        from_os_str(self.as_os_str())
    }
}

/// The number of bytes `text` stands for.
pub fn byte_len(text: &str) -> usize {
    if !text.as_bytes().contains(&0xF4) {
        return text.len();
    }
    text.chars()
        .map(|c| {
            if raw_byte(c).is_some() {
                1
            } else {
                c.len_utf8()
            }
        })
        .sum()
}

/// Translates written shell text to the bytes it stands for: each UTF-8
/// encoded raw character in `buf` becomes its byte.
pub fn encode_output(buf: &[u8]) -> Cow<'_, [u8]> {
    if !buf.contains(&0xF4) {
        return Cow::Borrowed(buf);
    }
    let mut out = Vec::with_capacity(buf.len());
    let mut index = 0;
    while index < buf.len() {
        if buf[index] == 0xF4
            && let Some(&[_, second, third, fourth]) = buf.get(index..index + 4)
            && second == 0x8F
            && third >= 0xBE
            && (0x80..0xC0).contains(&fourth)
        {
            out.push(((third & 0x01) << 6) | (fourth & 0x3F) | 0x80);
            index += 4;
        } else {
            out.push(buf[index]);
            index += 1;
        }
    }
    Cow::Owned(out)
}
