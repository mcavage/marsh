use crate::{ReadResult, ShellError};

pub(crate) struct NonTermLineReader;

impl super::LineReader for NonTermLineReader {
    fn read_line(
        &self,
        _prompt: Option<&str>,
        _completion_handler: impl FnMut(
            &str,
            usize,
        ) -> Result<
            brush_core::completion::Completions,
            crate::ShellError,
        >,
    ) -> Result<crate::ReadResult, crate::ShellError> {
        let mut input = Vec::new();
        let bytes_read =
            std::io::BufRead::read_until(&mut std::io::stdin().lock(), b'\n', &mut input)
                .map_err(ShellError::InputError)?;
        // Bash ignores NUL bytes in its input.
        input.retain(|byte| *byte != 0);

        if bytes_read == 0 {
            Ok(ReadResult::Eof)
        } else {
            Ok(ReadResult::Input(brush_core::bytes::decode_vec(input)))
        }
    }
}
