// SPDX-License-Identifier: Apache-2.0

//! One rules file: splitting it into lines as git's `add_patterns_from_buffer` does, compiling
//! each line as `parse_path_pattern` does, and matching with the semantics of `wildmatch`.

use std::fmt;
use std::marker::PhantomData;

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::canonical_combining_class;

use crate::NameKey;

/// The form names and patterns are compared in: the name key, then NFC.
pub(crate) fn key<K: NameKey>(name: &str) -> String {
    K::key(name).nfc().collect()
}

/// Why a line of a rules file is not used. The line is skipped and the rest of the file applies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LineError {
    /// The line is not valid UTF-8.
    NotUtf8,
    /// The line holds a NUL byte, which no name can contain.
    ContainsNul,
    /// A `[` has no closing `]`, so the pattern could never match.
    UnclosedBracket,
    /// A `[:name:]` names no POSIX class, so the pattern could never match.
    UnknownClass(String),
    /// A bracket range has an endpoint with combining marks, so the endpoint is not one character.
    CombiningMarkInRange,
    /// The pattern ends with an unescaped `\`, so it could never match.
    TrailingBackslash,
}

/// A line that is not used, numbered from 1 as git numbers it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidLine {
    pub line: u32,
    pub error: LineError,
}

/// The parsed rules of one `.fbignore` file.
pub struct RuleFile<K> {
    rules: Vec<Rule>,
    invalid: Vec<InvalidLine>,
    key: PhantomData<fn() -> K>,
}

impl<K> Clone for RuleFile<K> {
    fn clone(&self) -> Self {
        Self {
            rules: self.rules.clone(),
            invalid: self.invalid.clone(),
            key: PhantomData,
        }
    }
}

impl<K> fmt::Debug for RuleFile<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RuleFile")
            .field("rules", &self.rules)
            .field("invalid", &self.invalid)
            .finish()
    }
}

impl<K> Default for RuleFile<K> {
    fn default() -> Self {
        Self {
            rules: Vec::new(),
            invalid: Vec::new(),
            key: PhantomData,
        }
    }
}

impl<K: NameKey> RuleFile<K> {
    /// Parses a rules file. Never fails: a line that cannot be used is listed in
    /// [`RuleFile::invalid_lines`] and skipped.
    ///
    /// A UTF-8 byte order mark at the start is skipped. Lines end at `\n`, with a `\r` before it
    /// removed. Blank lines and lines starting with `#` are skipped. Trailing spaces are removed
    /// unless escaped with `\`.
    pub fn parse(bytes: &[u8]) -> Self {
        let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
        let mut lines: Vec<&[u8]> = bytes.split(|&b| b == b'\n').collect();
        if lines.last().is_some_and(|last| last.is_empty()) {
            lines.pop();
        }
        let mut file = Self::default();
        for (index, line) in lines.into_iter().enumerate() {
            let number = u32::try_from(index + 1).unwrap_or(u32::MAX);
            if line.is_empty() || line[0] == b'#' {
                continue;
            }
            let line = trim_trailing_spaces(line.strip_suffix(b"\r").unwrap_or(line));
            match compile::<K>(line, number) {
                Ok(Some(rule)) => file.rules.push(rule),
                Ok(None) => {}
                Err(error) => file.invalid.push(InvalidLine {
                    line: number,
                    error,
                }),
            }
        }
        file
    }

    /// The lines that are not used, in file order.
    pub fn invalid_lines(&self) -> &[InvalidLine] {
        &self.invalid
    }

    /// The last rule matching a path. `path` is the path below the file's directory and
    /// `basename` its last component, both in key form and split into [`units`].
    pub(crate) fn last_match(
        &self,
        path: &[&[char]],
        basename: &[&[char]],
        is_dir: bool,
    ) -> Option<&Rule> {
        self.rules
            .iter()
            .rev()
            .find(|rule| rule.matches::<K>(path, basename, is_dir))
    }
}

/// Git's `trim_trailing_spaces`: drops a run of spaces at the end unless the first of them is
/// escaped. A line ending in a lone `\` is left as it is.
fn trim_trailing_spaces(line: &[u8]) -> &[u8] {
    let mut last_space = None;
    let mut i = 0;
    while i < line.len() {
        match line[i] {
            b' ' => {
                last_space.get_or_insert(i);
            }
            b'\\' => {
                i += 1;
                if i == line.len() {
                    return line;
                }
                last_space = None;
            }
            _ => last_space = None,
        }
        i += 1;
    }
    &line[..last_space.unwrap_or(line.len())]
}

#[derive(Clone, Debug)]
pub(crate) struct Rule {
    pub(crate) line: u32,
    pub(crate) negated: bool,
    dir_only: bool,
    /// The pattern has a slash, so it matches the path below the file's directory instead of the
    /// basename anywhere below it.
    pathname: bool,
    tokens: Vec<Token>,
}

impl Rule {
    fn matches<K: NameKey>(&self, path: &[&[char]], basename: &[&[char]], is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        let text = if self.pathname { path } else { basename };
        wildmatch::<K>(&self.tokens, text, self.pathname)
    }
}

/// Splits key-form text into the characters matching counts: a character with the combining
/// marks (canonical combining class other than 0) that follow it. A `/` never takes marks, so
/// path components split the same way on their own and joined.
pub(crate) fn units(text: &[char]) -> Vec<&[char]> {
    let mut units = Vec::new();
    let mut start = 0;
    for i in 1..text.len() {
        if !is_mark(text[i]) || text[start] == '/' {
            units.push(&text[start..i]);
            start = i;
        }
    }
    if start < text.len() {
        units.push(&text[start..]);
    }
    units
}

fn is_mark(c: char) -> bool {
    canonical_combining_class(c) != 0
}

fn single(unit: &[char]) -> Option<char> {
    match unit {
        [c] => Some(*c),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    /// One character with its combining marks, in key form.
    Literal(Box<[char]>),
    /// A `/`, written plain or escaped.
    Slash,
    /// `?`
    AnyChar,
    /// `*`, or a run of asterisks that is not a special `**`.
    Star,
    /// `**` at the start or end of the pattern or between slashes. Followed by a slash, it also
    /// matches zero directories.
    DoubleStar,
    Class(Class),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Class {
    negated: bool,
    items: Vec<ClassItem>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ClassItem {
    /// One character with its combining marks, in key form.
    Member(Box<[char]>),
    Range(char, char),
    Posix(Posix),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Posix {
    Alnum,
    Alpha,
    Blank,
    Cntrl,
    Digit,
    Graph,
    Lower,
    Print,
    Punct,
    Space,
    Upper,
    Xdigit,
}

impl Posix {
    fn named(name: &str) -> Option<Self> {
        Some(match name {
            "alnum" => Self::Alnum,
            "alpha" => Self::Alpha,
            "blank" => Self::Blank,
            "cntrl" => Self::Cntrl,
            "digit" => Self::Digit,
            "graph" => Self::Graph,
            "lower" => Self::Lower,
            "print" => Self::Print,
            "punct" => Self::Punct,
            "space" => Self::Space,
            "upper" => Self::Upper,
            "xdigit" => Self::Xdigit,
            _ => return None,
        })
    }

    /// Git's ASCII classes. Under folding, `upper` also matches a lowercase letter whose
    /// uppercase form has it as key, as git does with `core.ignorecase`.
    fn matches<K: NameKey>(self, c: char) -> bool {
        match self {
            Self::Alnum => c.is_ascii_alphanumeric(),
            Self::Alpha => c.is_ascii_alphabetic(),
            Self::Blank => c == ' ' || c == '\t',
            Self::Cntrl => c.is_ascii_control(),
            Self::Digit => c.is_ascii_digit(),
            Self::Graph => c.is_ascii_graphic(),
            Self::Lower => c.is_ascii_lowercase(),
            Self::Print => matches!(c, ' '..='~'),
            Self::Punct => c.is_ascii_punctuation(),
            Self::Space => matches!(c, ' ' | '\t' | '\n' | '\r'),
            Self::Upper => {
                c.is_ascii_uppercase()
                    || (c.is_ascii_lowercase() && folds_to::<K>(c.to_ascii_uppercase(), c))
            }
            Self::Xdigit => c.is_ascii_hexdigit(),
        }
    }
}

/// Whether `upper`, as a name, has the single character `c` as key.
fn folds_to<K: NameKey>(upper: char, c: char) -> bool {
    let folded: Vec<char> = key::<K>(upper.encode_utf8(&mut [0; 4])).chars().collect();
    folded == [c]
}

impl Class {
    fn matches<K: NameKey>(&self, unit: &[char]) -> bool {
        let found = self.items.iter().any(|item| match item {
            ClassItem::Member(member) => **member == *unit,
            ClassItem::Range(low, high) => single(unit).is_some_and(|c| {
                let range = *low..=*high;
                range.contains(&c) || {
                    let mut upper = c.to_uppercase();
                    match (upper.next(), upper.next()) {
                        (Some(u), None) if u != c => range.contains(&u) && folds_to::<K>(u, c),
                        _ => false,
                    }
                }
            }),
            ClassItem::Posix(class) => single(unit).is_some_and(|c| class.matches::<K>(c)),
        });
        found != self.negated
    }
}

/// Compiles one trimmed line, as git's `parse_path_pattern` does, after normalizing it to NFC so
/// that canonically equivalent spellings compile to the same pattern. `Ok(None)` is a pattern
/// that matches nothing and is not an error in git either, such as `!` or `/`.
fn compile<K: NameKey>(line: &[u8], number: u32) -> Result<Option<Rule>, LineError> {
    if line.is_empty() {
        return Ok(None);
    }
    if line.contains(&0) {
        return Err(LineError::ContainsNul);
    }
    let text: String = std::str::from_utf8(line)
        .map_err(|_| LineError::NotUtf8)?
        .nfc()
        .collect();
    let (negated, body) = match text.strip_prefix('!') {
        Some(rest) => (true, rest),
        None => (false, text.as_str()),
    };
    let (dir_only, body) = match body.strip_suffix('/') {
        Some(rest) => (true, rest),
        None => (false, body),
    };
    let pathname = body.contains('/');
    let body = if pathname {
        body.strip_prefix('/').unwrap_or(body)
    } else {
        body
    };
    let tokens = tokenize::<K>(body)?;
    if tokens.is_empty() {
        return Ok(None);
    }
    Ok(Some(Rule {
        line: number,
        negated,
        dir_only,
        pathname,
        tokens,
    }))
}

enum Raw {
    Char(char),
    Token(Token),
}

fn tokenize<K: NameKey>(body: &str) -> Result<Vec<Token>, LineError> {
    let chars: Vec<char> = body.chars().collect();
    let mut raw = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => {
                let escaped = *chars.get(i + 1).ok_or(LineError::TrailingBackslash)?;
                raw.push(if escaped == '/' {
                    Raw::Token(Token::Slash)
                } else {
                    Raw::Char(escaped)
                });
                i += 2;
            }
            '*' => {
                let start = i;
                while chars.get(i) == Some(&'*') {
                    i += 1;
                }
                let after_slash = matches!(raw.last(), None | Some(Raw::Token(Token::Slash)));
                let before_slash = match chars.get(i) {
                    None | Some('/') => true,
                    Some('\\') => chars.get(i + 1) == Some(&'/'),
                    Some(_) => false,
                };
                raw.push(Raw::Token(
                    if i - start >= 2 && after_slash && before_slash {
                        Token::DoubleStar
                    } else {
                        Token::Star
                    },
                ));
            }
            '?' => {
                raw.push(Raw::Token(Token::AnyChar));
                i += 1;
            }
            '[' => {
                let (class, next) = parse_class::<K>(&chars, i)?;
                raw.push(Raw::Token(Token::Class(class)));
                i = next;
            }
            '/' => {
                raw.push(Raw::Token(Token::Slash));
                i += 1;
            }
            c => {
                raw.push(Raw::Char(c));
                i += 1;
            }
        }
    }

    let mut tokens = Vec::with_capacity(raw.len());
    let mut run = String::new();
    for item in raw {
        match item {
            Raw::Char(c) => run.push(c),
            Raw::Token(token) => {
                push_literals::<K>(&mut tokens, &run);
                run.clear();
                tokens.push(token);
            }
        }
    }
    push_literals::<K>(&mut tokens, &run);
    Ok(tokens)
}

fn push_literals<K: NameKey>(tokens: &mut Vec<Token>, run: &str) {
    let keyed: Vec<char> = key::<K>(run).chars().collect();
    tokens.extend(units(&keyed).into_iter().map(|u| Token::Literal(u.into())));
}

/// The end of the combining marks starting at `chars[from]`.
fn marks_end(chars: &[char], from: usize) -> usize {
    from + chars[from.min(chars.len())..]
        .iter()
        .take_while(|&&c| is_mark(c))
        .count()
}

/// Parses the bracket expression starting at `chars[open]`, as `wildmatch` reads it, and
/// returns it with the index after its closing `]`. A member is a character with the combining
/// marks that follow it; a range endpoint must be a single character.
fn parse_class<K: NameKey>(chars: &[char], open: usize) -> Result<(Class, usize), LineError> {
    let mut j = open + 1;
    let negated = matches!(chars.get(j), Some('!' | '^'));
    if negated {
        j += 1;
    }
    let mut items = Vec::new();
    let mut prev: Option<Vec<char>> = None;
    let mut first = true;
    loop {
        let c = *chars.get(j).ok_or(LineError::UnclosedBracket)?;
        if c == ']' && !first {
            j += 1;
            break;
        }
        first = false;
        if c == '-' && prev.is_some() && chars.get(j + 1).is_some_and(|&next| next != ']') {
            let low = prev.take().unwrap_or_default();
            let mut k = j + 1;
            let mut high = chars[k];
            k += 1;
            if high == '\\' {
                high = *chars.get(k).ok_or(LineError::UnclosedBracket)?;
                k += 1;
            }
            let (Some(low), false) = (single(&low), chars.get(k).is_some_and(|&c| is_mark(c)))
            else {
                return Err(LineError::CombiningMarkInRange);
            };
            items.push(ClassItem::Range(low, high));
            j = k;
        } else if c == '[' && chars.get(j + 1) == Some(&':') {
            let start = j + 2;
            let close = chars[start..]
                .iter()
                .position(|&c| c == ']')
                .map(|offset| start + offset)
                .ok_or(LineError::UnclosedBracket)?;
            if close == start || chars[close - 1] != ':' {
                items.push(ClassItem::Member(Box::new(['['])));
                prev = Some(vec!['[']);
                j += 1;
                continue;
            }
            let name: String = chars[start..close - 1].iter().collect();
            let class = Posix::named(&name).ok_or(LineError::UnknownClass(name))?;
            items.push(ClassItem::Posix(class));
            prev = None;
            j = close + 1;
        } else {
            let (member_start, rest) = if c == '\\' {
                (j + 1, j + 2)
            } else {
                (j, j + 1)
            };
            let first_char = *chars.get(member_start).ok_or(LineError::UnclosedBracket)?;
            let end = marks_end(chars, rest);
            let member: Vec<char> = std::iter::once(first_char)
                .chain(chars[rest..end].iter().copied())
                .collect();
            items.push(ClassItem::Member(member.clone().into()));
            prev = Some(member);
            j = end;
        }
    }

    let items = items
        .into_iter()
        .filter_map(|item| match item {
            ClassItem::Member(member) => {
                let keyed: Vec<char> = key::<K>(&member.iter().collect::<String>())
                    .chars()
                    .collect();
                match units(&keyed)[..] {
                    [unit] => Some(ClassItem::Member(unit.into())),
                    _ => None,
                }
            }
            other => Some(other),
        })
        .collect();
    Ok((Class { negated, items }, j))
}

/// Whether `tokens` match all of `text` (see [`units`]), simulated as an automaton so that no
/// pattern takes exponential time. State `i` is "before token `i`", state `n + 1 + i` is
/// "inside the star token `i`". In `pathname` mode only `**` and a `/` match a slash.
fn wildmatch<K: NameKey>(tokens: &[Token], text: &[&[char]], pathname: bool) -> bool {
    let n = tokens.len();
    let mut current = vec![false; 2 * n + 1];
    let mut next = vec![false; 2 * n + 1];
    let mut pending = Vec::new();
    enter(tokens, &mut current, 0, &mut pending);
    for &unit in text {
        next.fill(false);
        let mut alive = false;
        let is_slash = *unit == ['/'];
        let slash_blocked = pathname && is_slash;
        for (i, token) in tokens.iter().enumerate() {
            if current[i] {
                let advance = match token {
                    Token::Literal(literal) => **literal == *unit,
                    Token::Slash => is_slash,
                    Token::AnyChar => !slash_blocked,
                    Token::Class(class) => !slash_blocked && class.matches::<K>(unit),
                    Token::Star | Token::DoubleStar => false,
                };
                if advance {
                    enter(tokens, &mut next, i + 1, &mut pending);
                    alive = true;
                }
            }
            if current[n + 1 + i] && !(slash_blocked && *token == Token::Star) {
                next[n + 1 + i] = true;
                enter(tokens, &mut next, i + 1, &mut pending);
                alive = true;
            }
        }
        if !alive {
            return false;
        }
        std::mem::swap(&mut current, &mut next);
    }
    current[n]
}

/// Adds state `start` and the states reachable from it without reading a character.
fn enter(tokens: &[Token], states: &mut [bool], start: usize, pending: &mut Vec<usize>) {
    let n = tokens.len();
    pending.push(start);
    while let Some(i) = pending.pop() {
        if states[i] {
            continue;
        }
        states[i] = true;
        match tokens.get(i) {
            Some(Token::Star) => {
                states[n + 1 + i] = true;
                pending.push(i + 1);
            }
            Some(Token::DoubleStar) => {
                states[n + 1 + i] = true;
                pending.push(i + 1);
                if tokens.get(i + 1) == Some(&Token::Slash) {
                    pending.push(i + 2);
                }
            }
            _ => {}
        }
    }
}
