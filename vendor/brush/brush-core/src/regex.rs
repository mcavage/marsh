#![allow(clippy::needless_pass_by_value)]

use std::borrow::Cow;
use std::cell::RefCell;

use crate::error;
use cached::Cached;

/// Cache mapping a (pattern, case-insensitive, multiline) key to a compiled regex.
type RegexCache = cached::LruCache<(String, bool, bool), fancy_regex::Regex>;

thread_local! {
    // Wrapped in `Option` so that if cache construction ever fails we gracefully
    // degrade to compiling regexes uncached rather than panicking. (With a fixed
    // positive `max_size` this always succeeds, but `build()` is fallible.)
    static REGEX_CACHE: RefCell<Option<RegexCache>> =
        RefCell::new(cached::LruCache::builder().max_size(64).build().ok());
}

/// Represents a piece of a regular expression.
#[derive(Clone, Debug)]
pub(crate) enum RegexPiece {
    /// A pattern that should be interpreted as a regular expression.
    Pattern(String),
    /// A literal string that should be matched exactly.
    Literal(String),
}

#[cfg(not(unix))]
impl RegexPiece {
    fn to_regex_str(&self) -> Cow<'_, str> {
        match self {
            Self::Pattern(s) => Cow::Borrowed(s.as_str()),
            Self::Literal(s) => escape_literal_regex_piece(s.as_str()),
        }
    }
}

type RegexWord = Vec<RegexPiece>;

/// Encapsulates a regular expression usable in the shell.
#[derive(Clone, Debug)]
pub struct Regex {
    pieces: RegexWord,
    case_insensitive: bool,
    multiline: bool,
}

impl From<RegexWord> for Regex {
    fn from(pieces: RegexWord) -> Self {
        Self {
            pieces,
            case_insensitive: false,
            multiline: false,
        }
    }
}

impl Regex {
    /// Sets the regular expression's case sensitivity.
    ///
    /// # Arguments
    ///
    /// * `value` - The new case sensitivity value.
    pub const fn set_case_insensitive(mut self, value: bool) -> Self {
        self.case_insensitive = value;
        self
    }

    /// Enables (or disables) multiline support for this pattern: `.` then also matches
    /// newline characters. `^` and `$` always anchor to the whole string, as in POSIX
    /// regular expressions, which have no line anchors.
    ///
    /// # Arguments
    ///
    /// * `value` - The new multiline value.
    pub const fn set_multiline(mut self, value: bool) -> Self {
        self.multiline = value;
        self
    }

    /// Computes if the regular expression matches the given string.
    ///
    /// # Arguments
    ///
    /// * `value` - The string to check for a match.
    #[cfg(not(unix))]
    pub fn matches(&self, value: &str) -> Result<Option<Vec<Option<String>>>, error::Error> {
        let regex_pattern: String = self
            .pieces
            .iter()
            .map(|piece| piece.to_regex_str())
            .collect();

        let re = compile_regex(regex_pattern, self.case_insensitive, self.multiline)?;

        Ok(re.captures(value)?.map(|captures| {
            captures
                .iter()
                .map(|c| c.map(|m| m.as_str().to_owned()))
                .collect()
        }))
    }
}

/// The outcome of matching with the C library's regular expressions.
#[cfg(unix)]
pub(crate) enum PosixMatch {
    /// The match and each group (`None` for a group that took no part).
    Matched(Vec<Option<String>>),
    NoMatch,
    /// `regcomp` refused the pattern; the C library's reason.
    Invalid(String),
}

impl Regex {
    /// The pattern as Bash hands it to `regcomp`: quoted parts keep only their
    /// ERE special characters backslash-escaped (Bash's `ere_char`). Traces of
    /// `[[ =~ ]]` show it.
    pub(crate) fn bash_pattern(&self) -> String {
        let mut pattern = String::new();
        for piece in &self.pieces {
            match piece {
                RegexPiece::Pattern(s) => pattern.push_str(s),
                RegexPiece::Literal(s) => {
                    for c in s.chars() {
                        if matches!(
                            c,
                            '.' | '[' | '\\' | '(' | ')' | '*' | '+' | '?' | '{' | '|' | '^' | '$'
                        ) {
                            pattern.push('\\');
                        }
                        pattern.push(c);
                    }
                }
            }
        }
        pattern
    }

    /// Matches `value` with the C library's POSIX extended regular expressions,
    /// as Bash's `[[ =~ ]]` does (leftmost-longest, the libc's classes and
    /// messages), with the character type and collation of the named locales.
    #[cfg(unix)]
    pub(crate) fn posix_matches(&self, value: &str, ctype: &str, collate: &str) -> PosixMatch {
        let pattern = self.bash_pattern();
        let (Ok(c_pattern), Ok(c_value)) = (
            std::ffi::CString::new(crate::bytes::encode(&pattern).into_owned()),
            std::ffi::CString::new(crate::bytes::encode(value).into_owned()),
        ) else {
            return PosixMatch::NoMatch;
        };
        let _locale = posix::ThreadLocale::new(ctype, collate);
        let mut flags = libc::REG_EXTENDED;
        if self.case_insensitive {
            flags |= libc::REG_ICASE;
        }
        // SAFETY: `regex` is initialized by a successful `regcomp`, used, and
        // freed once; the strings are NUL-terminated and outlive the calls.
        unsafe {
            let mut regex: libc::regex_t = std::mem::zeroed();
            let error = libc::regcomp(&raw mut regex, c_pattern.as_ptr(), flags);
            if error != 0 {
                let mut reason = [0u8; 256];
                libc::regerror(
                    error,
                    &raw const regex,
                    reason.as_mut_ptr().cast(),
                    reason.len(),
                );
                let reason = std::ffi::CStr::from_bytes_until_nul(&reason)
                    .map(|reason| reason.to_string_lossy().into_owned())
                    .unwrap_or_default();
                return PosixMatch::Invalid(reason);
            }
            let mut groups = vec![
                libc::regmatch_t {
                    rm_so: -1,
                    rm_eo: -1
                };
                posix::group_count(&pattern) + 1
            ];
            let found = libc::regexec(
                &raw const regex,
                c_value.as_ptr(),
                groups.len(),
                groups.as_mut_ptr(),
                0,
            ) == 0;
            libc::regfree(&raw mut regex);
            if !found {
                return PosixMatch::NoMatch;
            }
            let bytes = c_value.as_bytes();
            PosixMatch::Matched(
                groups
                    .iter()
                    .map(|group| {
                        let start = usize::try_from(group.rm_so).ok()?;
                        let end = usize::try_from(group.rm_eo).ok()?;
                        Some(crate::bytes::decode(bytes.get(start..end)?).into_owned())
                    })
                    .collect(),
            )
        }
    }
}

#[cfg(unix)]
mod posix {
    /// A locale in effect on this thread while it lives (`uselocale`).
    pub(super) struct ThreadLocale {
        locale: libc::locale_t,
        previous: libc::locale_t,
    }

    impl ThreadLocale {
        /// The named character type and collation (each falling back to `C`
        /// where the system has no such locale, as `setlocale` leaves Bash).
        pub(super) fn new(ctype: &str, collate: &str) -> Option<Self> {
            let named = |name: &str| std::ffi::CString::new(name).ok();
            // SAFETY: `newlocale` takes NUL-terminated names and a base it
            // consumes on success; `uselocale` installs a valid locale.
            unsafe {
                let mut locale = std::ptr::null_mut();
                for (mask, name) in [
                    (libc::LC_CTYPE_MASK, ctype),
                    (libc::LC_COLLATE_MASK, collate),
                ] {
                    let name = named(name)?;
                    let updated = libc::newlocale(mask, name.as_ptr(), locale);
                    let updated = if updated.is_null() {
                        libc::newlocale(mask, c"C".as_ptr(), locale)
                    } else {
                        updated
                    };
                    if updated.is_null() {
                        if !locale.is_null() {
                            libc::freelocale(locale);
                        }
                        return None;
                    }
                    locale = updated;
                }
                let previous = libc::uselocale(locale);
                Some(Self { locale, previous })
            }
        }
    }

    impl Drop for ThreadLocale {
        fn drop(&mut self) {
            // SAFETY: restores the thread's previous locale, then frees ours.
            unsafe {
                libc::uselocale(self.previous);
                libc::freelocale(self.locale);
            }
        }
    }

    /// How many groups (`(`) an extended regular expression has, as
    /// `regcomp`'s `re_nsub` counts them (outside bracket expressions).
    pub(super) fn group_count(pattern: &str) -> usize {
        let mut count = 0;
        let mut chars = pattern.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    chars.next();
                }
                '(' => count += 1,
                '[' => {
                    let _ = chars.next_if_eq(&'^');
                    let _ = chars.next_if_eq(&']');
                    while let Some(c) = chars.next() {
                        match c {
                            ']' => break,
                            '[' if chars.peek().is_some_and(|c| matches!(c, ':' | '.' | '=')) => {
                                let close = chars.next().unwrap_or_default();
                                let mut previous = '\0';
                                for c in chars.by_ref() {
                                    if c == ']' && previous == close {
                                        break;
                                    }
                                    previous = c;
                                }
                            }
                            _ => (),
                        }
                    }
                }
                _ => (),
            }
        }
        count
    }
}

pub(crate) fn compile_regex(
    regex_str: String,
    case_insensitive: bool,
    multiline: bool,
) -> Result<fancy_regex::Regex, error::Error> {
    // Move regex_str into the key to avoid cloning on cache-hit path.
    let key = (regex_str, case_insensitive, multiline);

    let cached_regex = REGEX_CACHE.with(|cache| {
        cache
            .borrow_mut()
            .as_mut()
            .and_then(|c| c.cache_get(&key).cloned())
    });
    if let Some(re) = cached_regex {
        return Ok(re);
    }

    // Handle identified cases where a shell-supported regex isn't supported directly by
    // `fancy_regex` -- specifically, adding missing escape characters.
    let mut regex_str = add_missing_escape_chars_to_regex(key.0.as_str());

    // Handle multiline enablement.
    if multiline {
        // The fancy_regex crate internally seems to have flags that can be used
        // to enable multiline support, but they're not exposed via its
        // RegexBuilder. We instead just prefix with the right flags.
        let updated_str = std::format!("(?s){regex_str}");
        regex_str = updated_str.into();
    }

    let mut builder = fancy_regex::RegexBuilder::new(regex_str.as_ref());
    builder.case_insensitive(case_insensitive);

    let re = match builder.build() {
        Ok(re) => re,
        Err(e) => return Err(error::ErrorKind::InvalidRegexError(e, regex_str.to_string()).into()),
    };

    // Release borrow on key.0 before moving key into cache_set.
    drop(regex_str);

    REGEX_CACHE.with(|cache| {
        if let Some(c) = cache.borrow_mut().as_mut() {
            c.cache_set(key, re.clone());
        }
    });

    Ok(re)
}

fn add_missing_escape_chars_to_regex(s: &str) -> Cow<'_, str> {
    // We may see a character class with an unescaped '[' (open bracket) character. We need
    // to escape that character.
    let mut in_escape = false;
    let mut in_brackets = false;
    // A ']' in the first member position of a bracket expression (immediately after the
    // opening '[' or after the leading '^' that negates it) is a member of the
    // expression, not its terminator.
    let mut at_first_member = false;
    let mut insertion_positions = vec![];

    let mut peekable = s.char_indices().peekable();
    while let Some((byte_offset, c)) = peekable.next() {
        let next_is_colon = peekable.peek().is_some_and(|(_, c)| *c == ':');
        let was_at_first_member = at_first_member;
        at_first_member = false;

        match c {
            '[' if !in_escape && !in_brackets => {
                in_brackets = true;
                // A '^' here negates the expression; it isn't a member itself, so the
                // first member position is the one after it. Any later '^' is an
                // ordinary member.
                let _ = peekable.next_if(|(_, c)| *c == '^');
                at_first_member = true;
            }
            '[' if !in_escape && in_brackets && !next_is_colon => {
                // Need to escape.
                insertion_positions.push(byte_offset);
            }
            ']' if !in_escape && in_brackets && was_at_first_member => {
                // `fancy_regex` doesn't implement the rule that this ']' is a member;
                // escape it so it's a member there too, and so it can be the low end
                // of a range (e.g. `[]-a]`).
                insertion_positions.push(byte_offset);
            }
            ']' if !in_escape && in_brackets => {
                in_brackets = false;
            }
            _ => (),
        }

        in_escape = !in_escape && c == '\\';
    }

    if insertion_positions.is_empty() {
        return s.into();
    }

    let mut updated = s.to_owned();
    for pos in insertion_positions.iter().rev() {
        updated.insert(*pos, '\\');
    }

    updated.into()
}

#[cfg(not(unix))]
fn escape_literal_regex_piece(s: &str) -> Cow<'_, str> {
    let mut result = String::new();

    for c in s.chars() {
        match c {
            c if regex_char_is_special(c) => {
                result.push('\\');
                result.push(c);
            }
            c => result.push(c),
        }
    }

    result.into()
}

pub(crate) const fn regex_char_is_special(c: char) -> bool {
    matches!(
        c,
        '\\' | '^' | '$' | '.' | '|' | '?' | '*' | '+' | '(' | ')' | '[' | ']' | '{' | '}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_missing_escape_chars_to_regex() {
        // Negative cases -- where we don't need to escape.
        assert_eq!(add_missing_escape_chars_to_regex("a[b]"), "a[b]");
        assert_eq!(add_missing_escape_chars_to_regex(r"a\[b\]"), r"a\[b\]");
        assert_eq!(add_missing_escape_chars_to_regex(r"a[b\[]"), r"a[b\[]");

        // Positive case -- where we need to escape.
        assert_eq!(add_missing_escape_chars_to_regex(r"a[b[]"), r"a[b\[]");
        assert_eq!(add_missing_escape_chars_to_regex(r"a[[]"), r"a[\[]");
    }

    #[test]
    fn test_leading_close_bracket_in_bracket_expression() {
        // A ']' in the first member position (after any '^') is a member of the bracket
        // expression, not its terminator, so the expression is still open after it.
        // It's escaped as well, since `fancy_regex` doesn't implement that rule; that
        // also makes it the low end of a range in `[]-a]`, as it is in a shell pattern.
        assert_eq!(add_missing_escape_chars_to_regex("[]]"), r"[\]]");
        assert_eq!(add_missing_escape_chars_to_regex("[^]]"), r"[^\]]");
        assert_eq!(add_missing_escape_chars_to_regex("[][]"), r"[\]\[]");
        assert_eq!(add_missing_escape_chars_to_regex("[^][]"), r"[^\]\[]");
        assert_eq!(add_missing_escape_chars_to_regex("[]a[]"), r"[\]a\[]");
        assert_eq!(add_missing_escape_chars_to_regex("[]-a]"), r"[\]-a]");

        // The rule only applies in the first member position: the ']' in `[a]` and the
        // already-escaped one in `[\]]` both close their expressions.
        assert_eq!(add_missing_escape_chars_to_regex("[a][]x]"), r"[a][\]x]");
        assert_eq!(add_missing_escape_chars_to_regex(r"[\]][]x]"), r"[\]][\]x]");

        // Only the '^' that negates the expression is skipped over; a later one is an
        // ordinary member, so the ']' after it terminates the expression.
        assert_eq!(add_missing_escape_chars_to_regex("[^^][ab]"), "[^^][ab]");
        assert_eq!(add_missing_escape_chars_to_regex("[^^][b[]"), r"[^^][b\[]");

        // A '^' outside a bracket expression doesn't open one.
        assert_eq!(add_missing_escape_chars_to_regex("^[a[]"), r"^[a\[]");
    }
}
