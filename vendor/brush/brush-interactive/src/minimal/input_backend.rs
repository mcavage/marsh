use std::io::{IsTerminal, Write};
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

use crate::{
    InputBackend, ShellError,
    input_backend::{InteractivePrompt, ReadResult},
};

/// Represents a minimal shell input backend, capable of taking commands from standard input.
#[derive(Default)]
pub struct MinimalInputBackend {
    async_stdin: Option<tokio::io::BufReader<tokio::io::Stdin>>,
    pending: PendingInput,
}

/// Input consumed from the reader but not yet returned: a partial line, and the
/// complete lines of a program that isn't complete yet.
#[derive(Default)]
struct PendingInput {
    line: Vec<u8>,
    program: String,
}

impl InputBackend for MinimalInputBackend {
    async fn read_line(
        &mut self,
        shell_ref: &crate::ShellRef<impl brush_core::ShellExtensions>,
        prompt: InteractivePrompt,
    ) -> Result<ReadResult, ShellError> {
        // An interactive shell prompts even when its input is not a terminal.
        let display_prompt =
            std::io::stdin().is_terminal() || shell_ref.lock().await.options().interactive;
        let reader = self
            .async_stdin
            .get_or_insert_with(|| tokio::io::BufReader::new(tokio::io::stdin()));

        Self::read_program_from(
            shell_ref,
            display_prompt.then_some(&prompt),
            reader,
            &mut std::io::stderr(),
            &mut self.pending,
        )
        .await
    }
}

impl MinimalInputBackend {
    /// Reads lines from the given reader until they form a complete shell program.
    /// Compound commands, here documents, and backslash continuations all span lines,
    /// so a single line is often not a program on its own. Input consumed is kept in
    /// `pending` until it's returned, so cancelling the read (as when a `TERM` trap
    /// must run while input is idle) loses none of it.
    ///
    /// # Arguments
    ///
    /// * `shell_ref` - The shell whose parsing options decide when input is complete.
    /// * `prompt` - The prompt to display, or `None` to display nothing.
    /// * `reader` - The source to read lines from.
    /// * `writer` - Where prompts are written.
    /// * `pending` - Input already consumed from `reader`.
    async fn read_program_from<R: AsyncBufRead + Unpin, W: Write>(
        shell_ref: &crate::ShellRef<impl brush_core::ShellExtensions>,
        prompt: Option<&InteractivePrompt>,
        reader: &mut R,
        writer: &mut W,
        pending: &mut PendingInput,
    ) -> Result<ReadResult, ShellError> {
        loop {
            if let Some(prompt) = prompt
                && pending.line.is_empty()
            {
                let to_display = if pending.program.is_empty() {
                    &prompt.prompt
                } else {
                    &prompt.continuation_prompt
                };

                writer.write_all(&brush_core::bytes::encode(to_display))?;
                writer.flush()?;
            }

            // `read_line` is not cancellation safe: a TERM trap could discard
            // bytes already consumed into its local String. Keep each partial
            // line in `pending` before consuming the reader instead.
            let available = reader.fill_buf().await.map_err(ShellError::InputError)?;
            // Nothing more is coming; run what we have and let the parser complain if
            // it's still incomplete.
            if available.is_empty() {
                break;
            }
            let count = available
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(available.len(), |index| index + 1);
            let line_complete = available[count - 1] == b'\n';
            // Bash ignores NUL bytes in its input.
            pending
                .line
                .extend(available[..count].iter().filter(|byte| **byte != 0));
            reader.consume(count);
            if !line_complete {
                continue;
            }

            let line = brush_core::bytes::decode_vec(std::mem::take(&mut pending.line));
            pending.program.push_str(&line);
            if !crate::completeness::needs_more_input(shell_ref, &pending.program) {
                break;
            }
        }

        if !pending.line.is_empty() {
            let line = brush_core::bytes::decode_vec(std::mem::take(&mut pending.line));
            pending.program.push_str(&line);
        }

        if pending.program.is_empty() {
            Ok(ReadResult::Eof)
        } else {
            Ok(ReadResult::Input(std::mem::take(&mut pending.program)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Cursor, sync::Arc};

    async fn test_shell_ref() -> crate::ShellRef {
        Arc::new(tokio::sync::Mutex::new(
            brush_core::Shell::builder().build().await.unwrap(),
        ))
    }

    fn test_prompt() -> InteractivePrompt {
        InteractivePrompt {
            prompt: String::from("$ "),
            alt_side_prompt: String::new(),
            continuation_prompt: String::from("> "),
        }
    }

    /// Reads one program from `input`, returning what the backend accumulated.
    async fn read_program(input: &str) -> ReadResult {
        let shell_ref = test_shell_ref().await;
        let mut reader = Cursor::new(input.as_bytes());

        MinimalInputBackend::read_program_from(
            &shell_ref,
            None,
            &mut reader,
            &mut Vec::new(),
            &mut PendingInput::default(),
        )
        .await
        .unwrap()
    }

    async fn assert_reads(input: &str, expected: &str) {
        let actual = match read_program(input).await {
            ReadResult::Input(s) => format!("Input({s:?})"),
            ReadResult::BoundCommand(s) => format!("BoundCommand({s:?})"),
            ReadResult::Eof => String::from("Eof"),
            ReadResult::Interrupted => String::from("Interrupted"),
        };

        assert_eq!(
            actual,
            format!("Input({expected:?})"),
            "for input {input:?}"
        );
    }

    #[tokio::test]
    async fn stops_at_a_complete_program() {
        assert_reads("echo one\necho two\n", "echo one\n").await;
    }

    #[tokio::test]
    async fn accumulates_until_complete() {
        assert_reads("true &&\necho yes\n", "true &&\necho yes\n").await;
        assert_reads(
            "if true\nthen\necho yes\nfi\necho after\n",
            "if true\nthen\necho yes\nfi\n",
        )
        .await;
        assert_reads("cat <<EOF\nbody\nEOF\n", "cat <<EOF\nbody\nEOF\n").await;
        assert_reads("echo a\\\nb\n", "echo a\\\nb\n").await;
    }

    #[tokio::test]
    async fn returns_incomplete_input_at_end_of_input() {
        // Truncated mid-construct: hand it over anyway so the parser reports the error.
        assert_reads("if true\nthen\n", "if true\nthen\n").await;
    }

    #[tokio::test]
    async fn reports_end_of_input_when_there_is_nothing_to_read() {
        assert!(matches!(read_program("").await, ReadResult::Eof));
    }

    #[tokio::test]
    async fn displays_a_continuation_prompt_for_each_extra_line() {
        let shell_ref = test_shell_ref().await;
        let prompt = test_prompt();
        let mut prompts = Vec::new();

        MinimalInputBackend::read_program_from(
            &shell_ref,
            Some(&prompt),
            &mut Cursor::new(b"if true\nthen\necho hi\nfi\n".as_slice()),
            &mut prompts,
            &mut PendingInput::default(),
        )
        .await
        .unwrap();

        assert_eq!(String::from_utf8(prompts).unwrap(), "$ > > > ");
    }

    #[tokio::test]
    async fn displays_no_prompts_without_one_to_display() {
        let shell_ref = test_shell_ref().await;
        let mut prompts = Vec::new();

        MinimalInputBackend::read_program_from(
            &shell_ref,
            None,
            &mut Cursor::new(b"if true\nthen\necho hi\nfi\n".as_slice()),
            &mut prompts,
            &mut PendingInput::default(),
        )
        .await
        .unwrap();

        assert_eq!(prompts, Vec::<u8>::new());
    }
}
