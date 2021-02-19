/*
 * Copyright 2018 The Starlark in Rust Authors.
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use crate::{
    errors::Diagnostic,
    syntax::{cursors::CursorBytes, dialect::Dialect},
};
use codemap::{CodeMap, Span};
use gazebo::dupe::Dupe;
use logos::Logos;
use std::{char, collections::VecDeque, fmt, fmt::Display, iter::Peekable, sync::Arc};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum LexemeError {
    #[error("Parse error: incorrect indentation")]
    Indentation,
    #[error("Parse error: Character not valid at present location")]
    InvalidCharacter,
    #[error("Parse error: tabs are not allowed in the dialect")]
    InvalidTab,
    #[error("Parse error: unfinished string literal")]
    UnfinishedStringLiteral,
    #[error("Parse error: invalid string escape sequence")]
    InvalidEscapeSequence,
}

type Lexeme = anyhow::Result<(u64, Token, u64)>;

pub(crate) struct Lexer<'a> {
    // Information for spans
    codemap: Arc<CodeMap>,
    filespan: Span,
    // Other info
    indent_levels: Vec<usize>,
    /// Lexemes that have been generated but not yet returned
    buffer: VecDeque<Lexeme>,
    parens: isize, // Number of parens we have seen
    lexer: logos::Lexer<'a, Token>,
    done: bool,
    dialect_allow_tabs: bool,
}

fn enumerate_chars(x: impl Iterator<Item = char>) -> impl Iterator<Item = (usize, char)> {
    x.scan(0, |state, c| {
        *state += c.len_utf8();
        Some((*state, c))
    })
}

impl<'a> Lexer<'a> {
    pub fn new(input: &'a str, dialect: &Dialect, codemap: Arc<CodeMap>, filespan: Span) -> Self {
        let lexer = Token::lexer(input);
        let mut lexer2 = Self {
            codemap,
            filespan,
            // Aim to size all the buffers such that they never resize
            indent_levels: Vec::with_capacity(20),
            buffer: VecDeque::with_capacity(10),
            lexer,
            parens: 0,
            done: false,
            dialect_allow_tabs: dialect.enable_tabs,
        };
        if let Err(e) = lexer2.calculate_indent() {
            lexer2.buffer.push_back(Err(e));
        }
        lexer2
    }

    fn err_pos<T>(&self, msg: LexemeError, pos: u64) -> anyhow::Result<T> {
        self.err_span(msg, pos, pos)
    }

    fn err_span<T>(&self, msg: LexemeError, start: u64, end: u64) -> anyhow::Result<T> {
        Err(Diagnostic::add_span(
            msg,
            self.filespan.subspan(start, end),
            self.codemap.dupe(),
        ))
    }

    /// We have just seen a newline, read how many indents we have
    /// and then set self.indent properly
    fn calculate_indent(&mut self) -> anyhow::Result<()> {
        // consume tabs and spaces, output the indentation levels
        let mut it = CursorBytes::new(self.lexer.remainder());
        let mut spaces = 0;
        let mut tabs = 0;
        let mut indent_start = self.lexer.span().start as u64;
        loop {
            match it.next_char() {
                None => {
                    self.lexer.bump(it.pos());
                    return Ok(());
                }
                Some(' ') => {
                    spaces += 1;
                }
                Some('\t') => {
                    tabs += 1;
                }
                Some('\n') => {
                    // A line that is entirely blank gets emitted as a newline, and then
                    // we don't consume the subsequent newline character.
                    self.lexer.bump(it.pos() - 1);
                    return Ok(());
                }
                Some('#') => {
                    // A line that is all comments doesn't get emitted at all
                    // Skip until the next newline
                    // Remove skip now, so we can freely add it on later
                    spaces = 0;
                    tabs = 0;
                    loop {
                        match it.next_char() {
                            None => {
                                self.lexer.bump(it.pos());
                                return Ok(());
                            }
                            Some('\n') => break, // only the inner loop
                            Some(_) => {}
                        }
                    }
                    indent_start = self.lexer.span().start as u64 + it.pos() as u64;
                }
                _ => break,
            }
        }
        self.lexer.bump(it.pos() - 1); // last character broke us out the loop
        let indent = spaces + tabs * 8;
        if tabs > 0 && !self.dialect_allow_tabs {
            return self.err_pos(LexemeError::InvalidTab, self.lexer.span().start as u64);
        }
        let now = self.indent_levels.last().copied().unwrap_or(0);

        if indent > now {
            self.indent_levels.push(indent);
            let span = self.lexer.span();
            self.buffer.push_back(Ok((
                indent_start as u64 + 1,
                Token::Indent,
                span.end as u64,
            )));
        } else if indent < now {
            let mut dedents = 1;
            self.indent_levels.pop().unwrap();
            loop {
                let now = self.indent_levels.last().copied().unwrap_or(0);
                if now == indent {
                    break;
                } else if now > indent {
                    dedents += 1;
                    self.indent_levels.pop().unwrap();
                } else {
                    let pos = self.lexer.span();
                    return self.err_span(
                        LexemeError::Indentation,
                        pos.start as u64,
                        pos.end as u64,
                    );
                }
            }
            for _ in 0..dedents {
                // We must declare each dedent is only a position, so multiple adjacent dedents don't overlap
                self.buffer.push_back(Ok((
                    indent_start as u64 + 1,
                    Token::Dedent,
                    indent_start as u64 + 1,
                )))
            }
        }
        Ok(())
    }

    fn wrap(&mut self, token: Token) -> Option<Lexeme> {
        let span = self.lexer.span();
        Some(Ok((span.start as u64, token, span.end as u64)))
    }

    fn consume_int_r(
        it: &mut Peekable<impl Iterator<Item = (usize, char)>>,
        radix: u32,
    ) -> Result<i32, ()> {
        let mut number = String::new();
        while it.peek().map_or(false, |x| x.1.is_digit(radix)) {
            number.push(it.next().unwrap().1);
        }
        let val = i32::from_str_radix(&number, radix);
        match val {
            Err(_) => Err(()),
            Ok(v) => Ok(v),
        }
    }

    // We have seen a '\' character, now parse what comes next
    fn escape(
        &self,
        it: &mut Peekable<impl Iterator<Item = (usize, char)>>,
        pos: usize,
        res: &mut String,
    ) -> anyhow::Result<()> {
        if let Some((pos2, c2)) = it.next() {
            match c2 {
                'n' => {
                    res.push('\n');
                    Ok(())
                }
                'r' => {
                    res.push('\r');
                    Ok(())
                }
                't' => {
                    res.push('\t');
                    Ok(())
                }
                '0' => {
                    if it.peek().map_or(false, |x| x.1.is_digit(8)) {
                        if let Ok(r) = Self::consume_int_r(it, 8) {
                            res.push(char::from_u32(r as u32).unwrap());
                            Ok(())
                        } else {
                            self.err_span(
                                LexemeError::InvalidEscapeSequence,
                                pos as u64,
                                pos2 as u64,
                            )
                        }
                    } else {
                        res.push('\0');
                        Ok(())
                    }
                }
                'x' => {
                    if let Ok(r) = Self::consume_int_r(it, 16) {
                        res.push(char::from_u32(r as u32).unwrap());
                        Ok(())
                    } else {
                        self.err_span(LexemeError::InvalidEscapeSequence, pos as u64, pos2 as u64)
                    }
                }
                '1'..='9' => {
                    self.err_span(LexemeError::InvalidEscapeSequence, pos as u64, pos2 as u64)
                }
                'u' => match it.next() {
                    Some((_, '{')) => {
                        if let Ok(r) = Self::consume_int_r(it, 16) {
                            if let Some((_, '}')) = it.next() {
                                res.push(char::from_u32(r as u32).unwrap());
                                return Ok(());
                            }
                        }
                        self.err_span(LexemeError::InvalidEscapeSequence, pos as u64, pos2 as u64)
                    }
                    _ => self.err_span(LexemeError::InvalidEscapeSequence, pos as u64, pos2 as u64),
                },
                '"' | '\'' | '\\' => {
                    res.push(c2);
                    Ok(())
                }
                '\n' => Ok(()),
                _ => {
                    res.push('\\');
                    res.push(c2);
                    Ok(())
                }
            }
        } else {
            self.err_pos(LexemeError::UnfinishedStringLiteral, pos as u64)
        }
    }

    // String parsing is a hot-spot, so parameterise by a `stop` function which gets
    // specialised for each variant
    fn string(
        &mut self,
        triple: bool,
        raw: bool,
        mut stop: impl FnMut(char) -> bool,
    ) -> Option<Lexeme> {
        // We have seen an openning quote, which is either ' or "
        // If triple is true, it was a triple quote
        // stop lets us know when a string ends.

        let mut s = self.lexer.remainder().as_bytes();
        if triple {
            s = &s[2..];
        }

        let mut res = String::new();
        let mut adjust = 0;
        let mut s_rest = self.lexer.remainder();
        let start = self.lexer.span().start as u64 + if raw { 1 } else { 0 };
        // Take the fast path as long as the result is a slice of the original, with no changes.
        for (i, c) in s.iter().map(|c| *c as char).enumerate() {
            if stop(c) {
                let str = if triple {
                    self.lexer.remainder()[2..i].to_owned()
                } else {
                    self.lexer.remainder()[..i].to_owned()
                };
                self.lexer.bump(i + 1 + if triple { 2 } else { 0 });
                return Some(Ok((
                    start,
                    Token::StringLiteral(str),
                    start + i as u64 + if triple { 4 } else { 2 },
                )));
            } else if c == '\\' || c == '\r' || (c == '\n' && !triple) {
                res = String::with_capacity(i + 10);
                res.push_str(
                    &self.lexer.remainder()
                        [if triple { 2 } else { 0 }..if triple { 2 } else { 0 } + i],
                );
                adjust = i;
                s_rest = &self.lexer.remainder()[if triple { 2 } else { 0 } + i..];
                break;
            }
        }

        // We bailed out of the fast path, that means we now accumulate character by character,
        // might have an error, run out of characters or be dealing with escape characters.
        let mut it = enumerate_chars(s_rest.chars()).peekable();
        while let Some((i, c)) = it.next() {
            if stop(c) {
                self.lexer.bump(adjust + i + if triple { 2 } else { 0 });
                if triple {
                    res.truncate(res.len() - 2);
                }
                return Some(Ok((
                    start,
                    Token::StringLiteral(res),
                    start + adjust as u64 + i as u64 + if triple { 3 } else { 1 },
                )));
            }
            match c {
                '\n' if !triple => {
                    break; // Will raise an error about out of chars
                }
                '\r' => {
                    // We just ignore these in all modes
                }
                '\\' => {
                    if raw {
                        match it.next() {
                            Some((_, c)) => {
                                if c == '\'' || c == '"' {
                                    res.push(c);
                                } else {
                                    res.push('\\');
                                    res.push(c);
                                }
                            }
                            _ => break, // Out of chars
                        }
                    } else if let Err(e) = self.escape(&mut it, i, &mut res) {
                        return Some(Err(e));
                    }
                }
                c => res.push(c),
            }
        }

        // We ran out of characters
        Some(self.err_span(LexemeError::UnfinishedStringLiteral, start, start + 1))
    }

    pub fn next(&mut self) -> Option<Lexeme> {
        loop {
            // Note that this function doesn't always return - a few branches use `continue`
            // to always go round the loop again.
            return if let Some(x) = self.buffer.pop_front() {
                Some(x)
            } else if self.done {
                None
            } else {
                match self.lexer.next() {
                    None => {
                        self.done = true;
                        let pos = self.lexer.span().end as u64;
                        for _ in 0..self.indent_levels.len() {
                            self.buffer.push_back(Ok((pos, Token::Dedent, pos)))
                        }
                        self.indent_levels.clear();
                        self.wrap(Token::Newline)
                    }
                    Some(token) => match token {
                        Token::Tabs => {
                            if !self.dialect_allow_tabs {
                                self.buffer.push_back(self.err_pos(
                                    LexemeError::InvalidTab,
                                    self.lexer.span().start as u64,
                                ));
                            }
                            continue;
                        }
                        Token::Newline => {
                            if self.parens == 0 {
                                let span = self.lexer.span();
                                if let Err(e) = self.calculate_indent() {
                                    return Some(Err(e));
                                }
                                Some(Ok((span.start as u64, Token::Newline, span.end as u64)))
                            } else {
                                continue;
                            }
                        }
                        Token::Error => Some(self.err_span(
                            LexemeError::InvalidCharacter,
                            self.lexer.span().start as u64,
                            self.lexer.span().end as u64,
                        )),
                        Token::RawDoubleQuote => {
                            let raw = self.lexer.span().len() == 2;
                            if self.lexer.remainder().starts_with("\"\"") {
                                let mut qs = 0;
                                self.string(true, raw, |c| {
                                    if c == '\"' {
                                        qs += 1;
                                        qs == 3
                                    } else {
                                        qs = 0;
                                        false
                                    }
                                })
                            } else {
                                self.string(false, raw, |c| c == '\"')
                            }
                        }
                        Token::RawSingleQuote => {
                            let raw = self.lexer.span().len() == 2;
                            if self.lexer.remainder().starts_with("''") {
                                let mut qs = 0;
                                self.string(true, raw, |c| {
                                    if c == '\'' {
                                        qs += 1;
                                        qs == 3
                                    } else {
                                        qs = 0;
                                        false
                                    }
                                })
                            } else {
                                self.string(false, raw, |c| c == '\'')
                            }
                        }
                        Token::OpeningCurly | Token::OpeningRound | Token::OpeningSquare => {
                            self.parens += 1;
                            self.wrap(token)
                        }
                        Token::ClosingCurly | Token::ClosingRound | Token::ClosingSquare => {
                            self.parens -= 1;
                            self.wrap(token)
                        }
                        _ => self.wrap(token),
                    },
                }
            };
        }
    }
}

/// All token that can be generated by the lexer
#[derive(Logos, Debug, Clone, PartialEq)]
pub enum Token {
    #[regex(" +", logos::skip)] // Whitespace
    #[token("\\\n", logos::skip)] // Escaped newline
    #[token("\\\r\n", logos::skip)] // Escaped newline (Windows line ending)
    #[regex(r#"#[^\n]*"#, logos::skip)] // Comments
    #[error]
    Error,

    #[regex("\t+")] // Tabs (might be an error)
    Tabs,

    // Indentation block & meaningfull spaces
    Indent, // New indentation block
    Dedent, // Leaving an indentation block
    #[regex(r"(\r)?\n")]
    Newline, // Newline outside a string

    // Some things the lexer can't deal with well, so we step in and generate
    // things ourselves
    #[token("'")]
    #[token("r'")]
    RawSingleQuote,
    #[token("\"")]
    #[token("r\"")]
    RawDoubleQuote,

    #[regex(
        "as|import|is|class|nonlocal|del|raise|except|try|finally|while|from|with|global|yield"
    , |lex| lex.slice().to_owned())]
    Reserved(String), // One of the reserved keywords

    #[regex(
        "[a-zA-Z_][a-zA-Z0-9_]*"
    , |lex| lex.slice().to_owned())]
    Identifier(String), // An identifier

    #[regex(
        "[0-9]+"
    , |lex|
        if lex.slice().len() > 1 && &lex.slice()[0..1] == "0" {
            // Hack to make it return an error
            "".parse()
        } else {
            lex.slice().parse()
        }
    )]
    #[regex(
        "0[xX][A-Fa-f0-9]+"
    , |lex| i32::from_str_radix(&lex.slice()[2..], 16))]
    #[regex(
        "0[bB][01]+"
    , |lex| i32::from_str_radix(&lex.slice()[2..], 2))]
    #[regex(
        "0[oO][0-7]+"
    , |lex| i32::from_str_radix(&lex.slice()[2..], 8))]
    IntegerLiteral(i32), // An integer literal (123, 0x1, 0b1011, 0o755, ...)

    StringLiteral(String), // A string literal

    // Keywords
    #[token("and")]
    And,
    #[token("else")]
    Else,
    #[token("load")]
    Load,
    #[token("break")]
    Break,
    #[token("for")]
    For,
    #[token("not")]
    Not,
    #[token("continue")]
    Continue,
    #[token("if")]
    If,
    #[token("or")]
    Or,
    #[token("def")]
    Def,
    #[token("in")]
    In,
    #[token("pass")]
    Pass,
    #[token("elif")]
    Elif,
    #[token("return")]
    Return,
    #[token("lambda")]
    Lambda,
    // Symbols
    #[token(",")]
    Comma,
    #[token(";")]
    Semicolon,
    #[token(":")]
    Colon,
    #[token("+=")]
    PlusEqual,
    #[token("-=")]
    MinusEqual,
    #[token("*=")]
    StarEqual,
    #[token("/=")]
    SlashEqual,
    #[token("//=")]
    SlashSlashEqual,
    #[token("%=")]
    PercentEqual,
    #[token("==")]
    EqualEqual,
    #[token("!=")]
    BangEqual,
    #[token("<=")]
    LessEqual,
    #[token(">=")]
    GreaterEqual,
    #[token("**")]
    StarStar,
    #[token("->")]
    RightArrow,
    #[token("=")]
    Equal,
    #[token("<")]
    LessThan,
    #[token(">")]
    GreaterThan,
    #[token("-")]
    Minus,
    #[token("+")]
    Plus,
    #[token("*")]
    Star,
    #[token("%")]
    Percent,
    #[token("/")]
    Slash,
    #[token("//")]
    SlashSlash,
    #[token(".")]
    Dot,
    #[token("|")]
    Pipe,

    // Brackets
    #[token("[")]
    OpeningSquare,
    #[token("{")]
    OpeningCurly,
    #[token("(")]
    OpeningRound,
    #[token("]")]
    ClosingSquare,
    #[token("}")]
    ClosingCurly,
    #[token(")")]
    ClosingRound,
}

impl Token {
    /// Used for testing
    pub(crate) fn unlex(&self) -> String {
        match self {
            Token::Indent => "\t".to_owned(),
            Token::Newline => "\n".to_owned(),
            Token::Dedent => "#dedent".to_owned(),
            Token::StringLiteral(x) => format!("{:?}", x),
            _ => {
                let s = self.to_string();
                let first = s.find('\'');
                match first {
                    Some(first) if s.ends_with('\'') && first != s.len() - 1 => {
                        s[first + 1..s.len() - 1].to_owned()
                    }
                    _ => s,
                }
            }
        }
    }
}

impl Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Token::Error => write!(f, "lexical error"),
            Token::Indent => write!(f, "new indentation block"),
            Token::Dedent => write!(f, "end of indentation block"),
            Token::Newline => write!(f, "new line"),
            Token::And => write!(f, "keyword 'and'"),
            Token::Else => write!(f, "keyword 'else'"),
            Token::Load => write!(f, "keyword 'load'"),
            Token::Break => write!(f, "keyword 'break'"),
            Token::For => write!(f, "keyword 'for'"),
            Token::Not => write!(f, "keyword 'not'"),
            Token::Continue => write!(f, "keyword 'continue'"),
            Token::If => write!(f, "keyword 'if'"),
            Token::Or => write!(f, "keyword 'or'"),
            Token::Def => write!(f, "keyword 'def'"),
            Token::In => write!(f, "keyword 'in'"),
            Token::Pass => write!(f, "keyword 'pass'"),
            Token::Elif => write!(f, "keyword 'elif'"),
            Token::Return => write!(f, "keyword 'return'"),
            Token::Lambda => write!(f, "keyword 'lambda'"),
            Token::Comma => write!(f, "symbol ','"),
            Token::Semicolon => write!(f, "symbol ';'"),
            Token::Colon => write!(f, "symbol ':'"),
            Token::PlusEqual => write!(f, "symbol '+='"),
            Token::MinusEqual => write!(f, "symbol '-='"),
            Token::StarEqual => write!(f, "symbol '*='"),
            Token::SlashEqual => write!(f, "symbol '/='"),
            Token::SlashSlashEqual => write!(f, "symbol '//='"),
            Token::PercentEqual => write!(f, "symbol '%='"),
            Token::EqualEqual => write!(f, "symbol '=='"),
            Token::BangEqual => write!(f, "symbol '!='"),
            Token::LessEqual => write!(f, "symbol '<='"),
            Token::GreaterEqual => write!(f, "symbol '>='"),
            Token::StarStar => write!(f, "symbol '**'"),
            Token::RightArrow => write!(f, "symbol '->'"),
            Token::Equal => write!(f, "symbol '='"),
            Token::LessThan => write!(f, "symbol '<'"),
            Token::GreaterThan => write!(f, "symbol '>'"),
            Token::Minus => write!(f, "symbol '-'"),
            Token::Plus => write!(f, "symbol '+'"),
            Token::Star => write!(f, "symbol '*'"),
            Token::Percent => write!(f, "symbol '%'"),
            Token::Slash => write!(f, "symbol '/'"),
            Token::SlashSlash => write!(f, "symbol '//'"),
            Token::Dot => write!(f, "symbol '.'"),
            Token::Pipe => write!(f, "symbol '|'"),
            Token::OpeningSquare => write!(f, "symbol '['"),
            Token::OpeningCurly => write!(f, "symbol '{{'"),
            Token::OpeningRound => write!(f, "symbol '('"),
            Token::ClosingSquare => write!(f, "symbol ']'"),
            Token::ClosingCurly => write!(f, "symbol '}}'"),
            Token::ClosingRound => write!(f, "symbol ')'"),
            Token::Reserved(s) => write!(f, "reserved keyword '{}'", s),
            Token::Identifier(s) => write!(f, "identifier '{}'", s),
            Token::IntegerLiteral(i) => write!(f, "integer literal '{}'", i),
            Token::StringLiteral(s) => write!(f, "string literal '{}'", s),
            Token::RawSingleQuote => write!(f, "starting '"),
            Token::RawDoubleQuote => write!(f, "starting \""),
            Token::Tabs => Ok(()),
        }
    }
}

impl<'a> Iterator for Lexer<'a> {
    type Item = Lexeme;

    fn next(&mut self) -> Option<Self::Item> {
        self.next()
    }
}
