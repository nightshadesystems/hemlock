//! Recursive-descent parser: tokens -> [`ConfigTree`].
//!
//! Grammar:
//! ```text
//! config    := statement*
//! statement := WORD WORD* ( EOL | ';' | '{' statement* '}' )
//! ```
//! The first word is the node name; the remaining words are leaf values
//! (before end-of-line) or block keys (before `{`). Leaves end at the end
//! of the line; `;` is also accepted as a terminator so configs written in
//! the older semicolon style keep parsing. A closing `}` or end of input
//! also finishes a pending leaf, allowing `flags { fast }` on one line.

use crate::lexer::{lex, LexError, Spanned, Token};
use crate::tree::{ConfigTree, Item};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseError {
    #[error(transparent)]
    Lex(#[from] LexError),

    #[error("line {line}:{col}: expected {expected}, found {found}")]
    Unexpected {
        line: u32,
        col: u32,
        expected: &'static str,
        found: String,
    },

    #[error("unexpected end of input (unclosed block?)")]
    UnexpectedEof,
}

pub fn parse(input: &str) -> Result<ConfigTree, ParseError> {
    let tokens = lex(input)?;
    let mut parser = Parser { tokens, pos: 0 };
    let items = parser.statements(/* top_level = */ true)?;
    Ok(ConfigTree { items })
}

struct Parser {
    tokens: Vec<Spanned>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Spanned> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<Spanned> {
        let token = self.tokens.get(self.pos).cloned();
        if token.is_some() {
            self.pos += 1;
        }
        token
    }

    fn unexpected(spanned: &Spanned, expected: &'static str) -> ParseError {
        ParseError::Unexpected {
            line: spanned.line,
            col: spanned.col,
            expected,
            found: spanned.token.to_string(),
        }
    }

    /// Parse statements until EOF (top level) or a closing brace. Blank
    /// lines between statements are skipped.
    fn statements(&mut self, top_level: bool) -> Result<Vec<Item>, ParseError> {
        let mut items = Vec::new();
        loop {
            match self.peek() {
                None if top_level => return Ok(items),
                None => return Err(ParseError::UnexpectedEof),
                Some(spanned) if spanned.token == Token::Newline => {
                    self.pos += 1;
                }
                Some(spanned) if spanned.token == Token::RBrace => {
                    if top_level {
                        return Err(Self::unexpected(spanned, "a statement"));
                    }
                    self.pos += 1; // consume '}'
                    return Ok(items);
                }
                Some(_) => items.push(self.statement()?),
            }
        }
    }

    fn statement(&mut self) -> Result<Item, ParseError> {
        let name = match self.next() {
            Some(Spanned {
                token: Token::Word(word),
                ..
            }) => word,
            Some(other) => return Err(Self::unexpected(&other, "a name")),
            None => return Err(ParseError::UnexpectedEof),
        };

        let mut words = Vec::new();
        loop {
            match self.peek() {
                Some(Spanned {
                    token: Token::Word(_),
                    ..
                }) => {
                    let Some(Spanned {
                        token: Token::Word(word),
                        ..
                    }) = self.next()
                    else {
                        unreachable!("peek said Word");
                    };
                    words.push(word);
                }
                // Leaf terminators: end of line, `;`, or — without
                // consuming — the enclosing block's `}` / end of input.
                Some(Spanned {
                    token: Token::Semi | Token::Newline,
                    ..
                }) => {
                    self.pos += 1;
                    return Ok(Item::Leaf {
                        name,
                        values: words,
                    });
                }
                Some(Spanned {
                    token: Token::RBrace,
                    ..
                })
                | None => {
                    return Ok(Item::Leaf {
                        name,
                        values: words,
                    });
                }
                Some(Spanned {
                    token: Token::LBrace,
                    ..
                }) => {
                    self.pos += 1;
                    let children = self.statements(false)?;
                    return Ok(Item::Block {
                        name,
                        keys: words,
                        children,
                    });
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_config() {
        let tree = parse(
            r#"
system {
    hostname sw1;
}
interfaces {
    ethernet Ethernet0 {
        description "uplink to core";
        admin-state disabled;
    }
    ethernet Ethernet1 {
        admin-state enabled;
    }
}
"#,
        )
        .unwrap();

        let (_, interfaces) = tree.block("interfaces").unwrap();
        let eths: Vec<_> = ConfigTree::blocks_named(interfaces, "ethernet").collect();
        assert_eq!(eths.len(), 2);
        assert_eq!(eths[0].0, ["Ethernet0"]);
        assert_eq!(
            ConfigTree::leaf_value(eths[0].1, "admin-state"),
            Some("disabled")
        );
    }

    #[test]
    fn empty_config_is_empty_tree() {
        assert_eq!(parse("").unwrap(), ConfigTree::default());
        assert_eq!(
            parse("  # only a comment\n").unwrap(),
            ConfigTree::default()
        );
    }

    #[test]
    fn round_trips_canonical_text() {
        let source = "interfaces {\n    ethernet Ethernet0 {\n        description \"has spaces\"\n    }\n}\n";
        let tree = parse(source).unwrap();
        assert_eq!(tree.to_text(), source);
        assert_eq!(parse(&tree.to_text()).unwrap(), tree);
    }

    #[test]
    fn newline_terminated_statements_parse() {
        // The current style: no semicolons anywhere.
        let tree = parse("system {\n    hostname sw1\n}\ninterfaces {\n    ethernet Ethernet0 {\n        admin-state enabled\n    }\n}\n").unwrap();
        let (_, system) = tree.block("system").unwrap();
        assert_eq!(ConfigTree::leaf_value(system, "hostname"), Some("sw1"));
        let (_, interfaces) = tree.block("interfaces").unwrap();
        let (_, eth) = ConfigTree::blocks_named(interfaces, "ethernet")
            .next()
            .unwrap();
        assert_eq!(ConfigTree::leaf_value(eth, "admin-state"), Some("enabled"));

        // Mixed styles parse identically: `;`, newline, and }-terminated.
        assert_eq!(
            parse("a { b 1; c 2\n d 3 }").unwrap(),
            parse("a {\n b 1\n c 2\n d 3\n}").unwrap()
        );
    }

    #[test]
    fn flags_and_multi_values() {
        let tree = parse("flags { fast; mtu 9216 bytes; }").unwrap();
        let (_, flags) = tree.block("flags").unwrap();
        assert!(matches!(
            &flags[0],
            Item::Leaf { name, values } if name == "fast" && values.is_empty()
        ));
        assert!(matches!(
            &flags[1],
            Item::Leaf { name, values } if name == "mtu" && values == &["9216", "bytes"]
        ));
    }

    #[test]
    fn error_reports_position() {
        // A statement must start with a name; a bare '{' is pointed at.
        let err = parse("system {\n    { oops }\n}\n").unwrap_err();
        match err {
            ParseError::Unexpected { line, .. } => assert_eq!(line, 2),
            other => panic!("wrong error: {other}"),
        }
    }

    #[test]
    fn unclosed_block_is_eof_error() {
        assert_eq!(parse("a { b;").unwrap_err(), ParseError::UnexpectedEof);
    }

    #[test]
    fn stray_close_brace_is_an_error() {
        assert!(matches!(parse("}"), Err(ParseError::Unexpected { .. })));
    }
}
