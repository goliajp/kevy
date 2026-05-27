//! TOML subset tokenizer. Splits the input into [`Token`]s the parser
//! consumes; tracks `(line, col)` for diagnostics.
//!
//! Subset covered: see [`crate`] top-level doc.

use crate::schema::ConfigError;

/// One token from the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Token {
    /// `[` — section header start.
    LBracket,
    /// `]` — section header end.
    RBracket,
    /// `=` — key/value separator.
    Equals,
    /// `\n` — statement terminator. Multiple newlines compress here for
    /// the parser to consume one at a time.
    Newline,
    /// Bare identifier (`key`, `section`). TOML allows `A-Za-z0-9_-`.
    Ident(String),
    /// Quoted string literal. Both `"..."` and `'...'` produce this.
    Str(String),
    /// Decimal integer literal (signed). Out-of-i64 is a parse error.
    Int(i64),
    /// `true` / `false`.
    Bool(bool),
}

/// One token plus its `(line, col)` source position (1-based).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Spanned {
    pub(crate) tok: Token,
    pub(crate) line: usize,
    pub(crate) col: usize,
}

/// Tokenize the full input into a vec of spanned tokens. Comments and
/// inline whitespace are dropped; newlines are kept (one per source line
/// after a `# comment` is stripped).
pub(crate) fn tokenize(src: &str) -> Result<Vec<Spanned>, ConfigError> {
    let mut out = Vec::new();
    let mut lexer = Lexer::new(src);
    while let Some(spanned) = lexer.next_token()? {
        out.push(spanned);
    }
    Ok(out)
}

struct Lexer<'a> {
    bytes: &'a [u8],
    pos: usize,
    line: usize,
    col: usize,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            bytes: src.as_bytes(),
            pos: 0,
            line: 1,
            col: 1,
        }
    }

    fn next_token(&mut self) -> Result<Option<Spanned>, ConfigError> {
        self.skip_inline_ws_and_comments();
        let Some(&b) = self.bytes.get(self.pos) else {
            return Ok(None);
        };
        let (line, col) = (self.line, self.col);
        let tok = match b {
            b'\n' => {
                self.advance();
                Token::Newline
            }
            b'[' => {
                self.advance();
                Token::LBracket
            }
            b']' => {
                self.advance();
                Token::RBracket
            }
            b'=' => {
                self.advance();
                Token::Equals
            }
            b'"' | b'\'' => self.consume_string(b, line, col)?,
            c if c.is_ascii_digit() || c == b'-' || c == b'+' => self.consume_number(line, col)?,
            c if is_ident_start(c) => self.consume_ident_or_bool(),
            other => {
                return Err(parse_err(
                    line,
                    col,
                    format!("unexpected character {:?}", other as char),
                ));
            }
        };
        Ok(Some(Spanned { tok, line, col }))
    }

    fn advance(&mut self) {
        let b = self.bytes[self.pos];
        self.pos += 1;
        if b == b'\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
    }

    fn skip_inline_ws_and_comments(&mut self) {
        loop {
            while let Some(&b) = self.bytes.get(self.pos) {
                if b == b' ' || b == b'\t' || b == b'\r' {
                    self.advance();
                } else {
                    break;
                }
            }
            // Comments run to end-of-line; we leave the `\n` for the next
            // token call so the parser sees the statement terminator.
            if let Some(b'#') = self.bytes.get(self.pos).copied() {
                while let Some(&b) = self.bytes.get(self.pos) {
                    if b == b'\n' {
                        break;
                    }
                    self.advance();
                }
                continue;
            }
            return;
        }
    }

    fn consume_string(&mut self, quote: u8, line: usize, col: usize) -> Result<Token, ConfigError> {
        self.advance(); // consume opening quote
        let mut buf = String::new();
        loop {
            let Some(&b) = self.bytes.get(self.pos) else {
                return Err(parse_err(line, col, "unterminated string"));
            };
            if b == quote {
                self.advance();
                return Ok(Token::Str(buf));
            }
            if b == b'\n' {
                return Err(parse_err(
                    line,
                    col,
                    "newline inside string (multi-line strings not supported)",
                ));
            }
            if b == b'\\' && quote == b'"' {
                self.advance();
                let Some(&esc) = self.bytes.get(self.pos) else {
                    return Err(parse_err(line, col, "unterminated escape"));
                };
                let decoded = match esc {
                    b'n' => '\n',
                    b't' => '\t',
                    b'r' => '\r',
                    b'\\' => '\\',
                    b'"' => '"',
                    b'0' => '\0',
                    other => {
                        return Err(parse_err(
                            self.line,
                            self.col,
                            format!("unsupported escape \\{}", other as char),
                        ));
                    }
                };
                buf.push(decoded);
                self.advance();
                continue;
            }
            buf.push(b as char);
            self.advance();
        }
    }

    fn consume_number(&mut self, line: usize, col: usize) -> Result<Token, ConfigError> {
        let start = self.pos;
        if matches!(self.bytes.get(self.pos), Some(b'-' | b'+')) {
            self.advance();
        }
        while let Some(&b) = self.bytes.get(self.pos) {
            if b.is_ascii_digit() || b == b'_' {
                self.advance();
            } else {
                break;
            }
        }
        let raw: String = self.bytes[start..self.pos]
            .iter()
            .filter(|&&b| b != b'_')
            .map(|&b| b as char)
            .collect();
        let n: i64 = raw
            .parse()
            .map_err(|_| parse_err(line, col, format!("invalid integer {raw:?}")))?;
        Ok(Token::Int(n))
    }

    fn consume_ident_or_bool(&mut self) -> Token {
        let start = self.pos;
        while let Some(&b) = self.bytes.get(self.pos) {
            if is_ident_cont(b) {
                self.advance();
            } else {
                break;
            }
        }
        let raw = std::str::from_utf8(&self.bytes[start..self.pos])
            .expect("kevy-config: TOML source must be UTF-8 (caller guarantees this)")
            .to_owned();
        match raw.as_str() {
            "true" => Token::Bool(true),
            "false" => Token::Bool(false),
            _ => Token::Ident(raw),
        }
    }
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_cont(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

fn parse_err(line: usize, col: usize, msg: impl Into<String>) -> ConfigError {
    ConfigError::Parse {
        line,
        col,
        msg: msg.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<Token> {
        tokenize(src)
            .unwrap()
            .into_iter()
            .map(|s| s.tok)
            .collect()
    }

    #[test]
    fn empty_input() {
        assert!(toks("").is_empty());
        assert!(toks("   \t\r ").is_empty());
        assert_eq!(toks("\n"), vec![Token::Newline]);
    }

    #[test]
    fn section_header() {
        assert_eq!(
            toks("[server]"),
            vec![Token::LBracket, Token::Ident("server".into()), Token::RBracket]
        );
    }

    #[test]
    fn key_int_value() {
        assert_eq!(
            toks("port = 6004"),
            vec![Token::Ident("port".into()), Token::Equals, Token::Int(6004)]
        );
    }

    #[test]
    fn key_bool_value() {
        assert_eq!(
            toks("aof = false"),
            vec![Token::Ident("aof".into()), Token::Equals, Token::Bool(false)]
        );
    }

    #[test]
    fn quoted_strings() {
        assert_eq!(toks("\"hello\""), vec![Token::Str("hello".into())]);
        assert_eq!(toks("'127.0.0.1'"), vec![Token::Str("127.0.0.1".into())]);
        assert_eq!(toks("\"esc\\n\""), vec![Token::Str("esc\n".into())]);
    }

    #[test]
    fn comment_stripped() {
        let got = toks("port = 7000 # the port\n");
        assert_eq!(
            got,
            vec![
                Token::Ident("port".into()),
                Token::Equals,
                Token::Int(7000),
                Token::Newline,
            ]
        );
    }

    #[test]
    fn signed_and_underscored_ints() {
        assert_eq!(toks("-7"), vec![Token::Int(-7)]);
        assert_eq!(toks("1_000_000"), vec![Token::Int(1_000_000)]);
    }

    #[test]
    fn unterminated_string_errors() {
        let err = tokenize("\"oops").unwrap_err();
        match err {
            ConfigError::Parse { line, .. } => assert_eq!(line, 1),
            _ => panic!("expected Parse"),
        }
    }

    #[test]
    fn newline_inside_string_errors() {
        let err = tokenize("\"oops\nyikes\"").unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn unknown_char_errors() {
        let err = tokenize("@").unwrap_err();
        match err {
            ConfigError::Parse { line, col, .. } => {
                assert_eq!(line, 1);
                assert_eq!(col, 1);
            }
            _ => panic!("expected Parse"),
        }
    }

    #[test]
    fn line_col_tracking() {
        let spans = tokenize("a = 1\n[s]").unwrap();
        // Line 1: a, =, 1, newline. Line 2: [, s, ].
        assert_eq!(spans[0].line, 1);
        assert_eq!(spans[0].col, 1);
        assert_eq!(spans[4].line, 2); // `[`
        assert_eq!(spans[4].col, 1);
    }
}
