//! Shared diagnostic type.
//!
//! The parser and the resolver both report span-tagged errors; rather than two
//! near-identical structs, both produce [`Diag`].  A `Diag` carries a byte
//! [`Span`] so the CLI (`offset_to_line_col` in `main.rs`) can render it with
//! source-location context.

use crate::ast::Span;

/// A single diagnostic message anchored to a source span.
#[derive(Debug, Clone)]
pub struct Diag {
    pub span: Span,
    pub msg: String,
}

impl Diag {
    pub fn new(span: Span, msg: impl Into<String>) -> Self {
        Diag { span, msg: msg.into() }
    }
}

impl std::fmt::Display for Diag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "error at {}..{}: {}", self.span.start, self.span.end, self.msg)
    }
}
