//! Text that comes from a services VM is hostile: it is read in bounded
//! pieces and made plain before it is kept, logged or shown.

use std::io::BufRead;

/// The most bytes of one log line kept for display.
pub const DISPLAY_LINE_BYTES: usize = 1024;

/// Lines of VM output, each at most `cap` bytes: the rest of a longer line is
/// read and thrown away, so memory stays at the reader's buffer and one line's
/// cap however much the VM writes without a newline. Ends at end of input or
/// the first read error.
pub struct CappedLines<R> {
    reader: R,
    cap: usize,
}

impl<R: BufRead> CappedLines<R> {
    pub fn new(reader: R, cap: usize) -> Self {
        Self { reader, cap }
    }
}

impl<R: BufRead> Iterator for CappedLines<R> {
    type Item = String;

    fn next(&mut self) -> Option<String> {
        let mut line = Vec::new();
        let mut any = false;
        loop {
            let available = match self.reader.fill_buf() {
                Ok(available) => available,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            if available.is_empty() {
                break;
            }
            any = true;
            let (chunk, used, done) = match available.iter().position(|byte| *byte == b'\n') {
                Some(end) => (&available[..end], end + 1, true),
                None => (available, available.len(), false),
            };
            let room = self.cap.saturating_sub(line.len());
            line.extend_from_slice(&chunk[..chunk.len().min(room)]);
            self.reader.consume(used);
            if done {
                break;
            }
        }
        any.then(|| {
            String::from_utf8_lossy(&line)
                .trim_end_matches('\r')
                .to_owned()
        })
    }
}

/// `text` as one plain line of at most `max_bytes`: control and escape
/// characters (and the characters that reorder text) are dropped, so VM output
/// cannot repaint a terminal or hide what follows it.
#[must_use]
pub fn plain_text(text: &str, max_bytes: usize) -> String {
    let mut out = String::new();
    for character in text.chars() {
        if character.is_control()
            || matches!(character, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
        {
            continue;
        }
        if out.len() + character.len_utf8() > max_bytes {
            break;
        }
        out.push(character);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Read};

    /// A reader that makes `total` bytes of one endless line, and counts them.
    struct Endless {
        left: usize,
    }

    impl Read for Endless {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = buf.len().min(self.left);
            buf[..n].fill(b'A');
            self.left -= n;
            Ok(n)
        }
    }

    #[test]
    fn a_line_without_a_newline_is_cut_and_the_rest_discarded() {
        let mut lines = CappedLines::new(BufReader::new(Endless { left: 64 << 20 }), 1024);
        let first = lines.next().unwrap();
        assert_eq!(first.len(), 1024);
        assert!(lines.next().is_none());
    }

    #[test]
    fn lines_after_a_long_one_survive_and_short_ones_are_whole() {
        let mut input = Vec::new();
        input.extend_from_slice(b"one\r\n");
        input.extend(std::iter::repeat_n(b'x', 10_000));
        input.extend_from_slice(b"\ntwo\nlast without newline");
        let lines: Vec<String> = CappedLines::new(BufReader::new(&input[..]), 16).collect();
        assert_eq!(
            lines,
            ["one", "xxxxxxxxxxxxxxxx", "two", "last without new"]
        );
    }

    #[test]
    fn plain_text_drops_escapes_and_caps() {
        assert_eq!(
            plain_text("a\u{1b}[31mred\u{7}\u{202E}b\t", 100),
            "a[31mredb"
        );
        assert_eq!(plain_text("abcdef", 3), "abc");
        assert_eq!(plain_text("\u{e9}\u{e9}", 3), "\u{e9}");
    }
}
