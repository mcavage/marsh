//! Lexical alias expansion over a token stream, as Bash performs it while
//! reading input: an unquoted word in command position whose text names an
//! alias is replaced by the tokens of the alias value before parsing.

use std::collections::HashMap;

use crate::tokenizer::{Token, TokenizerOptions, tokenize_str_with_options};

/// Expands aliases in `tokens`. A word is eligible when it is in command
/// position (after a control operator, a reserved word that begins a command
/// list, or assignment words), or follows an alias whose value ends in a
/// blank. An alias is not expanded again within its own expansion.
pub(super) fn expand_aliases(
    tokens: Vec<Token>,
    aliases: &HashMap<String, String>,
    options: &TokenizerOptions,
) -> Vec<Token> {
    let mut expander = Expander {
        aliases,
        options,
        output: Vec::with_capacity(tokens.len()),
        active: Vec::new(),
        state: CommandState {
            command: true,
            ..CommandState::default()
        },
    };
    expander.process(tokens);
    expander.output
}

#[derive(Default)]
struct CommandState {
    /// The next word is in command position.
    command: bool,
    /// The next word may expand because the previous alias value ended in a blank.
    after_blank_alias: bool,
    /// The next word is a redirection target.
    redirect_target: bool,
    /// Here-document tokens (tag, body, end tag) still to pass through.
    here_doc_tokens: usize,
    /// Parenthesis depth inside an arithmetic `(( ... ))` command.
    arithmetic: usize,
    /// Inside a `[[ ... ]]` conditional.
    conditional: bool,
    /// The next word names a function (`function NAME`).
    function_name: bool,
    /// Enclosing `case` commands.
    cases: Vec<CaseState>,
    /// The previous token was an opening parenthesis operator.
    after_open_paren: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CaseState {
    /// Between `case` and `in`.
    Subject,
    /// Reading a pattern list.
    Pattern,
    /// Reading a clause body.
    Body,
}

struct Expander<'a> {
    aliases: &'a HashMap<String, String>,
    options: &'a TokenizerOptions,
    output: Vec<Token>,
    active: Vec<String>,
    state: CommandState,
}

impl Expander<'_> {
    fn process(&mut self, tokens: Vec<Token>) {
        let mut tokens = tokens.into_iter().peekable();
        while let Some(token) = tokens.next() {
            let next_is_open_paren =
                matches!(tokens.peek(), Some(Token::Operator(op, _)) if op == "(");
            match token {
                Token::Operator(op, location) => {
                    self.operator(&op, next_is_open_paren);
                    self.output.push(Token::Operator(op, location));
                }
                Token::Word(word, location) => {
                    if self.word_may_expand() {
                        if let Some(value) = self.lookup(&word) {
                            self.expand(&word, &value, &location);
                            continue;
                        }
                    }
                    self.word(&word);
                    self.output.push(Token::Word(word, location));
                }
            }
        }
    }

    fn word_may_expand(&mut self) -> bool {
        let state = &mut self.state;
        let eligible = std::mem::take(&mut state.after_blank_alias)
            || (state.command
                && !matches!(
                    state.cases.last(),
                    Some(CaseState::Subject | CaseState::Pattern)
                ));
        eligible
            && state.here_doc_tokens == 0
            && state.arithmetic == 0
            && !state.conditional
            && !state.redirect_target
            && !state.function_name
    }

    fn lookup(&self, word: &str) -> Option<String> {
        if self.active.iter().any(|active| active == word) {
            return None;
        }
        // Quoted or escaped words never match: alias names cannot contain quotes.
        self.aliases.get(word).cloned()
    }

    fn expand(&mut self, name: &str, value: &str, location: &crate::SourceSpan) {
        let Ok(tokens) = tokenize_str_with_options(value, self.options) else {
            // An alias value that cannot be tokenized stands as a single word.
            self.word(value);
            self.output
                .push(Token::Word(value.to_owned(), location.clone()));
            return;
        };
        let tokens = tokens
            .into_iter()
            .map(|token| match token {
                Token::Operator(op, _) => Token::Operator(op, location.clone()),
                Token::Word(word, _) => Token::Word(word, location.clone()),
            })
            .collect();
        self.active.push(name.to_owned());
        self.process(tokens);
        self.active.pop();
        if value.ends_with([' ', '\t']) {
            self.state.after_blank_alias = true;
        }
    }

    fn operator(&mut self, op: &str, next_is_open_paren: bool) {
        let state = &mut self.state;
        let after_open_paren = std::mem::take(&mut state.after_open_paren);
        if state.here_doc_tokens > 0 {
            return;
        }
        if state.arithmetic > 0 {
            match op {
                "(" => state.arithmetic += 1,
                ")" => {
                    state.arithmetic -= 1;
                    state.command = false;
                }
                _ => {}
            }
            return;
        }
        if state.conditional {
            return;
        }
        match op {
            "<<" | "<<-" => state.here_doc_tokens = 3,
            "<" | ">" | ">>" | ">|" | "<>" | "<&" | ">&" | "&>" | "&>>" | "<<<" => {
                state.redirect_target = true;
            }
            "(" if state.command && next_is_open_paren => state.arithmetic = 1,
            "(" => {
                state.after_open_paren = true;
                if !matches!(state.cases.last(), Some(CaseState::Pattern)) {
                    state.command = true;
                }
            }
            ")" if matches!(state.cases.last(), Some(CaseState::Pattern)) => {
                set_case(state, CaseState::Body);
                state.command = true;
            }
            // `NAME ()` is followed by a function body.
            ")" => state.command = after_open_paren,
            ";;" | ";&" | ";;&" => {
                set_case(state, CaseState::Pattern);
                state.command = false;
            }
            "\n" if matches!(state.cases.last(), Some(CaseState::Pattern)) => {}
            "|" if matches!(state.cases.last(), Some(CaseState::Pattern)) => {}
            "\n" | ";" | "&" | "&&" | "||" | "|" | "|&" => state.command = true,
            _ => {}
        }
    }

    fn word(&mut self, word: &str) {
        let state = &mut self.state;
        state.after_open_paren = false;
        if state.here_doc_tokens > 0 {
            state.here_doc_tokens -= 1;
            return;
        }
        if std::mem::take(&mut state.redirect_target) || state.arithmetic > 0 {
            return;
        }
        if state.conditional {
            if word == "]]" {
                state.conditional = false;
                state.command = false;
            }
            return;
        }
        if std::mem::take(&mut state.function_name) {
            state.command = true;
            return;
        }
        match state.cases.last() {
            Some(CaseState::Subject) => {
                if word == "in" {
                    set_case(state, CaseState::Pattern);
                }
                return;
            }
            Some(CaseState::Pattern) => {
                if word == "esac" {
                    state.cases.pop();
                    state.command = false;
                }
                return;
            }
            Some(CaseState::Body) if state.command && word == "esac" => {
                state.cases.pop();
                state.command = false;
                return;
            }
            _ => {}
        }
        if !state.command {
            return;
        }
        match word {
            "if" | "then" | "else" | "elif" | "do" | "while" | "until" | "!" | "{" | "time"
            | "coproc" => {}
            "case" => {
                state.cases.push(CaseState::Subject);
                state.command = false;
            }
            "function" => {
                state.function_name = true;
                state.command = false;
            }
            "[[" => state.conditional = true,
            _ if is_assignment(word) => {}
            _ => state.command = false,
        }
    }
}

fn set_case(state: &mut CommandState, case: CaseState) {
    if let Some(top) = state.cases.last_mut() {
        *top = case;
    }
}

fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let name = name.strip_suffix('+').unwrap_or(name);
    let name = name.split_once('[').map_or(name, |(name, _)| name);
    name.starts_with(|c: char| c == '_' || c.is_ascii_alphabetic())
        && name.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
}
