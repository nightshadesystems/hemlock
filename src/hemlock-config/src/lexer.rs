//! Tokenizer for the curly-brace config format.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    /// Bare word or quoted string (quotes already stripped/unescaped).
    Word(String),
    LBrace,
    RBrace,
    /// Statement terminator: newlines end leaves; `;` is accepted too so
    /// configs written in the older semicolon style still parse.
    Semi,
    Newline,
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Token::Word(w) => write!(f, "{w:?}"),
            Token::LBrace => write!(f, "'{{'"),
            Token::RBrace => write!(f, "'}}'"),
            Token::Semi => write!(f, "';'"),
            Token::Newline => write!(f, "end of line"),
        }
    }
}

/// A token plus where it started, for error messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spanned {
    pub token: Token,
    pub line: u32,
    pub col: u32,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LexError {
    #[error("line {line}:{col}: unterminated string")]
    UnterminatedString { line: u32, col: u32 },

    #[error("line {line}:{col}: unterminated block comment")]
    UnterminatedComment { line: u32, col: u32 },

    #[error("line {line}:{col}: unexpected character {ch:?}")]
    UnexpectedChar { line: u32, col: u32, ch: char },
}

/// True for characters that may appear in a bare (unquoted) word. Covers
/// interface names, IPs/prefixes, paths, and dashed keywords.
pub fn is_word_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ':' | '/' | '*' | '@' | '+')
}

pub fn lex(input: &str) -> Result<Vec<Spanned>, LexError> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();
    let (mut line, mut col) = (1u32, 1u32);

    macro_rules! bump {
        () => {{
            let ch = chars.next();
            if ch == Some('\n') {
                line += 1;
                col = 1;
            } else if ch.is_some() {
                col += 1;
            }
            ch
        }};
    }

    while let Some(&ch) = chars.peek() {
        let (start_line, start_col) = (line, col);
        match ch {
            ' ' | '\t' | '\r' => {
                bump!();
            }
            '\n' => {
                bump!();
                tokens.push(Spanned {
                    token: Token::Newline,
                    line: start_line,
                    col: start_col,
                });
            }
            '{' => {
                bump!();
                tokens.push(Spanned {
                    token: Token::LBrace,
                    line: start_line,
                    col: start_col,
                });
            }
            '}' => {
                bump!();
                tokens.push(Spanned {
                    token: Token::RBrace,
                    line: start_line,
                    col: start_col,
                });
            }
            ';' => {
                bump!();
                tokens.push(Spanned {
                    token: Token::Semi,
                    line: start_line,
                    col: start_col,
                });
            }
            '#' => {
                while let Some(&c) = chars.peek() {
                    if c == '\n' {
                        break;
                    }
                    bump!();
                }
            }
            '/' => {
                bump!();
                match chars.peek() {
                    Some('/') => {
                        while let Some(&c) = chars.peek() {
                            if c == '\n' {
                                break;
                            }
                            bump!();
                        }
                    }
                    Some('*') => {
                        bump!();
                        let mut closed = false;
                        while let Some(c) = bump!() {
                            if c == '*' && chars.peek() == Some(&'/') {
                                bump!();
                                closed = true;
                                break;
                            }
                        }
                        if !closed {
                            return Err(LexError::UnterminatedComment {
                                line: start_line,
                                col: start_col,
                            });
                        }
                    }
                    _ => {
                        // A bare '/' starts a word (e.g. a path).
                        let mut word = String::from('/');
                        while let Some(&c) = chars.peek() {
                            if is_word_char(c) {
                                word.push(c);
                                bump!();
                            } else {
                                break;
                            }
                        }
                        tokens.push(Spanned {
                            token: Token::Word(word),
                            line: start_line,
                            col: start_col,
                        });
                    }
                }
            }
            '"' => {
                bump!();
                let mut value = String::new();
                loop {
                    match bump!() {
                        Some('"') => break,
                        Some('\\') => match bump!() {
                            Some('"') => value.push('"'),
                            Some('\\') => value.push('\\'),
                            Some('n') => value.push('\n'),
                            Some(other) => {
                                value.push('\\');
                                value.push(other);
                            }
                            None => {
                                return Err(LexError::UnterminatedString {
                                    line: start_line,
                                    col: start_col,
                                })
                            }
                        },
                        Some(c) => value.push(c),
                        None => {
                            return Err(LexError::UnterminatedString {
                                line: start_line,
                                col: start_col,
                            })
                        }
                    }
                }
                tokens.push(Spanned {
                    token: Token::Word(value),
                    line: start_line,
                    col: start_col,
                });
            }
            c if is_word_char(c) => {
                let mut word = String::new();
                while let Some(&c) = chars.peek() {
                    if is_word_char(c) {
                        word.push(c);
                        bump!();
                    } else {
                        break;
                    }
                }
                tokens.push(Spanned {
                    token: Token::Word(word),
                    line: start_line,
                    col: start_col,
                });
            }
            other => {
                return Err(LexError::UnexpectedChar {
                    line,
                    col,
                    ch: other,
                });
            }
        }
    }
    Ok(tokens)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn words(input: &str) -> Vec<Token> {
        lex(input).unwrap().into_iter().map(|s| s.token).collect()
    }

    #[test]
    fn lexes_basic_statement() {
        assert_eq!(
            words("hostname sw1;"),
            vec![
                Token::Word("hostname".into()),
                Token::Word("sw1".into()),
                Token::Semi
            ]
        );
    }

    #[test]
    fn strings_and_escapes() {
        assert_eq!(
            words(r#"description "uplink \"A\" to core";"#),
            vec![
                Token::Word("description".into()),
                Token::Word("uplink \"A\" to core".into()),
                Token::Semi
            ]
        );
    }

    #[test]
    fn comments_are_skipped() {
        let input = "# line\nfoo; // trailing\n/* block\nstill */ bar;";
        assert_eq!(
            words(input),
            vec![
                Token::Newline,
                Token::Word("foo".into()),
                Token::Semi,
                Token::Newline,
                Token::Word("bar".into()),
                Token::Semi
            ]
        );
    }

    #[test]
    fn newlines_are_tokens() {
        assert_eq!(
            words("a\nb"),
            vec![
                Token::Word("a".into()),
                Token::Newline,
                Token::Word("b".into()),
            ]
        );
    }

    #[test]
    fn tracks_positions() {
        let tokens = lex("a {\n  b;\n}").unwrap();
        // a { NL b ; NL }
        let b = &tokens[3];
        assert_eq!((b.line, b.col), (2, 3));
    }

    #[test]
    fn rejects_unterminated_string() {
        assert!(matches!(
            lex("desc \"oops"),
            Err(LexError::UnterminatedString { .. })
        ));
    }
}
