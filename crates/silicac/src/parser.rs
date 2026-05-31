//! Recursive-descent parser for Silica — Phase 0 subset.
//!
//! Entry point: `parse(tokens) -> Result<Module, ParseError>`.
//!
//! The parser is intentionally table-free: each grammar rule maps to one
//! function.  The lookahead is always at most 2 tokens.

use crate::ast::*;
use crate::lexer::{Spanned, Token};

// ─── Error ───────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct ParseError {
    pub span: Span,
    pub msg: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "parse error at {}..{}: {}", self.span.start, self.span.end, self.msg)
    }
}

// ─── Parser state ─────────────────────────────────────────────────────────────

struct Parser {
    tokens: Vec<Spanned<Token>>,
    pos: usize,
}

impl Parser {
    fn new(tokens: Vec<Spanned<Token>>) -> Self {
        Parser { tokens, pos: 0 }
    }

    // ── Primitives ──────────────────────────────────────────────────────────

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos).map(|s| &s.inner)
    }

    fn peek2(&self) -> Option<&Token> {
        self.tokens.get(self.pos + 1).map(|s| &s.inner)
    }

    fn current_span(&self) -> Span {
        self.tokens
            .get(self.pos)
            .map(|s| Span::new(s.start, s.end))
            .unwrap_or_default()
    }

    fn prev_span(&self) -> Span {
        if self.pos == 0 {
            return Span::default();
        }
        let s = &self.tokens[self.pos - 1];
        Span::new(s.start, s.end)
    }

    fn advance(&mut self) -> Option<&Spanned<Token>> {
        let t = self.tokens.get(self.pos);
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, expected: &Token) -> Result<Span, ParseError> {
        match self.peek() {
            Some(t) if t == expected => {
                let span = self.current_span();
                self.advance();
                Ok(span)
            }
            other => Err(ParseError {
                span: self.current_span(),
                msg: format!("expected {:?}, got {:?}", expected, other),
            }),
        }
    }

    fn eat_ident(&mut self) -> Result<Ident, ParseError> {
        match self.peek().cloned() {
            Some(Token::Ident(name)) => {
                let span = self.current_span();
                self.advance();
                Ok(Ident::new(name, span))
            }
            other => Err(ParseError {
                span: self.current_span(),
                msg: format!("expected identifier, got {:?}", other),
            }),
        }
    }

    fn at_end(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    fn error(&self, msg: impl Into<String>) -> ParseError {
        ParseError { span: self.current_span(), msg: msg.into() }
    }

    // ── Top level ───────────────────────────────────────────────────────────

    fn parse_module(&mut self) -> Result<Module, ParseError> {
        let mut items = Vec::new();
        while !self.at_end() {
            let item = self.parse_item()?;
            items.push(item);
        }
        Ok(Module { items })
    }

    fn parse_item(&mut self) -> Result<Item, ParseError> {
        match self.peek() {
            Some(Token::KwProgram) => Ok(Item::Program(self.parse_program()?)),
            Some(Token::KwDevice) => Ok(Item::Device(self.parse_device()?)),
            other => Err(ParseError {
                span: self.current_span(),
                msg: format!("expected top-level item (program/device), got {:?}", other),
            }),
        }
    }

    // ── Program ─────────────────────────────────────────────────────────────

    fn parse_program(&mut self) -> Result<ProgramDef, ParseError> {
        let start = self.current_span().start;
        self.eat(&Token::KwProgram)?;
        let name = self.eat_ident()?;
        self.eat(&Token::LBrace)?;

        let mut items = Vec::new();
        while self.peek() != Some(&Token::RBrace) {
            if self.at_end() {
                return Err(self.error("unexpected EOF inside program block"));
            }
            let item = self.parse_program_item()?;
            items.push(item);
        }
        let end = self.current_span().end;
        self.eat(&Token::RBrace)?;

        Ok(ProgramDef { name, span: Span::new(start, end), items })
    }

    fn parse_program_item(&mut self) -> Result<ProgramItem, ParseError> {
        match self.peek() {
            Some(Token::KwUse) => Ok(ProgramItem::UseDecl(self.parse_use()?)),
            Some(Token::KwLet) => Ok(ProgramItem::LetDecl(self.parse_let()?)),
            Some(Token::KwCell) => Ok(ProgramItem::CellDecl(self.parse_cell()?)),
            Some(Token::KwOn) => Ok(ProgramItem::Reaction(self.parse_reaction()?)),
            Some(Token::KwEvery) => Ok(ProgramItem::Reaction(self.parse_reaction()?)),
            other => Err(ParseError {
                span: self.current_span(),
                msg: format!("expected program item (use/let/cell/on/every), got {:?}", other),
            }),
        }
    }

    fn parse_use(&mut self) -> Result<UseDecl, ParseError> {
        let start = self.current_span().start;
        self.eat(&Token::KwUse)?;

        // `use <path> as <alias>`  — path is dot-separated identifiers
        let mut path = vec![self.eat_ident()?];
        while self.peek() == Some(&Token::Dot) {
            self.advance();
            path.push(self.eat_ident()?);
        }
        self.eat(&Token::KwAs)?;
        let alias = self.eat_ident()?;
        self.eat_optional_semi();
        let end = self.prev_span().end;
        Ok(UseDecl { path, alias, span: Span::new(start, end) })
    }

    fn parse_let(&mut self) -> Result<LetDecl, ParseError> {
        let start = self.current_span().start;
        self.eat(&Token::KwLet)?;
        let name = self.eat_ident()?;
        let ty = if self.peek() == Some(&Token::Colon) {
            self.advance();
            Some(self.parse_type()?)
        } else {
            None
        };
        self.eat(&Token::Eq)?;
        let init = self.parse_expr()?;
        self.eat_optional_semi();
        let end = self.prev_span().end;
        Ok(LetDecl { name, ty, init, span: Span::new(start, end) })
    }

    fn parse_cell(&mut self) -> Result<CellDecl, ParseError> {
        let start = self.current_span().start;
        self.eat(&Token::KwCell)?;
        let name = self.eat_ident()?;
        self.eat(&Token::Colon)?;
        let ty = self.parse_type()?;
        self.eat(&Token::Eq)?;
        let init = self.parse_expr()?;
        self.eat_optional_semi();
        let end = self.prev_span().end;
        Ok(CellDecl { name, ty, init, span: Span::new(start, end) })
    }

    fn parse_reaction(&mut self) -> Result<Reaction, ParseError> {
        let start = self.current_span().start;
        let trigger = match self.peek() {
            Some(Token::KwOn) => {
                self.advance();
                // `on <expr> { ... }`  — expr is a field access ending in the event name
                let event_ref = self.parse_event_ref()?;
                Trigger::On(event_ref)
            }
            Some(Token::KwEvery) => {
                self.advance();
                let dur = self.parse_duration()?;
                Trigger::Every(dur)
            }
            other => {
                return Err(ParseError {
                    span: self.current_span(),
                    msg: format!("expected 'on' or 'every', got {:?}", other),
                })
            }
        };

        // Optional `on fault <disp>` clause.
        let fault_disp = if self.peek() == Some(&Token::KwOn) {
            // peek ahead for `fault`
            if self.peek2() == Some(&Token::KwFault) {
                self.advance(); // eat `on`
                self.advance(); // eat `fault`
                Some(self.parse_fault_disp()?)
            } else {
                None
            }
        } else {
            None
        };

        let body = self.parse_block()?;
        let end = self.prev_span().end;
        Ok(Reaction { trigger, fault_disp, body, span: Span::new(start, end) })
    }

    /// Parse `<ident>.<ident>` (or deeper) as an event reference.
    fn parse_event_ref(&mut self) -> Result<EventRef, ParseError> {
        let start = self.current_span().start;
        // Build device expr (all but the last .field segment).
        let base = self.eat_ident()?;
        let base_span = base.span;
        let mut device = Expr {
            kind: ExprKind::Ident(base),
            span: base_span,
        };

        // Consume all `.ident` pairs; the LAST one is the event name.
        let mut segments: Vec<Ident> = Vec::new();
        while self.peek() == Some(&Token::Dot) {
            self.advance();
            segments.push(self.eat_ident()?);
        }

        if segments.is_empty() {
            return Err(ParseError {
                span: self.current_span(),
                msg: "event reference must be <device>.<event>".into(),
            });
        }

        // All but last become field accesses on device.
        let event = segments.pop().unwrap();
        for seg in segments {
            let seg_span = seg.span;
            device = Expr {
                kind: ExprKind::Field(Box::new(device), seg),
                span: seg_span,
            };
        }

        let end = event.span.end;
        Ok(EventRef { device, event, span: Span::new(start, end) })
    }

    fn parse_fault_disp(&mut self) -> Result<FaultDisp, ParseError> {
        let start = self.current_span().start;
        let kind = match self.peek() {
            Some(Token::Ident(s)) if s == "retry" => {
                self.advance();
                let max = if self.peek() == Some(&Token::LParen) {
                    self.advance();
                    // `max = N`
                    if let Some(Token::Ident(k)) = self.peek().cloned() {
                        if k == "max" {
                            self.advance();
                            self.eat(&Token::Eq)?;
                            if let Some(Token::IntLit(n)) = self.peek().cloned() {
                                self.advance();
                                self.eat(&Token::RParen)?;
                                Some(n as u32)
                            } else {
                                return Err(self.error("expected integer after 'max ='"));
                            }
                        } else {
                            return Err(self.error("expected 'max' in retry clause"));
                        }
                    } else {
                        return Err(self.error("expected 'max' in retry clause"));
                    }
                } else {
                    None
                };
                FaultDispKind::Retry { max }
            }
            Some(Token::Ident(s)) if s == "skip" => {
                self.advance();
                FaultDispKind::Skip
            }
            Some(Token::KwSafe) => {
                self.advance();
                FaultDispKind::Safe
            }
            Some(Token::Ident(s)) if s == "escalate" => {
                self.advance();
                FaultDispKind::Escalate
            }
            other => {
                return Err(ParseError {
                    span: self.current_span(),
                    msg: format!("unknown fault disposition {:?}", other),
                })
            }
        };
        let end = self.prev_span().end;
        Ok(FaultDisp { kind, span: Span::new(start, end) })
    }

    // ── Device ──────────────────────────────────────────────────────────────

    fn parse_device(&mut self) -> Result<DeviceDef, ParseError> {
        let start = self.current_span().start;
        self.eat(&Token::KwDevice)?;
        let name = self.eat_ident()?;

        let mut implements = Vec::new();
        if self.peek() == Some(&Token::KwImpl) {
            self.advance();
            implements.push(self.eat_ident()?);
            while self.peek() == Some(&Token::Comma) {
                self.advance();
                implements.push(self.eat_ident()?);
            }
        }

        self.eat(&Token::LBrace)?;
        let sections = self.parse_device_sections()?;
        let end = self.current_span().end;
        self.eat(&Token::RBrace)?;

        Ok(DeviceDef { name, implements, span: Span::new(start, end), sections })
    }

    fn parse_device_sections(&mut self) -> Result<DeviceSections, ParseError> {
        let mut sections = DeviceSections::default();
        while self.peek() != Some(&Token::RBrace) {
            if self.at_end() {
                return Err(self.error("unexpected EOF in device body"));
            }
            match self.peek() {
                Some(Token::KwOps) => {
                    sections.ops = Some(self.parse_ops_section()?);
                }
                Some(Token::KwStates) => {
                    sections.states = Some(self.parse_states_section()?);
                }
                Some(Token::Ident(s)) if s == "safe_state" => {
                    self.advance();
                    self.eat(&Token::Eq)?;
                    let state = self.eat_ident()?;
                    self.eat_optional_semi();
                    sections.safe_state = Some(state);
                }
                other => {
                    return Err(ParseError {
                        span: self.current_span(),
                        msg: format!("unexpected section keyword in device: {:?}", other),
                    })
                }
            }
        }
        Ok(sections)
    }

    fn parse_ops_section(&mut self) -> Result<OpsSection, ParseError> {
        let start = self.current_span().start;
        self.eat(&Token::KwOps)?;
        self.eat(&Token::LBrace)?;
        let mut items = Vec::new();
        while self.peek() != Some(&Token::RBrace) {
            if self.at_end() {
                return Err(self.error("unexpected EOF in ops section"));
            }
            match self.peek() {
                Some(Token::KwOp) => {
                    items.push(OpsItem::Op(self.parse_op_decl()?));
                }
                other => {
                    return Err(ParseError {
                        span: self.current_span(),
                        msg: format!("expected 'op' in ops section, got {:?}", other),
                    })
                }
            }
        }
        let end = self.current_span().end;
        self.eat(&Token::RBrace)?;
        Ok(OpsSection { items, span: Span::new(start, end) })
    }

    fn parse_op_decl(&mut self) -> Result<OpDecl, ParseError> {
        let start = self.current_span().start;
        self.eat(&Token::KwOp)?;
        let name = self.eat_ident()?;
        self.eat(&Token::LParen)?;
        let params = self.parse_params()?;
        self.eat(&Token::RParen)?;

        // optional `when <state>`
        let when = if self.peek() == Some(&Token::KwWhen) {
            self.advance();
            Some(self.eat_ident()?)
        } else {
            None
        };

        // `-> <type> [or fault]`
        let ret = if self.peek() == Some(&Token::Arrow) {
            self.advance();
            let ty = self.parse_type()?;
            let fallible = if self.peek() == Some(&Token::KwOr) {
                self.advance();
                self.eat(&Token::KwFault)?;
                true
            } else {
                false
            };
            ReturnType { ty, fallible }
        } else {
            ReturnType {
                ty: TypeExpr { kind: TypeKind::Unit, span: Span::default() },
                fallible: false,
            }
        };

        // optional `yields`
        let yields = if self.peek() == Some(&Token::KwYields) {
            self.advance();
            true
        } else {
            false
        };

        // body or empty (for interface ops)
        let body = if self.peek() == Some(&Token::LBrace) {
            self.parse_block()?
        } else {
            self.eat_optional_semi();
            Block { stmts: Vec::new(), span: Span::default() }
        };

        let end = self.prev_span().end;
        Ok(OpDecl { name, params, when, ret, yields, body, span: Span::new(start, end) })
    }

    fn parse_params(&mut self) -> Result<Vec<Param>, ParseError> {
        let mut params = Vec::new();
        while self.peek() != Some(&Token::RParen) {
            if !params.is_empty() {
                self.eat(&Token::Comma)?;
            }
            let name = self.eat_ident()?;
            self.eat(&Token::Colon)?;
            let ty = self.parse_type()?;
            let span = name.span.merge(ty.span);
            params.push(Param { name, ty, span });
            // allow trailing comma
            if self.peek() == Some(&Token::Comma) && self.peek2() == Some(&Token::RParen) {
                self.advance();
            }
        }
        Ok(params)
    }

    fn parse_states_section(&mut self) -> Result<StatesSection, ParseError> {
        let start = self.current_span().start;
        self.eat(&Token::KwStates)?;
        self.eat(&Token::LBrace)?;
        let mut states = Vec::new();
        while self.peek() != Some(&Token::RBrace) {
            states.push(self.eat_ident()?);
            if self.peek() == Some(&Token::Comma) {
                self.advance();
            }
        }
        let end = self.current_span().end;
        self.eat(&Token::RBrace)?;
        Ok(StatesSection { states, span: Span::new(start, end) })
    }

    // ── Block & statements ──────────────────────────────────────────────────

    fn parse_block(&mut self) -> Result<Block, ParseError> {
        let start = self.current_span().start;
        self.eat(&Token::LBrace)?;
        let mut stmts = Vec::new();
        while self.peek() != Some(&Token::RBrace) {
            if self.at_end() {
                return Err(self.error("unexpected EOF in block"));
            }
            stmts.push(self.parse_stmt()?);
        }
        let end = self.current_span().end;
        self.eat(&Token::RBrace)?;
        Ok(Block { stmts, span: Span::new(start, end) })
    }

    fn parse_stmt(&mut self) -> Result<Stmt, ParseError> {
        let start = self.current_span().start;
        match self.peek() {
            Some(Token::KwLet) => Ok(Stmt::Let(self.parse_let()?)),
            Some(Token::KwBecome) => {
                self.advance();
                let state = self.eat_ident()?;
                self.eat_optional_semi();
                Ok(Stmt::Become(state, Span::new(start, self.prev_span().end)))
            }
            Some(Token::KwReturn) => {
                self.advance();
                let expr = if self.peek() == Some(&Token::RBrace) || self.peek() == Some(&Token::Semi) {
                    None
                } else {
                    Some(self.parse_expr()?)
                };
                self.eat_optional_semi();
                Ok(Stmt::Return(expr, Span::new(start, self.prev_span().end)))
            }
            Some(Token::KwExit) => {
                self.advance();
                self.eat(&Token::LParen)?;
                let code = self.parse_expr()?;
                self.eat(&Token::RParen)?;
                self.eat_optional_semi();
                Ok(Stmt::Exit(code, Span::new(start, self.prev_span().end)))
            }
            _ => {
                let expr = self.parse_expr_stmt()?;
                Ok(Stmt::Expr(expr))
            }
        }
    }

    /// Parse an expression that may be an assignment; consume optional trailing `;`.
    fn parse_expr_stmt(&mut self) -> Result<Expr, ParseError> {
        let expr = self.parse_expr()?;
        self.eat_optional_semi();
        Ok(expr)
    }

    // ── Expressions (Pratt-style precedence) ────────────────────────────────

    fn parse_expr(&mut self) -> Result<Expr, ParseError> {
        self.parse_assign()
    }

    fn parse_assign(&mut self) -> Result<Expr, ParseError> {
        let lhs = self.parse_or()?;
        if self.peek() == Some(&Token::Eq) {
            let span_start = lhs.span.start;
            self.advance();
            let rhs = self.parse_assign()?; // right-associative
            let end = rhs.span.end;
            return Ok(Expr {
                kind: ExprKind::Assign(Box::new(lhs), Box::new(rhs)),
                span: Span::new(span_start, end),
            });
        }
        // += -= *= /=
        let op = match self.peek() {
            Some(Token::Plus) if self.peek2() == Some(&Token::Eq) => Some(BinOp::Add),
            Some(Token::Minus) if self.peek2() == Some(&Token::Eq) => Some(BinOp::Sub),
            Some(Token::Star) if self.peek2() == Some(&Token::Eq) => Some(BinOp::Mul),
            Some(Token::Slash) if self.peek2() == Some(&Token::Eq) => Some(BinOp::Div),
            _ => None,
        };
        if let Some(op) = op {
            let span_start = lhs.span.start;
            self.advance(); // op
            self.advance(); // =
            let rhs = self.parse_assign()?;
            let end = rhs.span.end;
            return Ok(Expr {
                kind: ExprKind::CompoundAssign(op, Box::new(lhs), Box::new(rhs)),
                span: Span::new(span_start, end),
            });
        }
        Ok(lhs)
    }

    fn parse_or(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_and()?;
        while self.peek() == Some(&Token::KwOr) {
            self.advance();
            let rhs = self.parse_and()?;
            let span = lhs.span.merge(rhs.span);
            lhs = Expr { kind: ExprKind::BinOp { op: BinOp::Or, lhs: Box::new(lhs), rhs: Box::new(rhs) }, span };
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_cmp()?;
        while self.peek() == Some(&Token::KwAnd) {
            self.advance();
            let rhs = self.parse_cmp()?;
            let span = lhs.span.merge(rhs.span);
            lhs = Expr { kind: ExprKind::BinOp { op: BinOp::And, lhs: Box::new(lhs), rhs: Box::new(rhs) }, span };
        }
        Ok(lhs)
    }

    fn parse_cmp(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_add()?;
        loop {
            let op = match self.peek() {
                Some(Token::EqEq) => BinOp::EqEq,
                Some(Token::BangEq) => BinOp::NotEq,
                Some(Token::Lt) => BinOp::Lt,
                Some(Token::Le) => BinOp::Le,
                Some(Token::Gt) => BinOp::Gt,
                Some(Token::Ge) => BinOp::Ge,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_add()?;
            let span = lhs.span.merge(rhs.span);
            lhs = Expr { kind: ExprKind::BinOp { op, lhs: Box::new(lhs), rhs: Box::new(rhs) }, span };
        }
        Ok(lhs)
    }

    fn parse_add(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_mul()?;
        loop {
            let op = match self.peek() {
                Some(Token::Plus) => BinOp::Add,
                Some(Token::Minus) => BinOp::Sub,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_mul()?;
            let span = lhs.span.merge(rhs.span);
            lhs = Expr { kind: ExprKind::BinOp { op, lhs: Box::new(lhs), rhs: Box::new(rhs) }, span };
        }
        Ok(lhs)
    }

    fn parse_mul(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Some(Token::Star) => BinOp::Mul,
                Some(Token::Slash) => BinOp::Div,
                Some(Token::Percent) => BinOp::Rem,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_unary()?;
            let span = lhs.span.merge(rhs.span);
            lhs = Expr { kind: ExprKind::BinOp { op, lhs: Box::new(lhs), rhs: Box::new(rhs) }, span };
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr, ParseError> {
        let start = self.current_span().start;
        if self.peek() == Some(&Token::KwNot) {
            self.advance();
            let inner = self.parse_unary()?;
            let end = inner.span.end;
            return Ok(Expr {
                kind: ExprKind::Not(Box::new(inner)),
                span: Span::new(start, end),
            });
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.parse_primary()?;
        loop {
            match self.peek() {
                Some(Token::Dot) => {
                    self.advance();
                    let field = self.eat_ident()?;
                    // If followed by `(`, it's a method call.
                    if self.peek() == Some(&Token::LParen) {
                        self.advance();
                        let (args, named) = self.parse_call_args()?;
                        self.eat(&Token::RParen)?;
                        let end = self.prev_span().end;
                        let expr_start = expr.span.start;
                        let callee = Expr {
                            kind: ExprKind::Field(Box::new(expr), field.clone()),
                            span: field.span,
                        };
                        expr = Expr {
                            kind: ExprKind::Call { callee: Box::new(callee), args, named },
                            span: Span::new(expr_start, end),
                        };
                    } else {
                        let span = expr.span.merge(field.span);
                        expr = Expr { kind: ExprKind::Field(Box::new(expr), field), span };
                    }
                }
                Some(Token::LParen) => {
                    // Direct call: `f(...)`
                    self.advance();
                    let (args, named) = self.parse_call_args()?;
                    self.eat(&Token::RParen)?;
                    let end = self.prev_span().end;
                    let start = expr.span.start;
                    expr = Expr {
                        kind: ExprKind::Call { callee: Box::new(expr), args, named },
                        span: Span::new(start, end),
                    };
                }
                Some(Token::Question) => {
                    let end = self.current_span().end;
                    self.advance();
                    let start = expr.span.start;
                    expr = Expr {
                        kind: ExprKind::Try(Box::new(expr)),
                        span: Span::new(start, end),
                    };
                }
                _ => break,
            }
        }
        Ok(expr)
    }

    /// Parse call arguments: `arg, arg, name = arg, ...`.
    fn parse_call_args(&mut self) -> Result<(Vec<Expr>, Vec<(Ident, Expr)>), ParseError> {
        let mut args = Vec::new();
        let mut named = Vec::new();
        while self.peek() != Some(&Token::RParen) {
            if !args.is_empty() || !named.is_empty() {
                self.eat(&Token::Comma)?;
                if self.peek() == Some(&Token::RParen) {
                    break; // trailing comma
                }
            }
            // Check for named arg: `ident =` (but not `==`).
            let is_named = matches!(self.peek(), Some(Token::Ident(_)))
                && self.peek2() == Some(&Token::Eq);
            if is_named {
                let name = self.eat_ident()?;
                self.eat(&Token::Eq)?;
                let val = self.parse_expr()?;
                named.push((name, val));
            } else {
                args.push(self.parse_expr()?);
            }
        }
        Ok((args, named))
    }

    fn parse_primary(&mut self) -> Result<Expr, ParseError> {
        let start = self.current_span().start;
        match self.peek().cloned() {
            Some(Token::Ident(name)) => {
                let span = self.current_span();
                self.advance();
                Ok(Expr {
                    kind: ExprKind::Ident(Ident::new(name, span)),
                    span,
                })
            }
            Some(Token::IntLit(n)) => {
                let span = self.current_span();
                self.advance();
                Ok(Expr { kind: ExprKind::IntLit(n), span })
            }
            Some(Token::KwTrue) => {
                let span = self.current_span();
                self.advance();
                Ok(Expr { kind: ExprKind::BoolLit(true), span })
            }
            Some(Token::KwFalse) => {
                let span = self.current_span();
                self.advance();
                Ok(Expr { kind: ExprKind::BoolLit(false), span })
            }
            Some(Token::StringLit(s)) => {
                let span = self.current_span();
                self.advance();
                Ok(Expr { kind: ExprKind::StringLit(s), span })
            }
            Some(Token::LParen) => {
                self.advance();
                let inner = self.parse_expr()?;
                self.eat(&Token::RParen)?;
                Ok(inner)
            }
            other => Err(ParseError {
                span: Span::new(start, start),
                msg: format!("expected expression, got {:?}", other),
            }),
        }
    }

    // ── Types ────────────────────────────────────────────────────────────────

    fn parse_type(&mut self) -> Result<TypeExpr, ParseError> {
        let start = self.current_span().start;
        match self.peek().cloned() {
            Some(Token::LParen) => {
                // `()` unit type
                self.advance();
                self.eat(&Token::RParen)?;
                Ok(TypeExpr { kind: TypeKind::Unit, span: Span::new(start, self.prev_span().end) })
            }
            Some(Token::Ident(name)) => {
                let span = self.current_span();
                self.advance();
                let ident = Ident::new(name.clone(), span);
                // `fixed<I, F>`
                if name == "fixed" && self.peek() == Some(&Token::Lt) {
                    self.advance();
                    let i = self.expect_int_lit()? as u32;
                    self.eat(&Token::Comma)?;
                    let f = self.expect_int_lit()? as u32;
                    self.eat(&Token::Gt)?;
                    return Ok(TypeExpr {
                        kind: TypeKind::Fixed(i, f),
                        span: Span::new(start, self.prev_span().end),
                    });
                }
                // `buffer<N>`
                if name == "buffer" && self.peek() == Some(&Token::Lt) {
                    self.advance();
                    let n = self.parse_expr()?;
                    self.eat(&Token::Gt)?;
                    return Ok(TypeExpr {
                        kind: TypeKind::Buffer(Box::new(n)),
                        span: Span::new(start, self.prev_span().end),
                    });
                }
                if name == "bytes" {
                    return Ok(TypeExpr { kind: TypeKind::Bytes, span });
                }
                Ok(TypeExpr { kind: TypeKind::Named(ident), span })
            }
            Some(Token::KwEnum) => {
                self.advance();
                self.eat(&Token::LBrace)?;
                let mut variants = Vec::new();
                while self.peek() != Some(&Token::RBrace) {
                    variants.push(self.eat_ident()?);
                    if self.peek() == Some(&Token::Comma) {
                        self.advance();
                    }
                }
                let end = self.current_span().end;
                self.eat(&Token::RBrace)?;
                Ok(TypeExpr {
                    kind: TypeKind::AnonEnum(variants),
                    span: Span::new(start, end),
                })
            }
            other => Err(ParseError {
                span: Span::new(start, start),
                msg: format!("expected type, got {:?}", other),
            }),
        }
    }

    // ── Duration literals ────────────────────────────────────────────────────

    fn parse_duration(&mut self) -> Result<Duration, ParseError> {
        match self.peek().cloned() {
            Some(Token::DurationLit(v, u)) => {
                let span = self.current_span();
                self.advance();
                Ok(Duration { value: v, unit: u, span })
            }
            other => Err(ParseError {
                span: self.current_span(),
                msg: format!("expected duration literal (e.g. 500ms), got {:?}", other),
            }),
        }
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn eat_optional_semi(&mut self) {
        if self.peek() == Some(&Token::Semi) {
            self.advance();
        }
    }

    fn expect_int_lit(&mut self) -> Result<u64, ParseError> {
        match self.peek().cloned() {
            Some(Token::IntLit(n)) => {
                self.advance();
                Ok(n)
            }
            other => Err(ParseError {
                span: self.current_span(),
                msg: format!("expected integer literal, got {:?}", other),
            }),
        }
    }
}

// ─── Public entry point ───────────────────────────────────────────────────────

pub fn parse(tokens: Vec<Spanned<Token>>) -> Result<Module, ParseError> {
    let mut p = Parser::new(tokens);
    p.parse_module()
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;

    fn parse_src(src: &str) -> Module {
        let tokens = lex(src).expect("lex failed");
        parse(tokens).expect("parse failed")
    }

    #[test]
    fn hello_world_parses() {
        let src = r#"
program hello {
    on sys.start {
        host_io.print("Hello, World!\n")
    }
}
"#;
        let m = parse_src(src);
        assert_eq!(m.items.len(), 1);
        if let Item::Program(p) = &m.items[0] {
            assert_eq!(p.name.name, "hello");
            assert_eq!(p.items.len(), 1);
        } else {
            panic!("expected program item");
        }
    }

    #[test]
    fn every_reaction_parses() {
        let src = r#"
program blink {
    cell lit : bool = false
    every 500ms {
        lit = not lit
    }
}
"#;
        let m = parse_src(src);
        if let Item::Program(p) = &m.items[0] {
            assert_eq!(p.items.len(), 2); // cell + every
        }
    }

    #[test]
    fn device_with_ops_parses() {
        let src = r#"
device host_console {
    ops {
        op print(msg: bytes) -> () {}
    }
}
"#;
        let m = parse_src(src);
        assert_eq!(m.items.len(), 1);
        if let Item::Device(d) = &m.items[0] {
            assert_eq!(d.name.name, "host_console");
        }
    }
}
