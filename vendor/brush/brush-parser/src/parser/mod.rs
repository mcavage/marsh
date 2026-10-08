use std::path::PathBuf;

use bon::bon;

use crate::ast;
use crate::tokenizer::{Token, TokenEndReason, Tokenizer, TokenizerOptions, Tokens};

mod aliases;
pub mod peg;
#[cfg(feature = "winnow-parser")]
pub mod winnow_str;

/// Parser implementation to use
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Default)]
pub enum ParserImpl {
    /// PEG-based parser (token-based)
    #[default]
    Peg,
    /// Winnow-based parser (string-based, direct)
    #[cfg(feature = "winnow-parser")]
    Winnow,
}

/// Options used to control the behavior of the parser.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct ParserOptions {
    /// Whether marsh-only composition syntax is enabled.
    pub marsh_extensions: bool,
    /// Whether or not to enable extended globbing (a.k.a. `extglob`).
    pub enable_extended_globbing: bool,
    /// Whether or not to enable POSIX compliance mode.
    pub posix_mode: bool,
    /// Whether or not to enable maximal compatibility with the `sh` shell.
    pub sh_mode: bool,
    /// Whether or not to perform tilde expansion for tildes at the start of words.
    pub tilde_expansion_at_word_start: bool,
    /// Whether or not to perform tilde expansion for tildes after colons.
    pub tilde_expansion_after_colon: bool,
    /// Select the parser internal implementation
    pub parser_impl: ParserImpl,
}

impl Default for ParserOptions {
    fn default() -> Self {
        Self {
            marsh_extensions: false,
            enable_extended_globbing: true,
            posix_mode: false,
            sh_mode: false,
            tilde_expansion_at_word_start: true,
            tilde_expansion_after_colon: false,
            parser_impl: ParserImpl::default(),
        }
    }
}

impl ParserOptions {
    /// Returns the tokenizer options implied by these parser options.
    pub const fn tokenizer_options(&self) -> TokenizerOptions {
        TokenizerOptions {
            enable_extended_globbing: self.enable_extended_globbing,
            posix_mode: self.posix_mode,
            sh_mode: self.sh_mode,
        }
    }
}

/// Information about the source of tokens.
#[derive(Clone, Debug, Default)]
#[allow(dead_code)]
pub struct SourceInfo {
    /// The source of the tokens.
    pub source: String,
}

impl From<PathBuf> for SourceInfo {
    fn from(path: PathBuf) -> Self {
        Self {
            source: path.to_string_lossy().to_string(),
        }
    }
}

/// Implements parsing for shell programs.
pub struct Parser<R: std::io::BufRead> {
    /// The reader to use for input
    reader: R,
    /// Parsing options
    options: ParserOptions,
}

#[bon]
impl<R: std::io::BufRead> Parser<R> {
    ///
    /// # Arguments
    ///
    /// * `reader` - The reader to use for input.
    /// * `options` - The options to use when parsing.
    pub fn new(reader: R, options: &ParserOptions) -> Self {
        Self {
            reader,
            options: options.clone(),
        }
    }

    /// Create a new parser instance through a builder
    #[builder(
        finish_fn(doc {
            /// Instantiate a parser with the provided reader as input
        })
    )]
    pub const fn builder(
        /// The reader to use for input
        #[builder(finish_fn)]
        reader: R,

        #[builder(default = true)]
        /// Whether or not to enable extended globbing (a.k.a. `extglob`).
        enable_extended_globbing: bool,
        #[builder(default = false)]
        /// Whether marsh-only composition syntax is enabled.
        marsh_extensions: bool,
        #[builder(default = false)]
        /// Whether or not to enable POSIX compliance mode.
        posix_mode: bool,
        #[builder(default = false)]
        /// Whether or not to enable maximal compatibility with the `sh` shell.
        sh_mode: bool,
        #[builder(default = true)]
        /// Whether or not to perform tilde expansion for tildes at the start of words.
        tilde_expansion_at_word_start: bool,
        #[builder(default = false)]
        /// Whether or not to perform tilde expansion for tildes after colons.
        tilde_expansion_after_colon: bool,
        #[builder(default)]
        /// Select the parser internal implementation
        parser_impl: ParserImpl,
    ) -> Self {
        let options = ParserOptions {
            marsh_extensions,
            enable_extended_globbing,
            posix_mode,
            sh_mode,
            tilde_expansion_at_word_start,
            tilde_expansion_after_colon,
            parser_impl,
        };
        Self { reader, options }
    }

    /// Parses the input into an abstract syntax tree (AST) of a shell program.
    pub fn parse_program(&mut self) -> Result<ast::Program, crate::error::ParseError> {
        //
        // References:
        //   * https://www.gnu.org/software/bash/manual/bash.html#Shell-Syntax
        //   * https://mywiki.wooledge.org/BashParser
        //   * https://aosabook.org/en/v1/bash.html
        //   * https://pubs.opengroup.org/onlinepubs/9699919799/utilities/V3_chap02.html
        //
        match self.options.parser_impl {
            ParserImpl::Peg => {
                let mut tokens = self.tokenize()?;
                if self.options.marsh_extensions {
                    normalize_marsh_fanout_tokens(&mut tokens);
                }
                parse_tokens(&tokens, &self.options)
            }
            #[cfg(feature = "winnow-parser")]
            ParserImpl::Winnow => {
                // Read entire input to string for winnow_str parser
                let mut input_str = String::new();
                std::io::Read::read_to_string(&mut self.reader, &mut input_str).map_err(|e| {
                    crate::error::ParseError::Tokenizing {
                        inner: crate::tokenizer::TokenizerError::from(e),
                        position: None,
                    }
                })?;

                winnow_str::parse_program(&input_str, &self.options, &SourceInfo::default())
                    .map_err(|_e| {
                        // Convert winnow error to ParseError
                        // TODO: Extract position information from winnow error
                        crate::error::ParseError::ParsingAtEndOfInput(None)
                    })
            }
        }
    }

    /// Parses the input like [`Self::parse_program`], first expanding `aliases`
    /// lexically as Bash does while reading input (PEG parser only).
    pub fn parse_program_with_aliases(
        &mut self,
        aliases: &std::collections::HashMap<String, String>,
    ) -> Result<ast::Program, crate::error::ParseError> {
        let tokens = self.tokenize()?;
        let mut tokens =
            aliases::expand_aliases(tokens, aliases, &self.options.tokenizer_options());
        if self.options.marsh_extensions {
            normalize_marsh_fanout_tokens(&mut tokens);
        }
        parse_tokens(&tokens, &self.options)
    }

    /// Parses a function definition body from the input. The body is expected to be
    /// preceded by "()", but no function name.
    pub fn parse_function_parens_and_body(
        &mut self,
    ) -> Result<ast::FunctionBody, crate::error::ParseError> {
        let tokens = self.tokenize()?;
        let parse_result =
            peg::token_parser::function_parens_and_body(&Tokens { tokens: &tokens }, &self.options);
        parse_result_to_error(parse_result, &tokens)
    }

    fn tokenize(&mut self) -> Result<Vec<Token>, crate::error::ParseError> {
        // First we tokenize the input, according to the policy implied by provided options.
        let mut tokenizer = Tokenizer::new(&mut self.reader, &self.options.tokenizer_options());

        tracing::debug!(target: "tokenize", "Tokenizing...");

        let mut tokens = vec![];
        loop {
            let result = match tokenizer.next_token() {
                Ok(result) => result,
                Err(e) => {
                    return Err(crate::error::ParseError::Tokenizing {
                        inner: e,
                        position: tokenizer.current_location(),
                    });
                }
            };

            let reason = result.reason;
            if let Some(token) = result.token {
                tracing::debug!(target: "tokenize", "TOKEN {}: {:?} {reason:?}", tokens.len(), token);
                tokens.push(token);
            }

            if matches!(reason, TokenEndReason::EndOfInput) {
                break;
            }
        }

        tracing::debug!(target: "tokenize", "  => {} token(s)", tokens.len());

        Ok(tokens)
    }
}

fn normalize_marsh_fanout_tokens(tokens: &mut Vec<Token>) {
    let mut start = 0;
    let mut command_start = true;
    while start + 1 < tokens.len() {
        // Carry command-position state forward once. Recomputing it from the
        // complete prefix for every token made ordinary large scripts O(n²).
        let fanout = command_start
            && matches!((&tokens[start], &tokens[start + 1]),
            (Token::Word(name, _), Token::Word(open, _))
                if (name == "fanout" || name == "split") && open == "{");
        if !fanout {
            update_fanout_command_start(&mut command_start, &tokens[start]);
            start += 1;
            continue;
        }
        let Some(end) = fanout_end(tokens, start + 2) else {
            break;
        };
        let body = normalize_fanout_body(tokens[start + 2..end].to_vec());
        tokens.splice(start + 2..end, body);
        update_fanout_command_start(&mut command_start, &tokens[start]);
        start += 1;
    }
}

fn update_fanout_command_start(command_start: &mut bool, token: &Token) {
    match token {
        Token::Operator(operator, _)
            if matches!(operator.as_str(), "\n" | ";" | "&" | "&&" | "||" | "|") =>
        {
            *command_start = true;
        }
        Token::Operator(operator, _)
            if *command_start && matches!(operator.as_str(), "(" | "{") => {}
        Token::Operator(_, _) => {}
        Token::Word(word, _)
            if *command_start
                && matches!(
                    word.as_str(),
                    "if" | "then" | "do" | "else" | "elif" | "while" | "until" | "!" | "time" | "{"
                ) => {}
        Token::Word(_, _) => *command_start = false,
    }
}

fn fanout_end(tokens: &[Token], start: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, token) in tokens[start..].iter().enumerate() {
        match token {
            Token::Word(word, _) if word == "{" => depth += 1,
            Token::Word(word, _) if word == "}" => {
                if depth == 0 {
                    return Some(start + offset);
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    None
}

fn normalize_fanout_body(body: Vec<Token>) -> Vec<Token> {
    let body = body
        .into_iter()
        .flat_map(|token| match token {
            Token::Word(word, location) => split_fanout_word(&word)
                .into_iter()
                .enumerate()
                .flat_map(|(index, part)| {
                    let mut tokens = Vec::with_capacity(2);
                    if index != 0 {
                        tokens.push(Token::Word(",".into(), location.clone()));
                    }
                    if !part.is_empty() {
                        tokens.push(Token::Word(part, location.clone()));
                    }
                    tokens
                })
                .collect::<Vec<_>>(),
            token => vec![token],
        })
        .collect::<Vec<_>>();
    let leading_linebreak = body
        .first()
        .cloned()
        .filter(|token| matches!(token, Token::Operator(operator, _) if operator == "\n"));
    // Label-led branches: a branch ends at a top-level newline or `,`, or at a
    // top-level `;` whose next word is a `LABEL:`. Any other `;` (like `&&`,
    // `||`, groups, and subshells) is ordinary sequencing within the branch.
    let mut branches = Vec::<Vec<Token>>::new();
    let mut current = Vec::new();
    let mut nesting = 0usize;
    let mut body = body.into_iter().peekable();
    while let Some(token) = body.next() {
        let boundary = nesting == 0
            && match &token {
                Token::Operator(operator, _) if operator == "\n" => true,
                Token::Operator(operator, _) if operator == ";" => matches!(
                    body.peek(),
                    Some(Token::Word(word, _)) if valid_fanout_label(word)
                ),
                Token::Word(word, _) => word == ",",
                Token::Operator(_, _) => false,
            };
        if boundary {
            branches.push(std::mem::take(&mut current));
            continue;
        }
        match &token {
            Token::Word(word, _) | Token::Operator(word, _) if word == "{" || word == "(" => {
                nesting += 1;
            }
            Token::Word(word, _) | Token::Operator(word, _)
                if (word == "}" || word == ")") && nesting > 0 =>
            {
                nesting -= 1;
            }
            _ => {}
        }
        current.push(token);
    }
    branches.push(current);

    let explicit = branches
        .iter()
        .filter_map(|branch| branch.first())
        .filter_map(|token| match token {
            Token::Word(word, _) if valid_fanout_label(word) => {
                Some(word[..word.len() - 1].to_owned())
            }
            _ => None,
        })
        .collect::<std::collections::HashSet<_>>();
    let mut used = explicit;
    let mut output = Vec::new();
    if let Some(linebreak) = leading_linebreak {
        output.push(linebreak);
    }
    for mut branch in branches {
        if branch.is_empty() {
            continue;
        }
        let labeled =
            matches!(branch.first(), Some(Token::Word(word, _)) if valid_fanout_label(word));
        // A branch body is in command position after its label, so a nested
        // `fanout {` or `split {` there (`review: fanout { a: x, b: y }`)
        // is normalized too. Normalizing a normalized body is a no-op.
        let mut rest = branch.split_off(usize::from(labeled));
        normalize_marsh_fanout_tokens(&mut rest);
        branch.append(&mut rest);
        if !labeled {
            let base = branch
                .first()
                .and_then(|token| match token {
                    Token::Word(word, _) if valid_fanout_label(&format!("{word}:")) => {
                        Some(word.as_str())
                    }
                    _ => None,
                })
                .unwrap_or("branch");
            let label = unique_branch_label(base, &mut used);
            let location = branch[0].location().clone();
            branch.insert(0, Token::Word(format!("{label}:"), location));
        }
        output.append(&mut branch);
        let location = output.last().expect("nonempty branch").location().clone();
        output.push(Token::Operator("\n".into(), location));
    }
    output
}

fn split_fanout_word(word: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;
    let mut nesting = 0usize;
    for (index, character) in word.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if let Some(delimiter) = quote {
            if character == delimiter {
                quote = None;
            }
            continue;
        }
        if matches!(character, '\'' | '"') {
            quote = Some(character);
        } else if matches!(character, '(' | '{') {
            nesting += 1;
        } else if matches!(character, ')' | '}') {
            nesting = nesting.saturating_sub(1);
        } else if character == ',' && nesting == 0 {
            parts.push(word[start..index].to_owned());
            start = index + character.len_utf8();
        }
    }
    parts.push(word[start..].to_owned());
    parts
}

fn unique_branch_label(base: &str, used: &mut std::collections::HashSet<String>) -> String {
    if used.insert(base.to_owned()) {
        return base.to_owned();
    }
    for suffix in 2..=16 {
        let candidate = format!("{base}-{suffix}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    format!("{base}-17")
}

fn valid_fanout_label(word: &str) -> bool {
    let Some(label) = word.strip_suffix(':') else {
        return false;
    };
    let mut chars = label.chars();
    matches!(chars.next(), Some('a'..='z' | 'A'..='Z' | '_'))
        && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
}

/// Parses a sequence of tokens into the abstract syntax tree (AST) of a shell program.
///
/// # Arguments
///
/// * `tokens` - The tokens to parse.
/// * `options` - The options to use when parsing.
pub fn parse_tokens(
    tokens: &[Token],
    options: &ParserOptions,
) -> Result<ast::Program, crate::error::ParseError> {
    let parse_result = peg::token_parser::program(&Tokens { tokens }, options);
    parse_result_to_error(parse_result, tokens)
}

/// Tokenizes and parses text as a compound assignment value, returning its element words. Returns
/// `None` if the text is not a well-formed compound value.
///
/// # Arguments
///
/// * `input` - The text to parse.
/// * `options` - The options to use when parsing.
pub(crate) fn parse_compound_assignment_value(
    input: &str,
    options: &ParserOptions,
) -> Option<Vec<String>> {
    let mut parser = Parser::new(input.as_bytes(), options);
    let tokens = parser.tokenize().ok()?;
    let tokens = Tokens { tokens: &tokens };
    let elements = peg::token_parser::compound_assignment_value(&tokens, options).ok()?;
    Some(elements.into_iter().cloned().collect())
}

fn parse_result_to_error<R>(
    parse_result: Result<R, ::peg::error::ParseError<usize>>,
    tokens: &[Token],
) -> Result<R, crate::error::ParseError>
where
    R: std::fmt::Debug,
{
    match parse_result {
        Ok(program) => {
            tracing::debug!(target: "parse", "PROG: {:?}", program);
            Ok(program)
        }
        Err(parse_error) => {
            tracing::debug!(target: "parse", "Parse error: {:?}", parse_error);
            Err(crate::error::convert_peg_parse_error(&parse_error, tokens))
        }
    }
}

#[cfg(test)]
mod tests;
