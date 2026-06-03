//! TOML subset parser. Consumes the [`crate::lex`] token stream and
//! produces a flat list of [`Item`]s — one per `key = value` statement,
//! each tagged with the most recent `[section]` header.

use crate::lex::{Spanned, Token, tokenize};
use crate::schema::ConfigError;

/// One concrete value the schema can accept after parsing. Size literals
/// stay as [`Value::Str`] until the schema field decides to convert via
/// [`crate::size::parse_size`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Value {
    /// `"text"` / `'text'`.
    Str(String),
    /// Decimal integer.
    Int(i64),
    /// `true` / `false`.
    Bool(bool),
}

/// One parsed `(section, key, value)` triple from the TOML source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Item {
    /// `[section]` header in effect when this item appears, if any.
    /// `None` = key sat at the top of the file before any section header.
    pub(crate) section: Option<String>,
    /// LHS identifier.
    pub(crate) key: String,
    /// RHS value.
    pub(crate) value: Value,
    /// 1-based source line of the key (for diagnostics).
    pub(crate) line: usize,
}

/// Parse a complete TOML source string into a flat list of items.
pub(crate) fn parse(src: &str) -> Result<Vec<Item>, ConfigError> {
    let tokens = tokenize(src)?;
    let mut p = Parser {
        tokens,
        pos: 0,
        current_section: None,
        items: Vec::new(),
    };
    p.parse_all()?;
    Ok(p.items)
}

struct Parser {
    tokens: Vec<Spanned>,
    pos: usize,
    current_section: Option<String>,
    items: Vec<Item>,
}

impl Parser {
    fn parse_all(&mut self) -> Result<(), ConfigError> {
        while self.pos < self.tokens.len() {
            match &self.tokens[self.pos].tok {
                Token::Newline => {
                    self.pos += 1;
                }
                Token::LBracket => self.parse_section_header()?,
                Token::Ident(_) => self.parse_assignment()?,
                other => {
                    let s = &self.tokens[self.pos];
                    return Err(unexpected(s, format!("{other:?}")));
                }
            }
        }
        Ok(())
    }

    fn parse_section_header(&mut self) -> Result<(), ConfigError> {
        // Already at `[`.
        self.pos += 1;
        let name = match self.tokens.get(self.pos) {
            Some(Spanned {
                tok: Token::Ident(n),
                ..
            }) => n.clone(),
            Some(s) => return Err(unexpected(s, "expected section name".into())),
            None => return Err(eof("expected section name")),
        };
        self.pos += 1;
        match self.tokens.get(self.pos) {
            Some(Spanned {
                tok: Token::RBracket,
                ..
            }) => self.pos += 1,
            Some(s) => return Err(unexpected(s, "expected ']'".into())),
            None => return Err(eof("expected ']'")),
        }
        self.expect_eol("after section header")?;
        self.current_section = Some(name);
        Ok(())
    }

    fn parse_assignment(&mut self) -> Result<(), ConfigError> {
        let key_span = self.tokens[self.pos].clone();
        // By contract, `parse_all` only dispatches here when the head is
        // `Token::Ident`. If a future refactor of the dispatcher changes
        // that, we want a structured error — not a panic in a public
        // `Config::load` call. Treat a non-Ident head as a parser
        // invariant violation reported the same way a malformed file is.
        let key = match key_span.tok {
            Token::Ident(k) => k,
            _ => return Err(unexpected(&key_span, "expected key identifier".into())),
        };
        let line = key_span.line;
        self.pos += 1;
        match self.tokens.get(self.pos) {
            Some(Spanned {
                tok: Token::Equals,
                ..
            }) => self.pos += 1,
            Some(s) => return Err(unexpected(s, "expected '='".into())),
            None => return Err(eof("expected '='")),
        }
        let value_span = self
            .tokens
            .get(self.pos)
            .cloned()
            .ok_or_else(|| eof("expected value"))?;
        let value = match value_span.tok {
            Token::Str(s) => Value::Str(s),
            Token::Int(n) => Value::Int(n),
            Token::Bool(b) => Value::Bool(b),
            ref other => {
                return Err(unexpected(
                    &value_span,
                    format!("expected value, got {other:?}"),
                ));
            }
        };
        self.pos += 1;
        self.expect_eol("after value")?;
        self.items.push(Item {
            section: self.current_section.clone(),
            key,
            value,
            line,
        });
        Ok(())
    }

    /// Expect either a `Newline` or EOF. Anything else (a second token on
    /// the same line) is rejected — we don't support multiple assignments
    /// per line.
    fn expect_eol(&mut self, ctx: &str) -> Result<(), ConfigError> {
        match self.tokens.get(self.pos) {
            None => Ok(()),
            Some(Spanned {
                tok: Token::Newline,
                ..
            }) => {
                self.pos += 1;
                Ok(())
            }
            Some(s) => Err(unexpected(s, format!("expected newline {ctx}"))),
        }
    }
}

fn unexpected(s: &Spanned, msg: String) -> ConfigError {
    ConfigError::Parse {
        line: s.line,
        col: s.col,
        msg,
    }
}

fn eof(msg: &str) -> ConfigError {
    ConfigError::Parse {
        line: 0,
        col: 0,
        msg: format!("unexpected end of input: {msg}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(src: &str) -> Vec<Item> {
        parse(src).unwrap_or_else(|e| panic!("parse failed: {e}"))
    }

    #[test]
    fn empty_input_parses_to_nothing() {
        assert!(parse_ok("").is_empty());
        assert!(parse_ok("\n\n\n").is_empty());
        assert!(parse_ok("# only comments\n# more\n").is_empty());
    }

    #[test]
    fn section_then_keys() {
        let items = parse_ok(
            "[server]\n\
             bind = \"127.0.0.1\"\n\
             port = 6004\n",
        );
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].section.as_deref(), Some("server"));
        assert_eq!(items[0].key, "bind");
        assert_eq!(items[0].value, Value::Str("127.0.0.1".into()));
        assert_eq!(items[0].line, 2);
        assert_eq!(items[1].key, "port");
        assert_eq!(items[1].value, Value::Int(6004));
    }

    #[test]
    fn key_before_any_section() {
        let items = parse_ok("foo = true\n[s]\nbar = 1\n");
        assert_eq!(items[0].section, None);
        assert_eq!(items[1].section.as_deref(), Some("s"));
    }

    #[test]
    fn last_line_without_newline() {
        let items = parse_ok("[a]\nx = 1");
        assert_eq!(items[0].key, "x");
    }

    #[test]
    fn blank_lines_between_keys_ok() {
        let items = parse_ok("[a]\n\nx = 1\n\n\ny = 2\n");
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn duplicate_section_overwrites_current_section_pointer() {
        // We don't merge sections; later `[a]` just resets the current
        // section pointer. Schema validation handles dup-key conflicts.
        let items = parse_ok("[a]\nx = 1\n[a]\ny = 2\n");
        assert_eq!(items[0].section.as_deref(), Some("a"));
        assert_eq!(items[1].section.as_deref(), Some("a"));
    }

    #[test]
    fn missing_equals_errors() {
        let err = parse("[s]\nkey 1\n").unwrap_err();
        match err {
            ConfigError::Parse { line, .. } => assert_eq!(line, 2),
            _ => panic!("expected Parse"),
        }
    }

    #[test]
    fn missing_rbracket_errors() {
        assert!(matches!(parse("[s\n").unwrap_err(), ConfigError::Parse { .. }));
    }

    #[test]
    fn second_statement_on_same_line_errors() {
        let err = parse("a = 1 b = 2\n").unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn bare_section_header_is_ok() {
        let items = parse_ok("[empty]\n");
        assert!(items.is_empty());
    }
}
