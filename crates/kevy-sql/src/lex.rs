//! The lexer: SQL text → tokens with line/column anchors.
//!
//! Case-insensitive keywords (handled by folding unquoted identifiers
//! to ASCII lowercase — PG's own folding rule), `--` line comments,
//! `/* */` block comments, `'…'` string literals (with `''` escape),
//! `"…"` quoted identifiers (case preserved), `$N` parameters.

use crate::SqlError;

/// One lexical token.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Tok {
    /// Unquoted identifier, folded to ASCII lowercase (PG folding).
    Ident(String),
    /// `"quoted"` identifier, case preserved. Never a keyword.
    QIdent(String),
    /// Numeric literal, original text (`42`, `3.5`).
    Num(String),
    /// `'string'` literal, unescaped.
    Str(String),
    /// `$N` parameter, 1-based.
    Param(u32),
    /// Single-character symbol: `( ) , ; * . -`.
    Sym(char),
    /// Operator: `=` `<` `>` `<=` `>=` `<>` `!=`.
    Op(&'static str),
    /// End of input.
    Eof,
}

/// A token plus its 1-based source anchor.
#[derive(Debug, Clone)]
pub(crate) struct Token {
    pub(crate) tok: Tok,
    pub(crate) line: u32,
    pub(crate) col: u32,
}

struct Lexer<'a> {
    b: &'a [u8],
    i: usize,
    line: u32,
    col: u32,
}

impl<'a> Lexer<'a> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn peek2(&self) -> Option<u8> {
        self.b.get(self.i + 1).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let c = self.peek()?;
        self.i += 1;
        if c == b'\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(c)
    }

    fn err(&self, line: u32, col: u32, msg: impl Into<String>) -> SqlError {
        SqlError::at(line, col, msg)
    }

    /// Skip whitespace and both comment forms; `Err` on an unterminated
    /// block comment.
    fn skip_trivia(&mut self) -> Result<(), SqlError> {
        loop {
            match self.peek() {
                Some(c) if c.is_ascii_whitespace() => {
                    self.bump();
                }
                // A line starting with `\` is a psql meta-command
                // (pg_dump 18 writes \restrict/\unrestrict) — not SQL,
                // skipped like a comment. Column 1 only: a backslash
                // anywhere else is still an error.
                Some(b'\\') if self.col == 1 => {
                    while let Some(c) = self.peek() {
                        if c == b'\n' {
                            break;
                        }
                        self.bump();
                    }
                }
                Some(b'-') if self.peek2() == Some(b'-') => {
                    while let Some(c) = self.peek() {
                        if c == b'\n' {
                            break;
                        }
                        self.bump();
                    }
                }
                Some(b'/') if self.peek2() == Some(b'*') => {
                    let (l, c) = (self.line, self.col);
                    self.bump();
                    self.bump();
                    loop {
                        if self.peek() == Some(b'*') && self.peek2() == Some(b'/') {
                            self.bump();
                            self.bump();
                            break;
                        }
                        if self.bump().is_none() {
                            return Err(self.err(l, c, "unterminated /* block comment"));
                        }
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    /// `'…'` with `''` escaping; `Err` when the closing quote is missing.
    /// Bytes are collected raw, so multi-byte UTF-8 passes through
    /// intact (the delimiters are ASCII and cannot occur mid-sequence).
    fn string(&mut self, l: u32, c: u32) -> Result<Tok, SqlError> {
        self.bump(); // opening '
        let mut s = Vec::new();
        loop {
            match self.bump() {
                Some(b'\'') if self.peek() == Some(b'\'') => {
                    self.bump();
                    s.push(b'\'');
                }
                Some(b'\'') => {
                    return Ok(Tok::Str(String::from_utf8(s).expect("input is &str")));
                }
                Some(ch) => s.push(ch),
                None => return Err(self.err(l, c, "unterminated '…' string literal")),
            }
        }
    }

    /// `"…"` quoted identifier, case preserved.
    fn quoted_ident(&mut self, l: u32, c: u32) -> Result<Tok, SqlError> {
        self.bump(); // opening "
        let mut s = Vec::new();
        loop {
            match self.bump() {
                Some(b'"') => {
                    if s.is_empty() {
                        return Err(self.err(l, c, "empty \"\" quoted identifier"));
                    }
                    return Ok(Tok::QIdent(String::from_utf8(s).expect("input is &str")));
                }
                Some(ch) => s.push(ch),
                None => return Err(self.err(l, c, "unterminated \"…\" quoted identifier")),
            }
        }
    }

    /// `$N` parameter; `$0` and non-numeric `$` are named errors.
    fn param(&mut self, l: u32, c: u32) -> Result<Tok, SqlError> {
        self.bump(); // $
        let mut digits = String::new();
        while let Some(ch) = self.peek() {
            if !ch.is_ascii_digit() {
                break;
            }
            digits.push(ch as char);
            self.bump();
        }
        if digits.is_empty() {
            return Err(self.err(l, c, "'$' must be followed by a parameter number ($1, $2, …)"));
        }
        match digits.parse::<u32>() {
            Ok(n) if n >= 1 => Ok(Tok::Param(n)),
            Ok(_) => Err(self.err(l, c, "parameters are 1-based ($1 is the first)")),
            Err(_) => Err(self.err(l, c, format!("parameter number ${digits} is out of range"))),
        }
    }

    fn number(&mut self) -> Tok {
        let mut s = String::new();
        while let Some(ch) = self.peek() {
            if !ch.is_ascii_digit() {
                break;
            }
            s.push(ch as char);
            self.bump();
        }
        if self.peek() == Some(b'.') && self.peek2().is_some_and(|d| d.is_ascii_digit()) {
            s.push('.');
            self.bump();
            while let Some(ch) = self.peek() {
                if !ch.is_ascii_digit() {
                    break;
                }
                s.push(ch as char);
                self.bump();
            }
        }
        Tok::Num(s)
    }

    fn ident(&mut self) -> Tok {
        let mut s = String::new();
        while let Some(ch) = self.peek() {
            if !(ch.is_ascii_alphanumeric() || ch == b'_') {
                break;
            }
            s.push(ch.to_ascii_lowercase() as char);
            self.bump();
        }
        Tok::Ident(s)
    }

    /// One operator / symbol token; `Err` names an unexpected character.
    fn op_or_sym(&mut self, l: u32, c: u32) -> Result<Tok, SqlError> {
        let ch = self.bump().expect("caller checked peek");
        let two = |lx: &mut Lexer<'_>, op: &'static str| {
            lx.bump();
            Ok(Tok::Op(op))
        };
        match (ch, self.peek()) {
            (b'<', Some(b'=')) => two(self, "<="),
            (b'<', Some(b'>')) => two(self, "<>"),
            (b'>', Some(b'=')) => two(self, ">="),
            (b'!', Some(b'=')) => two(self, "!="),
            (b'<', _) => Ok(Tok::Op("<")),
            (b'>', _) => Ok(Tok::Op(">")),
            (b'=', _) => Ok(Tok::Op("=")),
            (b'(' | b')' | b',' | b';' | b'*' | b'.' | b'-' | b'+' | b'/', _) => {
                Ok(Tok::Sym(ch as char))
            }
            (b':', Some(b':')) => two(self, "::"),
            (b':', _) => Err(self.err(
                l,
                c,
                "a lone ':' is not SQL — the cast operator is '::'",
            )),
            (b'`', _) => Err(self.err(
                l,
                c,
                "backtick-quoted identifiers are MySQL syntax — use \"double quotes\" (or no quotes)",
            )),
            _ if ch.is_ascii() => {
                Err(self.err(l, c, format!("unexpected character '{}'", ch as char)))
            }
            _ => Err(self.err(l, c, format!("unexpected byte 0x{ch:02X}"))),
        }
    }
}

/// Lex `src` into tokens (terminated by [`Tok::Eof`]).
pub(crate) fn lex(src: &str) -> Result<Vec<Token>, SqlError> {
    let mut lx = Lexer { b: src.as_bytes(), i: 0, line: 1, col: 1 };
    let mut out = Vec::new();
    loop {
        lx.skip_trivia()?;
        let (line, col) = (lx.line, lx.col);
        let Some(c) = lx.peek() else {
            out.push(Token { tok: Tok::Eof, line, col });
            return Ok(out);
        };
        let tok = match c {
            b'\'' => lx.string(line, col)?,
            b'"' => lx.quoted_ident(line, col)?,
            b'$' => lx.param(line, col)?,
            _ if c.is_ascii_digit() => lx.number(),
            _ if c.is_ascii_alphabetic() || c == b'_' => lx.ident(),
            _ => lx.op_or_sym(line, col)?,
        };
        out.push(Token { tok, line, col });
    }
}
