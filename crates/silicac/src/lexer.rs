//! Silica lexer — hand-written, no external dependencies.
//!
//! Produces a flat `Vec<Spanned<Token>>` from a `&str`.  The span records
//! byte offsets into the original source so later passes can report errors
//! with source-location context.

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    // ── Keywords ──────────────────────────────────────────────────────────
    KwProgram,
    KwDevice,
    KwInterface,
    KwBoard,
    KwOps,
    KwOp,
    KwRegs,
    KwConfig,
    KwNeeds,
    KwStates,
    KwOn,
    KwEvery,
    KwWithin,
    KwCell,
    KwLet,
    KwBe,     // `become`
    KwBecome,
    KwWhen,
    KwReturn,
    KwNot,
    KwAnd,
    KwOr,
    KwFault,
    KwYields,
    KwEmits,
    KwAs,
    KwFor,
    KwSet,
    KwExtend,
    KwRemove,
    KwOverlay,
    KwUse,
    KwImpl,     // `implements`
    KwWhere,
    KwAt,
    KwIn,
    KwComptime,
    KwSafe,
    KwPoll,
    KwAwait,
    KwElse,
    KwMatch,
    KwEnum,
    KwTrue,
    KwFalse,
    KwExit,     // host intrinsic keyword

    // ── Identifiers & literals ────────────────────────────────────────────
    Ident(String),
    /// Integer literal, no suffix.
    IntLit(u64),
    /// Duration literal: value + unit (e.g. `500ms` → (500, "ms")).
    DurationLit(u64, DurationUnit),
    /// Size literal: `4K`, `512K`, `64M` (in bytes).
    SizeLit(u64),
    /// Frequency literal: `8MHz`, `16kHz`.
    FreqLit(u64), // Hz
    /// String literal (contents, already unescaped).
    StringLit(String),

    // ── Punctuation ───────────────────────────────────────────────────────
    LBrace,   // {
    RBrace,   // }
    LParen,   // (
    RParen,   // )
    LBracket, // [
    RBracket, // ]
    Dot,      // .
    Comma,    // ,
    Colon,    // :
    Semi,     // ;
    Eq,       // =
    Arrow,    // ->
    FatArrow, // =>
    Plus,     // +
    Minus,    // -
    Star,     // *
    Slash,    // /
    Percent,  // %
    Bang,     // !
    Lt,       // <
    Gt,       // >
    Le,       // <=
    Ge,       // >=
    EqEq,     // ==
    BangEq,   // !=
    Amp,      // &
    Pipe,     // |
    Caret,    // ^
    DotDot,   // ..
    DotDotEq, // ..=
    Question, // ?
    Hash,     // #
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurationUnit {
    Ns,
    Us,
    Ms,
    S,
}

impl DurationUnit {
    pub fn to_ns(self, value: u64) -> u64 {
        match self {
            DurationUnit::Ns => value,
            DurationUnit::Us => value * 1_000,
            DurationUnit::Ms => value * 1_000_000,
            DurationUnit::S => value * 1_000_000_000,
        }
    }
}

/// A token together with its byte-range in the source.
#[derive(Debug, Clone)]
pub struct Spanned<T> {
    pub inner: T,
    pub start: usize,
    pub end: usize,
}

impl<T> Spanned<T> {
    fn new(inner: T, start: usize, end: usize) -> Self {
        Spanned { inner, start, end }
    }
}

/// All errors that the lexer can produce.
#[derive(Debug)]
pub struct LexError {
    pub offset: usize,
    pub msg: String,
}

impl std::fmt::Display for LexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "lex error at offset {}: {}", self.offset, self.msg)
    }
}

pub fn lex(src: &str) -> Result<Vec<Spanned<Token>>, LexError> {
    let mut tokens = Vec::new();
    let bytes = src.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;

    while i < len {
        // Skip whitespace.
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }

        // Line comments `//`.
        if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }

        // Block comments `/* ... */`.
        if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            let start = i;
            i += 2;
            loop {
                if i + 1 >= len {
                    return Err(LexError {
                        offset: start,
                        msg: "unterminated block comment".into(),
                    });
                }
                if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    i += 2;
                    break;
                }
                i += 1;
            }
            continue;
        }

        let start = i;

        // String literals.
        if bytes[i] == b'"' {
            i += 1;
            let mut s = String::new();
            loop {
                if i >= len {
                    return Err(LexError {
                        offset: start,
                        msg: "unterminated string literal".into(),
                    });
                }
                match bytes[i] {
                    b'"' => {
                        i += 1;
                        break;
                    }
                    b'\\' => {
                        i += 1;
                        if i >= len {
                            return Err(LexError {
                                offset: start,
                                msg: "unterminated escape sequence".into(),
                            });
                        }
                        match bytes[i] {
                            b'n' => s.push('\n'),
                            b't' => s.push('\t'),
                            b'r' => s.push('\r'),
                            b'\\' => s.push('\\'),
                            b'"' => s.push('"'),
                            b'0' => s.push('\0'),
                            other => {
                                return Err(LexError {
                                    offset: i,
                                    msg: format!("unknown escape \\{}", other as char),
                                });
                            }
                        }
                        i += 1;
                    }
                    ch => {
                        s.push(ch as char);
                        i += 1;
                    }
                }
            }
            tokens.push(Spanned::new(Token::StringLit(s), start, i));
            continue;
        }

        // Numbers (integer / duration / size / frequency).
        if bytes[i].is_ascii_digit() {
            let mut num_str = String::new();
            while i < len && (bytes[i].is_ascii_digit() || bytes[i] == b'_') {
                if bytes[i] != b'_' {
                    num_str.push(bytes[i] as char);
                }
                i += 1;
            }
            let value: u64 = num_str.parse().map_err(|_| LexError {
                offset: start,
                msg: format!("integer overflow in literal '{}'", num_str),
            })?;

            // Check for suffix.
            if i < len && bytes[i].is_ascii_alphabetic() {
                let suf_start = i;
                let mut suf = String::new();
                while i < len && bytes[i].is_ascii_alphabetic() {
                    suf.push(bytes[i] as char);
                    i += 1;
                }
                let tok = match suf.as_str() {
                    "ns" => Token::DurationLit(value, DurationUnit::Ns),
                    "us" => Token::DurationLit(value, DurationUnit::Us),
                    "ms" => Token::DurationLit(value, DurationUnit::Ms),
                    "s" => Token::DurationLit(value, DurationUnit::S),
                    "K" => Token::SizeLit(value * 1024),
                    "M" => Token::SizeLit(value * 1024 * 1024),
                    "G" => Token::SizeLit(value * 1024 * 1024 * 1024),
                    "Hz" => Token::FreqLit(value),
                    "kHz" => Token::FreqLit(value * 1_000),
                    "MHz" => Token::FreqLit(value * 1_000_000),
                    "GHz" => Token::FreqLit(value * 1_000_000_000),
                    _ => {
                        return Err(LexError {
                            offset: suf_start,
                            msg: format!("unknown literal suffix '{}'", suf),
                        })
                    }
                };
                tokens.push(Spanned::new(tok, start, i));
                continue;
            }

            tokens.push(Spanned::new(Token::IntLit(value), start, i));
            continue;
        }

        // Identifiers and keywords.
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let mut word = String::new();
            while i < len && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                word.push(bytes[i] as char);
                i += 1;
            }
            let tok = keyword_or_ident(word);
            tokens.push(Spanned::new(tok, start, i));
            continue;
        }

        // Two-character punctuation.
        if i + 1 < len {
            let pair = (bytes[i], bytes[i + 1]);
            let maybe = match pair {
                (b'-', b'>') => Some(Token::Arrow),
                (b'=', b'>') => Some(Token::FatArrow),
                (b'<', b'=') => Some(Token::Le),
                (b'>', b'=') => Some(Token::Ge),
                (b'=', b'=') => Some(Token::EqEq),
                (b'!', b'=') => Some(Token::BangEq),
                (b'.', b'.') => {
                    // could be ..=
                    if i + 2 < len && bytes[i + 2] == b'=' {
                        tokens.push(Spanned::new(Token::DotDotEq, start, start + 3));
                        i += 3;
                        continue;
                    }
                    Some(Token::DotDot)
                }
                _ => None,
            };
            if let Some(tok) = maybe {
                tokens.push(Spanned::new(tok, start, start + 2));
                i += 2;
                continue;
            }
        }

        // Single-character punctuation.
        let single = match bytes[i] {
            b'{' => Some(Token::LBrace),
            b'}' => Some(Token::RBrace),
            b'(' => Some(Token::LParen),
            b')' => Some(Token::RParen),
            b'[' => Some(Token::LBracket),
            b']' => Some(Token::RBracket),
            b'.' => Some(Token::Dot),
            b',' => Some(Token::Comma),
            b':' => Some(Token::Colon),
            b';' => Some(Token::Semi),
            b'=' => Some(Token::Eq),
            b'+' => Some(Token::Plus),
            b'-' => Some(Token::Minus),
            b'*' => Some(Token::Star),
            b'/' => Some(Token::Slash),
            b'%' => Some(Token::Percent),
            b'!' => Some(Token::Bang),
            b'<' => Some(Token::Lt),
            b'>' => Some(Token::Gt),
            b'&' => Some(Token::Amp),
            b'|' => Some(Token::Pipe),
            b'^' => Some(Token::Caret),
            b'?' => Some(Token::Question),
            b'#' => Some(Token::Hash),
            _ => None,
        };
        if let Some(tok) = single {
            tokens.push(Spanned::new(tok, start, start + 1));
            i += 1;
            continue;
        }

        return Err(LexError {
            offset: i,
            msg: format!("unexpected character '{}'", bytes[i] as char),
        });
    }

    Ok(tokens)
}

fn keyword_or_ident(word: String) -> Token {
    match word.as_str() {
        "program" => Token::KwProgram,
        "device" => Token::KwDevice,
        "interface" => Token::KwInterface,
        "board" => Token::KwBoard,
        "ops" => Token::KwOps,
        "op" => Token::KwOp,
        "regs" => Token::KwRegs,
        "config" => Token::KwConfig,
        "needs" => Token::KwNeeds,
        "states" => Token::KwStates,
        "on" => Token::KwOn,
        "every" => Token::KwEvery,
        "within" => Token::KwWithin,
        "cell" => Token::KwCell,
        "let" => Token::KwLet,
        "become" => Token::KwBecome,
        "when" => Token::KwWhen,
        "return" => Token::KwReturn,
        "not" => Token::KwNot,
        "and" => Token::KwAnd,
        "or" => Token::KwOr,
        "fault" => Token::KwFault,
        "yields" => Token::KwYields,
        "emits" => Token::KwEmits,
        "as" => Token::KwAs,
        "for" => Token::KwFor,
        "set" => Token::KwSet,
        "extend" => Token::KwExtend,
        "remove" => Token::KwRemove,
        "overlay" => Token::KwOverlay,
        "use" => Token::KwUse,
        "implements" => Token::KwImpl,
        "where" => Token::KwWhere,
        "at" => Token::KwAt,
        "in" => Token::KwIn,
        "comptime" => Token::KwComptime,
        "safe" => Token::KwSafe,
        "poll" => Token::KwPoll,
        "await" => Token::KwAwait,
        "else" => Token::KwElse,
        "match" => Token::KwMatch,
        "enum" => Token::KwEnum,
        "true" => Token::KwTrue,
        "false" => Token::KwFalse,
        "exit" => Token::KwExit,
        _ => Token::Ident(word),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_tokens() {
        let src = "program hello { on sys.start { } }";
        let toks: Vec<_> = lex(src).unwrap().into_iter().map(|s| s.inner).collect();
        assert_eq!(
            toks,
            vec![
                Token::KwProgram,
                Token::Ident("hello".into()),
                Token::LBrace,
                Token::KwOn,
                Token::Ident("sys".into()),
                Token::Dot,
                Token::Ident("start".into()),
                Token::LBrace,
                Token::RBrace,
                Token::RBrace,
            ]
        );
    }

    #[test]
    fn duration_literals() {
        let toks: Vec<_> = lex("500ms 1s 10us 200ns")
            .unwrap()
            .into_iter()
            .map(|s| s.inner)
            .collect();
        assert_eq!(
            toks,
            vec![
                Token::DurationLit(500, DurationUnit::Ms),
                Token::DurationLit(1, DurationUnit::S),
                Token::DurationLit(10, DurationUnit::Us),
                Token::DurationLit(200, DurationUnit::Ns),
            ]
        );
    }

    #[test]
    fn string_escape() {
        let toks: Vec<_> = lex(r#""Hello\n""#)
            .unwrap()
            .into_iter()
            .map(|s| s.inner)
            .collect();
        assert_eq!(toks, vec![Token::StringLit("Hello\n".into())]);
    }

    #[test]
    fn line_comment_skipped() {
        let toks: Vec<_> = lex("// this is a comment\nprogram")
            .unwrap()
            .into_iter()
            .map(|s| s.inner)
            .collect();
        assert_eq!(toks, vec![Token::KwProgram]);
    }
}
