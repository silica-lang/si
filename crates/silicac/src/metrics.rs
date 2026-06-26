//! Corpus metrics (audit #35 P2-2) — the measurable proxy for the agentic-native
//! thesis (risk #4: *"measure how often escape hatches appear in the std lib"*).
//!
//! The std lib is the agent's idiom corpus (§7.4); if it is full of escape
//! hatches the defaults are wrong.  This counts the language's strictness escape
//! hatches in a source file:
//!
//!   - explicit numeric casts `<expr> as <type>` (§4.3 — the one way to narrow /
//!     change signedness),
//!   - the wrapping/saturating operators `+% +| -% -| *% *|`,
//!   - `.raw` register access and `.le`/`.be` endianness — spec'd but not yet
//!     lexable, so always 0 today (counted for forward-compatibility).
//!
//! It works at the **token** level: the lexer strips comments, so prose hits
//! don't count, and `as <type>` is distinguished from the pin form (`as output`)
//! and `use … as <alias>` by checking the token after `as` is a *type name*.

use crate::lexer::{self, Token};

/// Escape-hatch counts for one source file (or summed across many).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EscapeHatches {
    /// `<expr> as <type>` numeric casts.
    pub casts: u32,
    /// `+% +| -% -| *% *|` wrapping/saturating operators.
    pub wrap_sat: u32,
    /// `.raw` register escape hatch (not lexable yet → 0).
    pub raw: u32,
    /// `.le` / `.be` endianness (not lexable yet → 0).
    pub endian: u32,
}

impl EscapeHatches {
    pub fn total(&self) -> u32 {
        self.casts + self.wrap_sat + self.raw + self.endian
    }
    pub fn add(&mut self, o: EscapeHatches) {
        self.casts += o.casts;
        self.wrap_sat += o.wrap_sat;
        self.raw += o.raw;
        self.endian += o.endian;
    }
}

/// Is `name` a numeric/scalar type name — i.e. does `as <name>` denote a cast
/// (vs. a pin binding `as output`/`as input` or a `use … as <alias>`)?
fn is_type_name(name: &str) -> bool {
    matches!(
        name,
        "u8" | "u16" | "u32" | "u64"
            | "s8" | "s16" | "s32" | "s64"
            | "i8" | "i16" | "i32" | "i64"
            | "bool" | "bytes" | "fixed"
            | "instant" | "duration"
            | "f32" | "f64" | "float" | "double"
    )
}

/// Count the escape hatches in one Silica source file.
pub fn count_escape_hatches(src: &str) -> Result<EscapeHatches, lexer::LexError> {
    let toks = lexer::lex(src)?;
    let mut h = EscapeHatches::default();
    for (i, t) in toks.iter().enumerate() {
        match &t.inner {
            Token::PlusPercent | Token::PlusPipe | Token::MinusPercent | Token::MinusPipe
            | Token::StarPercent | Token::StarPipe => h.wrap_sat += 1,
            Token::KwAs => {
                // A cast is `as <type-name>`; `as output`/`as input` (pins) and
                // `use … as <alias>` name non-types, so they don't count.
                if let Some(Token::Ident(name)) = toks.get(i + 1).map(|s| &s.inner) {
                    if is_type_name(name) {
                        h.casts += 1;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_casts_and_wrap_sat_not_pins_or_comments() {
        let src = r#"
            // this comment mentions `as u32` and `+%` but must not count
            program p {
              use board b as dev      // alias, not a cast
              let x = 5 as u32        // a real cast
              let y = x +% 1          // wrapping op
              let z = y as s16        // another cast
            }
        "#;
        let h = count_escape_hatches(src).expect("lex");
        assert_eq!(h.casts, 2, "two `as <type>` casts (alias + comment excluded)");
        assert_eq!(h.wrap_sat, 1);
        assert_eq!(h.raw, 0);
        assert_eq!(h.total(), 3);
    }

    #[test]
    fn pin_binding_as_is_not_a_cast() {
        let src = "board b { gpio0 : nrf_gpio at 0x5000_0000  led : nrf_gpio.pin = gpio0.pin(13) as output }";
        assert_eq!(count_escape_hatches(src).expect("lex").casts, 0);
    }
}
