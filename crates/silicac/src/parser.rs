//! Recursive-descent parser for Silica — Phase 0 subset.
//!
//! Entry point: `parse(tokens) -> Result<Module, ParseError>`.
//!
//! The parser is intentionally table-free: each grammar rule maps to one
//! function.  The lookahead is always at most 2 tokens.

use crate::ast::*;
use crate::diag::Diag;
use crate::lexer::{Spanned, Token};

// ─── Error ───────────────────────────────────────────────────────────────────

/// Parser diagnostics share the common [`Diag`] type so the CLI renders parse
/// and resolve errors uniformly.
pub type ParseError = Diag;

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
            Some(Token::KwBoard) => Ok(Item::Board(self.parse_board()?)),
            Some(Token::KwInterface) => Ok(Item::Interface(self.parse_interface()?)),
            // `sim` is a contextual keyword (like `safe_state`) so it stays a
            // plain identifier elsewhere.
            Some(Token::Ident(s)) if s == "sim" => Ok(Item::Sim(self.parse_sim()?)),
            other => Err(ParseError {
                span: self.current_span(),
                msg: format!("expected top-level item (program/device/board/interface/sim), got {:?}", other),
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

        // `use board <name> as <alias>` imports a board; `use <path> as <alias>`
        // imports an intrinsic device / module.
        let kind = if self.peek() == Some(&Token::KwBoard) {
            self.advance();
            UseKind::Board
        } else {
            UseKind::Plain
        };

        // path is dot-separated identifiers
        let mut path = vec![self.eat_ident()?];
        while self.peek() == Some(&Token::Dot) {
            self.advance();
            path.push(self.eat_ident()?);
        }
        self.eat(&Token::KwAs)?;
        let alias = self.eat_ident()?;
        self.eat_optional_semi();
        let end = self.prev_span().end;
        Ok(UseDecl { kind, path, alias, span: Span::new(start, end) })
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

        // Optional `within <d>` deadline budget (§4.5/§5.6), between the trigger
        // and any `on fault` clause.
        let within = if self.peek() == Some(&Token::KwWithin) {
            self.advance();
            Some(self.parse_duration()?)
        } else {
            None
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
        Ok(Reaction { trigger, within, fault_disp, body, span: Span::new(start, end) })
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
                Some(Token::KwRegs) => {
                    sections.regs = Some(self.parse_regs_section()?);
                }
                Some(Token::KwConfig) => {
                    sections.config = Some(self.parse_config_section()?);
                }
                Some(Token::KwNeeds) => {
                    sections.needs = Some(self.parse_needs_section()?);
                }
                Some(Token::KwOps) => {
                    sections.ops = Some(self.parse_ops_section()?);
                }
                Some(Token::KwEmits) => {
                    sections.emits.push(self.parse_emit_decl()?);
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
        // `safe` is the conventional safe-op name (§5.6) but also a keyword
        // (the `safe` fault disposition); accept it as an op name here.
        let name = if self.peek() == Some(&Token::KwSafe) {
            let span = self.current_span();
            self.advance();
            Ident::new("safe", span)
        } else {
            self.eat_ident()?
        };
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
            let (fallible, fault_codes) = if self.peek() == Some(&Token::KwOr) {
                self.advance();
                self.eat(&Token::KwFault)?;
                // optional declared code set: `fault{nak, timeout}`
                let mut codes = Vec::new();
                if self.peek() == Some(&Token::LBrace) {
                    self.advance();
                    while self.peek() != Some(&Token::RBrace) {
                        codes.push(self.eat_ident()?);
                        if self.peek() == Some(&Token::Comma) {
                            self.advance();
                        }
                    }
                    self.eat(&Token::RBrace)?;
                }
                (true, codes)
            } else {
                (false, Vec::new())
            };
            ReturnType { ty, fallible, fault_codes }
        } else {
            ReturnType {
                ty: TypeExpr { kind: TypeKind::Unit, span: Span::default() },
                fallible: false,
                fault_codes: Vec::new(),
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

    // ── Device: regs / config / needs / emits ────────────────────────────────

    fn parse_regs_section(&mut self) -> Result<RegsSection, ParseError> {
        let start = self.current_span().start;
        self.eat(&Token::KwRegs)?;
        self.eat(&Token::LBrace)?;
        let mut regs = Vec::new();
        while self.peek() != Some(&Token::RBrace) {
            if self.at_end() {
                return Err(self.error("unexpected EOF in regs section"));
            }
            regs.push(self.parse_reg_decl()?);
        }
        let end = self.current_span().end;
        self.eat(&Token::RBrace)?;
        Ok(RegsSection { regs, span: Span::new(start, end) })
    }

    /// `<NAME> : reg32 at <offset> [access <acc>] { <fields> }`
    fn parse_reg_decl(&mut self) -> Result<RegDecl, ParseError> {
        let start = self.current_span().start;
        let name = self.eat_ident()?;
        self.eat(&Token::Colon)?;
        let ty = self.eat_ident()?;
        let width = match ty.name.as_str() {
            "reg8" => 8,
            "reg16" => 16,
            "reg32" => 32,
            other => {
                return Err(ParseError {
                    span: ty.span,
                    msg: format!("expected reg8/reg16/reg32, got '{}'", other),
                })
            }
        };
        self.eat(&Token::KwAt)?;
        let offset = self.expect_int_lit()?;

        // optional `access <acc>` and other register-level qualifiers
        let mut access = RegAccess::Rw;
        access = self.parse_opt_access(access)?;

        // optional field block
        let mut fields = Vec::new();
        if self.peek() == Some(&Token::LBrace) {
            self.advance();
            while self.peek() != Some(&Token::RBrace) {
                if self.at_end() {
                    return Err(self.error("unexpected EOF in register field list"));
                }
                fields.push(self.parse_field_decl()?);
                if self.peek() == Some(&Token::Comma) {
                    self.advance();
                }
            }
            self.eat(&Token::RBrace)?;
        }
        let end = self.prev_span().end;
        Ok(RegDecl { name, width, offset, access, fields, span: Span::new(start, end) })
    }

    /// Parse an optional `access <acc>` qualifier plus the §4.2 modifier
    /// idents (`side_effect`, `pop_on_read`, `reserved`) which the slice
    /// records-then-ignores beyond the access direction.
    fn parse_opt_access(&mut self, default: RegAccess) -> Result<RegAccess, ParseError> {
        let mut access = default;
        if matches!(self.peek(), Some(Token::Ident(s)) if s == "access") {
            self.advance();
            access = self.parse_access_kind()?;
        }
        // Swallow trailing register-semantics modifiers we don't model yet.
        while matches!(self.peek(), Some(Token::Ident(s))
            if s == "side_effect" || s == "pop_on_read" || s == "reserved")
        {
            self.advance();
        }
        Ok(access)
    }

    fn parse_access_kind(&mut self) -> Result<RegAccess, ParseError> {
        let acc = self.eat_ident()?;
        match acc.name.as_str() {
            "ro" => Ok(RegAccess::Ro),
            "wo" => Ok(RegAccess::Wo),
            "rw" => Ok(RegAccess::Rw),
            "w1c" => Ok(RegAccess::W1c),
            "rc" => Ok(RegAccess::Rc),
            other => Err(ParseError {
                span: acc.span,
                msg: format!("unknown access qualifier '{}' (expected ro/wo/rw/w1c/rc)", other),
            }),
        }
    }

    /// `<name> : bit[<n>]` or `<name> : field[<hi>:<lo>]` [access <acc>]
    fn parse_field_decl(&mut self) -> Result<FieldDecl, ParseError> {
        let start = self.current_span().start;
        let name = self.eat_ident()?;
        self.eat(&Token::Colon)?;
        let kind = self.eat_ident()?;
        self.eat(&Token::LBracket)?;
        let bits = match kind.name.as_str() {
            "bit" => {
                let n = self.expect_int_lit()? as u8;
                BitSpec::Bit(n)
            }
            "field" => {
                let hi = self.expect_int_lit()? as u8;
                self.eat(&Token::Colon)?;
                let lo = self.expect_int_lit()? as u8;
                BitSpec::Range(hi, lo)
            }
            other => {
                return Err(ParseError {
                    span: kind.span,
                    msg: format!("expected 'bit' or 'field', got '{}'", other),
                })
            }
        };
        self.eat(&Token::RBracket)?;
        let access = if matches!(self.peek(), Some(Token::Ident(s)) if s == "access") {
            self.advance();
            Some(self.parse_access_kind()?)
        } else {
            None
        };
        let end = self.prev_span().end;
        Ok(FieldDecl { name, bits, access, span: Span::new(start, end) })
    }

    fn parse_config_section(&mut self) -> Result<ConfigSection, ParseError> {
        let start = self.current_span().start;
        self.eat(&Token::KwConfig)?;
        self.eat(&Token::LBrace)?;
        let mut fields = Vec::new();
        while self.peek() != Some(&Token::RBrace) {
            if self.at_end() {
                return Err(self.error("unexpected EOF in config section"));
            }
            let fstart = self.current_span().start;
            let name = self.eat_ident()?;
            self.eat(&Token::Colon)?;
            let ty = self.parse_type()?;
            let constraint = if self.peek() == Some(&Token::KwWhere) {
                self.advance();
                // Parse below assignment precedence so a following `= <default>`
                // is not swallowed into the constraint as an assignment expr.
                Some(self.parse_or()?)
            } else {
                None
            };
            let default = if self.peek() == Some(&Token::Eq) {
                self.advance();
                Some(self.parse_expr()?)
            } else {
                None
            };
            self.eat_optional_semi();
            let fend = self.prev_span().end;
            fields.push(ConfigField { name, ty, constraint, default, span: Span::new(fstart, fend) });
        }
        let end = self.current_span().end;
        self.eat(&Token::RBrace)?;
        Ok(ConfigSection { fields, span: Span::new(start, end) })
    }

    fn parse_needs_section(&mut self) -> Result<NeedsSection, ParseError> {
        let start = self.current_span().start;
        self.eat(&Token::KwNeeds)?;
        self.eat(&Token::LBrace)?;
        let mut needs = Vec::new();
        while self.peek() != Some(&Token::RBrace) {
            if self.at_end() {
                return Err(self.error("unexpected EOF in needs section"));
            }
            let nstart = self.current_span().start;
            let name = self.eat_ident()?;
            self.eat(&Token::Colon)?;
            let ty = self.eat_ident()?;
            // optional `= <default>` (e.g. `addr : i2c.address = 0x76`) — skip value
            if self.peek() == Some(&Token::Eq) {
                self.advance();
                let _ = self.parse_expr()?;
            }
            self.eat_optional_semi();
            let nend = self.prev_span().end;
            needs.push(NeedDecl { name, ty, span: Span::new(nstart, nend) });
        }
        let end = self.current_span().end;
        self.eat(&Token::RBrace)?;
        Ok(NeedsSection { needs, span: Span::new(start, end) })
    }

    /// `emits <name> : event [when <expr>]`
    fn parse_emit_decl(&mut self) -> Result<EmitDecl, ParseError> {
        let start = self.current_span().start;
        self.eat(&Token::KwEmits)?;
        let name = self.eat_ident()?;
        self.eat(&Token::Colon)?;
        // the type word is always `event`; accept any ident here
        let _ev = self.eat_ident()?;
        let when = if self.peek() == Some(&Token::KwWhen) {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };
        self.eat_optional_semi();
        let end = self.prev_span().end;
        Ok(EmitDecl { name, when, span: Span::new(start, end) })
    }

    // ── Interface (thin stub) ─────────────────────────────────────────────────

    fn parse_interface(&mut self) -> Result<InterfaceDef, ParseError> {
        let start = self.current_span().start;
        self.eat(&Token::KwInterface)?;
        let name = self.eat_ident()?;
        self.eat(&Token::LBrace)?;
        let mut ops = Vec::new();
        let mut types = Vec::new();
        while self.peek() != Some(&Token::RBrace) {
            if self.at_end() {
                return Err(self.error("unexpected EOF in interface body"));
            }
            match self.peek() {
                Some(Token::KwOp) => ops.push(self.parse_op_decl()?),
                // `type address = u7`
                Some(Token::Ident(s)) if s == "type" => {
                    self.advance();
                    let alias = self.eat_ident()?;
                    self.eat(&Token::Eq)?;
                    let target = self.eat_ident()?;
                    self.eat_optional_semi();
                    types.push((alias, target));
                }
                other => {
                    return Err(ParseError {
                        span: self.current_span(),
                        msg: format!("expected `op` or `type` in interface body, got {:?}", other),
                    })
                }
            }
        }
        let end = self.current_span().end;
        self.eat(&Token::RBrace)?;
        Ok(InterfaceDef { name, ops, types, span: Span::new(start, end) })
    }

    fn skip_balanced_braces(&mut self) -> Result<(), ParseError> {
        self.eat(&Token::LBrace)?;
        let mut depth = 1usize;
        while depth > 0 {
            match self.peek() {
                Some(Token::LBrace) => depth += 1,
                Some(Token::RBrace) => depth -= 1,
                None => return Err(self.error("unexpected EOF inside braces")),
                _ => {}
            }
            self.advance();
        }
        Ok(())
    }

    // ── Board ─────────────────────────────────────────────────────────────────

    fn parse_board(&mut self) -> Result<BoardDef, ParseError> {
        let start = self.current_span().start;
        self.eat(&Token::KwBoard)?;
        let name = self.eat_ident()?;
        self.eat(&Token::LBrace)?;

        let mut soc = None;
        let mut instances = Vec::new();
        let mut pinctrl = Vec::new();
        let mut pin_bindings = Vec::new();

        while self.peek() != Some(&Token::RBrace) {
            if self.at_end() {
                return Err(self.error("unexpected EOF in board body"));
            }
            match self.peek() {
                Some(Token::Ident(s)) if s == "soc" => {
                    soc = Some(self.parse_soc()?);
                }
                Some(Token::Ident(s)) if s == "pinctrl" => {
                    self.advance();
                    self.eat(&Token::LBrace)?;
                    while self.peek() != Some(&Token::RBrace) {
                        pinctrl.push(self.parse_pinmux()?);
                    }
                    self.eat(&Token::RBrace)?;
                }
                // Otherwise it's `<name> : ...` — either a pin binding or an
                // instance.  Disambiguate by looking for `gpio.pin =`.
                Some(Token::Ident(_)) => {
                    if self.is_pin_binding() {
                        pin_bindings.push(self.parse_pin_binding()?);
                    } else {
                        instances.push(self.parse_instance()?);
                    }
                }
                other => {
                    return Err(ParseError {
                        span: self.current_span(),
                        msg: format!("unexpected item in board body: {:?}", other),
                    })
                }
            }
        }
        let end = self.current_span().end;
        self.eat(&Token::RBrace)?;
        Ok(BoardDef { name, soc, instances, pinctrl, pin_bindings, span: Span::new(start, end) })
    }

    /// True if the upcoming declaration is `<name> : <type> = ...` (a pin
    /// binding), as opposed to `<name> : <type> at ...` / `{ ... }` (instance).
    fn is_pin_binding(&self) -> bool {
        // pattern: Ident Colon Ident [Dot Ident] Eq
        // Scan a few tokens ahead for an `=` before any `{` or `at`.
        let mut k = self.pos;
        let toks = &self.tokens;
        // name
        if !matches!(toks.get(k).map(|t| &t.inner), Some(Token::Ident(_))) { return false; }
        k += 1;
        if toks.get(k).map(|t| &t.inner) != Some(&Token::Colon) { return false; }
        k += 1;
        // type path: Ident (Dot Ident)*
        loop {
            match toks.get(k).map(|t| &t.inner) {
                Some(Token::Ident(_)) => { k += 1; }
                _ => return false,
            }
            if toks.get(k).map(|t| &t.inner) == Some(&Token::Dot) {
                k += 1;
                continue;
            }
            break;
        }
        matches!(toks.get(k).map(|t| &t.inner), Some(Token::Eq))
    }

    fn parse_soc(&mut self) -> Result<SocDef, ParseError> {
        let start = self.current_span().start;
        self.advance(); // `soc`
        let name = self.eat_ident()?;
        self.eat(&Token::LBrace)?;
        let mut memory = Vec::new();
        let mut clocks = Vec::new();
        let mut irqs = Vec::new();
        while self.peek() != Some(&Token::RBrace) {
            if self.at_end() {
                return Err(self.error("unexpected EOF in soc body"));
            }
            match self.peek() {
                Some(Token::Ident(s)) if s == "memory" => {
                    self.advance();
                    self.eat(&Token::LBrace)?;
                    while self.peek() != Some(&Token::RBrace) {
                        memory.push(self.parse_region()?);
                    }
                    self.eat(&Token::RBrace)?;
                }
                Some(Token::Ident(s)) if s == "clocks" => {
                    self.advance();
                    self.eat(&Token::LBrace)?;
                    while self.peek() != Some(&Token::RBrace) {
                        clocks.push(self.parse_clock()?);
                    }
                    self.eat(&Token::RBrace)?;
                }
                Some(Token::Ident(s)) if s == "irqs" => {
                    self.advance();
                    self.eat(&Token::LBrace)?;
                    while self.peek() != Some(&Token::RBrace) {
                        irqs.push(self.parse_irq()?);
                    }
                    self.eat(&Token::RBrace)?;
                }
                other => {
                    return Err(ParseError {
                        span: self.current_span(),
                        msg: format!("unexpected soc section: {:?}", other),
                    })
                }
            }
        }
        let end = self.current_span().end;
        self.eat(&Token::RBrace)?;
        Ok(SocDef { name, memory, clocks, irqs, span: Span::new(start, end) })
    }

    /// `<name> : region at <addr> size <size>`
    fn parse_region(&mut self) -> Result<RegionDecl, ParseError> {
        let start = self.current_span().start;
        let name = self.eat_ident()?;
        self.eat(&Token::Colon)?;
        let _region_ty = self.eat_ident()?; // `region`
        self.eat(&Token::KwAt)?;
        let at = self.expect_int_lit()?;
        // `size <SizeLit|IntLit>`
        let size = if matches!(self.peek(), Some(Token::Ident(s)) if s == "size") {
            self.advance();
            self.expect_size_or_int()?
        } else {
            0
        };
        self.eat_optional_semi();
        let end = self.prev_span().end;
        Ok(RegionDecl { name, at, size, span: Span::new(start, end) })
    }

    /// `<name> : clock_source = <expr>`
    fn parse_clock(&mut self) -> Result<ClockDecl, ParseError> {
        let start = self.current_span().start;
        let name = self.eat_ident()?;
        self.eat(&Token::Colon)?;
        let _ty = self.eat_ident()?; // `clock_source`
        self.eat(&Token::Eq)?;
        let init = self.parse_expr()?;
        self.eat_optional_semi();
        let end = self.prev_span().end;
        Ok(ClockDecl { name, init, span: Span::new(start, end) })
    }

    /// `<name> : irq_line = <int>`
    fn parse_irq(&mut self) -> Result<IrqDecl, ParseError> {
        let start = self.current_span().start;
        let name = self.eat_ident()?;
        self.eat(&Token::Colon)?;
        let _ty = self.eat_ident()?; // `irq_line`
        self.eat(&Token::Eq)?;
        let num = self.expect_int_lit()? as u32;
        self.eat_optional_semi();
        let end = self.prev_span().end;
        Ok(IrqDecl { name, num, span: Span::new(start, end) })
    }

    /// `<name> : <device-type> [at <addr>] [{ [config {...}] [needs {...}] }]`
    fn parse_instance(&mut self) -> Result<Instance, ParseError> {
        let start = self.current_span().start;
        let name = self.eat_ident()?;
        self.eat(&Token::Colon)?;
        let device_ty = self.eat_ident()?;
        let at = if self.peek() == Some(&Token::KwAt) {
            self.advance();
            Some(self.expect_int_lit()?)
        } else {
            None
        };
        let mut config = Vec::new();
        let mut needs = Vec::new();
        if self.peek() == Some(&Token::LBrace) {
            self.advance();
            while self.peek() != Some(&Token::RBrace) {
                if self.at_end() {
                    return Err(self.error("unexpected EOF in instance body"));
                }
                match self.peek() {
                    Some(Token::KwConfig) => {
                        self.advance();
                        self.eat(&Token::LBrace)?;
                        while self.peek() != Some(&Token::RBrace) {
                            let key = self.eat_ident()?;
                            self.eat(&Token::Eq)?;
                            let val = self.parse_expr()?;
                            self.eat_optional_semi();
                            if self.peek() == Some(&Token::Comma) { self.advance(); }
                            config.push((key, val));
                        }
                        self.eat(&Token::RBrace)?;
                    }
                    Some(Token::KwNeeds) => {
                        self.advance();
                        self.eat(&Token::LBrace)?;
                        while self.peek() != Some(&Token::RBrace) {
                            let key = self.eat_ident()?;
                            self.eat(&Token::Eq)?;
                            let path = self.parse_dotted_path()?;
                            self.eat_optional_semi();
                            if self.peek() == Some(&Token::Comma) { self.advance(); }
                            needs.push((key, path));
                        }
                        self.eat(&Token::RBrace)?;
                    }
                    other => {
                        return Err(ParseError {
                            span: self.current_span(),
                            msg: format!("unexpected section in instance body: {:?}", other),
                        })
                    }
                }
            }
            self.eat(&Token::RBrace)?;
        }
        let end = self.prev_span().end;
        Ok(Instance { name, device_ty, at, config, needs, span: Span::new(start, end) })
    }

    /// `<name> : gpio.pin = <port>.pin(<index>) as <dir> [pulling up|down]`
    fn parse_pin_binding(&mut self) -> Result<PinBinding, ParseError> {
        let start = self.current_span().start;
        let name = self.eat_ident()?;
        self.eat(&Token::Colon)?;
        // type path `gpio.pin` — consume and ignore
        let _ = self.parse_dotted_path()?;
        self.eat(&Token::Eq)?;
        let (port, index, dir, pull, alt) = self.parse_pin_spec()?;
        self.eat_optional_semi();
        let end = self.prev_span().end;
        Ok(PinBinding { name, port, index, dir, pull, alt, span: Span::new(start, end) })
    }

    /// Parse `<port>.pin(<index>) [as <attrs>]` and return its attributes.
    fn parse_pin_spec(&mut self) -> Result<(Ident, u64, PinDir, Pull, Option<u32>), ParseError> {
        let port = self.eat_ident()?;
        self.eat(&Token::Dot)?;
        let pin_kw = self.eat_ident()?; // `pin`
        if pin_kw.name != "pin" {
            return Err(ParseError { span: pin_kw.span, msg: format!("expected 'pin', got '{}'", pin_kw.name) });
        }
        self.eat(&Token::LParen)?;
        let index = self.expect_int_lit()?;
        self.eat(&Token::RParen)?;

        let mut dir = PinDir::Input;
        let mut pull = Pull::None;
        let mut alt = None;
        if self.peek() == Some(&Token::KwAs) {
            self.advance();
            // first attribute: output / input / alt_fn(n)
            match self.peek().cloned() {
                Some(Token::Ident(s)) if s == "output" => { self.advance(); dir = PinDir::Output; }
                Some(Token::Ident(s)) if s == "input" => { self.advance(); dir = PinDir::Input; }
                Some(Token::Ident(s)) if s == "alt_fn" => {
                    self.advance();
                    self.eat(&Token::LParen)?;
                    alt = Some(self.expect_int_lit()? as u32);
                    self.eat(&Token::RParen)?;
                    dir = PinDir::Alt;
                }
                other => {
                    return Err(ParseError {
                        span: self.current_span(),
                        msg: format!("expected pin direction (output/input/alt_fn), got {:?}", other),
                    })
                }
            }
            // trailing attributes: `pulling up|down`, `drive push_pull`, `speed high`
            loop {
                match self.peek().cloned() {
                    Some(Token::Ident(s)) if s == "pulling" => {
                        self.advance();
                        match self.peek().cloned() {
                            Some(Token::Ident(u)) if u == "up" => { self.advance(); pull = Pull::Up; }
                            Some(Token::Ident(d)) if d == "down" => { self.advance(); pull = Pull::Down; }
                            other => return Err(ParseError {
                                span: self.current_span(),
                                msg: format!("expected up/down after 'pulling', got {:?}", other),
                            }),
                        }
                    }
                    Some(Token::Ident(s)) if s == "drive" => { self.advance(); let _ = self.eat_ident()?; }
                    Some(Token::Ident(s)) if s == "speed" => { self.advance(); let _ = self.eat_ident()?; }
                    _ => break,
                }
            }
        }
        Ok((port, index, dir, pull, alt))
    }

    /// `<name> : pinmux { <role> = <port>.pin(n) as alt_fn(k) ... }`
    fn parse_pinmux(&mut self) -> Result<PinmuxDef, ParseError> {
        let start = self.current_span().start;
        let name = self.eat_ident()?;
        self.eat(&Token::Colon)?;
        let _ty = self.eat_ident()?; // `pinmux`
        self.eat(&Token::LBrace)?;
        let mut pins = Vec::new();
        while self.peek() != Some(&Token::RBrace) {
            if self.at_end() {
                return Err(self.error("unexpected EOF in pinmux body"));
            }
            let pstart = self.current_span().start;
            let role = self.eat_ident()?;
            self.eat(&Token::Eq)?;
            let (port, index, _dir, pull, alt) = self.parse_pin_spec()?;
            self.eat_optional_semi();
            let pend = self.prev_span().end;
            pins.push(PinAssign { role, port, index, alt, pull, span: Span::new(pstart, pend) });
        }
        let end = self.current_span().end;
        self.eat(&Token::RBrace)?;
        Ok(PinmuxDef { name, pins, span: Span::new(start, end) })
    }

    fn parse_dotted_path(&mut self) -> Result<Vec<Ident>, ParseError> {
        let mut path = vec![self.eat_ident()?];
        while self.peek() == Some(&Token::Dot) {
            self.advance();
            path.push(self.eat_ident()?);
        }
        Ok(path)
    }

    // ── Sim script ────────────────────────────────────────────────────────────

    fn parse_sim(&mut self) -> Result<SimDef, ParseError> {
        let start = self.current_span().start;
        self.advance(); // `sim`
        let name = self.eat_ident()?;
        self.eat(&Token::KwFor)?;
        let program = self.eat_ident()?;
        self.eat(&Token::LBrace)?;
        let mut injections = Vec::new();
        let mut faults = Vec::new();
        let mut bus_faults = Vec::new();
        let mut bus_hangs = 0u32;
        let mut run_until = None;
        while self.peek() != Some(&Token::RBrace) {
            if self.at_end() {
                return Err(self.error("unexpected EOF in sim body"));
            }
            match self.peek().cloned() {
                Some(Token::Ident(s)) if s == "inject" => {
                    let istart = self.current_span().start;
                    self.advance();
                    // `inject fault <addr> at <dur>` vs `inject <event> at <dur>`.
                    if self.peek() == Some(&Token::KwFault) {
                        self.advance();
                        let addr = self.expect_int_lit()?;
                        self.eat(&Token::KwAt)?;
                        let at = self.parse_duration()?;
                        self.eat_optional_semi();
                        let iend = self.prev_span().end;
                        faults.push(FaultInjection { addr, at, span: Span::new(istart, iend) });
                    } else if matches!(self.peek(), Some(Token::Ident(s)) if s == "bus_hang") {
                        // `inject bus_hang [times <n>]`
                        self.advance();
                        let times = if matches!(self.peek(), Some(Token::Ident(s)) if s == "times") {
                            self.advance();
                            self.expect_int_lit()? as u32
                        } else {
                            1
                        };
                        self.eat_optional_semi();
                        bus_hangs += times;
                    } else if matches!(self.peek(), Some(Token::Ident(s)) if s == "bus_fault") {
                        // `inject bus_fault <code> times <n>`
                        self.advance();
                        let code = self.eat_ident()?;
                        let times = if matches!(self.peek(), Some(Token::Ident(s)) if s == "times") {
                            self.advance();
                            self.expect_int_lit()? as u32
                        } else {
                            1
                        };
                        self.eat_optional_semi();
                        bus_faults.push((code, times));
                    } else {
                        let event = self.parse_event_ref()?;
                        self.eat(&Token::KwAt)?;
                        let at = self.parse_duration()?;
                        self.eat_optional_semi();
                        let iend = self.prev_span().end;
                        injections.push(Injection { event, at, span: Span::new(istart, iend) });
                    }
                }
                Some(Token::Ident(s)) if s == "run" => {
                    self.advance();
                    // `until <duration>`
                    match self.peek().cloned() {
                        Some(Token::Ident(u)) if u == "until" => { self.advance(); }
                        other => return Err(ParseError {
                            span: self.current_span(),
                            msg: format!("expected 'until' after 'run', got {:?}", other),
                        }),
                    }
                    run_until = Some(self.parse_duration()?);
                    self.eat_optional_semi();
                }
                other => {
                    return Err(ParseError {
                        span: self.current_span(),
                        msg: format!("expected 'inject' or 'run until' in sim body, got {:?}", other),
                    })
                }
            }
        }
        let end = self.current_span().end;
        self.eat(&Token::RBrace)?;
        Ok(SimDef { name, program, injections, faults, bus_faults, bus_hangs, run_until, span: Span::new(start, end) })
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
            Some(Token::KwAtomic) => {
                self.advance();
                let block = self.parse_block()?;
                Ok(Stmt::Atomic(block, Span::new(start, self.prev_span().end)))
            }
            Some(Token::KwPoll) => {
                self.advance();
                // `cond` is parsed below assignment precedence so the trailing
                // `within`/`else` clauses are never swallowed.
                let cond = self.parse_or()?;
                self.eat(&Token::KwWithin)?;
                let within = self.parse_duration()?;
                self.eat(&Token::KwElse)?;
                self.eat(&Token::KwFault)?;
                let fault_code = self.eat_ident()?;
                self.eat_optional_semi();
                Ok(Stmt::Poll { cond, within, fault_code, span: Span::new(start, self.prev_span().end) })
            }
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
            // Don't consume a `+`/`-` that is really the start of a compound
            // assignment (`+=` / `-=`); leave it for `parse_assign`.
            let compound = self.peek2() == Some(&Token::Eq);
            let op = match self.peek() {
                Some(Token::Plus) if !compound => BinOp::Add,
                Some(Token::Minus) if !compound => BinOp::Sub,
                Some(Token::PlusPercent) => BinOp::AddWrap,
                Some(Token::PlusPipe) => BinOp::AddSat,
                Some(Token::MinusPercent) => BinOp::SubWrap,
                Some(Token::MinusPipe) => BinOp::SubSat,
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
            // Likewise leave `*=` / `/=` for `parse_assign`.
            let compound = self.peek2() == Some(&Token::Eq);
            let op = match self.peek() {
                Some(Token::Star) if !compound => BinOp::Mul,
                Some(Token::Slash) if !compound => BinOp::Div,
                Some(Token::Percent) => BinOp::Rem,
                Some(Token::StarPercent) => BinOp::MulWrap,
                Some(Token::StarPipe) => BinOp::MulSat,
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
            // Frequency / size literals fold to their integer value (Hz / bytes)
            // in expression position — e.g. a clock init `= 8MHz`.
            Some(Token::FreqLit(hz)) => {
                let span = self.current_span();
                self.advance();
                Ok(Expr { kind: ExprKind::IntLit(hz), span })
            }
            Some(Token::SizeLit(bytes)) => {
                let span = self.current_span();
                self.advance();
                Ok(Expr { kind: ExprKind::IntLit(bytes), span })
            }
            // Duration literals fold to nanoseconds in expression position —
            // e.g. a config value `timeout = 100ms`.
            Some(Token::DurationLit(v, u)) => {
                let span = self.current_span();
                self.advance();
                Ok(Expr { kind: ExprKind::IntLit(u.to_ns(v)), span })
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

    /// Accept either a size literal (`512K`) or a plain integer as a byte count.
    fn expect_size_or_int(&mut self) -> Result<u64, ParseError> {
        match self.peek().cloned() {
            Some(Token::SizeLit(n)) | Some(Token::IntLit(n)) => {
                self.advance();
                Ok(n)
            }
            other => Err(ParseError {
                span: self.current_span(),
                msg: format!("expected size literal (e.g. 512K) or integer, got {:?}", other),
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

    #[test]
    fn device_regs_needs_emits_parses() {
        let src = r#"
device gpio {
    regs {
        IDR : reg32 at 0x10 access ro {}
        ODR : reg32 at 0x14 access rw {}
    }
    needs { clock : clock_source }
    ops {
        op set(level: bool) -> () {}
        op get() -> bool {}
    }
    emits falling : event
}
"#;
        let m = parse_src(src);
        if let Item::Device(d) = &m.items[0] {
            let regs = d.sections.regs.as_ref().expect("regs section");
            assert_eq!(regs.regs.len(), 2);
            assert_eq!(regs.regs[0].name.name, "IDR");
            assert_eq!(regs.regs[0].offset, 0x10);
            assert_eq!(regs.regs[0].access, RegAccess::Ro);
            assert_eq!(regs.regs[1].access, RegAccess::Rw);
            let needs = d.sections.needs.as_ref().expect("needs section");
            assert_eq!(needs.needs[0].name.name, "clock");
            assert_eq!(d.sections.emits.len(), 1);
            assert_eq!(d.sections.emits[0].name.name, "falling");
        } else {
            panic!("expected device");
        }
    }

    #[test]
    fn board_with_soc_instances_pins_parses() {
        let src = r#"
board nucleo_f401re {
    soc stm32f401re {
        memory {
            flash : region at 0x0800_0000 size 512K
            sram  : region at 0x2000_0000 size 96K
        }
        clocks {
            hse    : clock_source = 8MHz
            sysclk : clock_source = pll(hse, mul = 84, div = 8)
        }
        irqs { usart2_irq : irq_line = 38 }
    }

    gpio_a : gpio at 0x4002_0000 { needs { clock = soc.sysclk } }
    gpio_c : gpio at 0x4002_0800 { needs { clock = soc.sysclk } }

    led_user : gpio.pin = gpio_a.pin(5)  as output
    btn_user : gpio.pin = gpio_c.pin(13) as input pulling up
}
"#;
        let m = parse_src(src);
        assert_eq!(m.items.len(), 1);
        if let Item::Board(b) = &m.items[0] {
            assert_eq!(b.name.name, "nucleo_f401re");
            let soc = b.soc.as_ref().expect("soc");
            assert_eq!(soc.memory.len(), 2);
            assert_eq!(soc.memory[0].at, 0x0800_0000);
            assert_eq!(soc.memory[0].size, 512 * 1024);
            assert_eq!(soc.clocks.len(), 2);
            assert_eq!(soc.irqs[0].num, 38);
            assert_eq!(b.instances.len(), 2);
            assert_eq!(b.instances[0].name.name, "gpio_a");
            assert_eq!(b.instances[0].at, Some(0x4002_0000));
            assert_eq!(b.instances[0].needs[0].0.name, "clock");
            assert_eq!(b.pin_bindings.len(), 2);
            assert_eq!(b.pin_bindings[0].name.name, "led_user");
            assert_eq!(b.pin_bindings[0].port.name, "gpio_a");
            assert_eq!(b.pin_bindings[0].index, 5);
            assert_eq!(b.pin_bindings[0].dir, PinDir::Output);
            assert_eq!(b.pin_bindings[1].index, 13);
            assert_eq!(b.pin_bindings[1].dir, PinDir::Input);
            assert_eq!(b.pin_bindings[1].pull, Pull::Up);
        } else {
            panic!("expected board");
        }
    }

    #[test]
    fn sim_block_parses() {
        let src = r#"
sim blink_demo for blink {
    inject btn_user.falling at 1200ms
    inject btn_user.falling at 1800ms
    run until 3000ms
}
"#;
        let m = parse_src(src);
        if let Item::Sim(s) = &m.items[0] {
            assert_eq!(s.name.name, "blink_demo");
            assert_eq!(s.program.name, "blink");
            assert_eq!(s.injections.len(), 2);
            assert_eq!(s.injections[0].event.event.name, "falling");
            assert_eq!(s.injections[0].at.to_ns(), 1_200_000_000);
            assert_eq!(s.run_until.unwrap().to_ns(), 3_000_000_000);
            assert!(s.faults.is_empty());
        } else {
            panic!("expected sim");
        }
    }

    #[test]
    fn compound_assignment_parses() {
        // Regression: `+=`/`-=`/`*=`/`/=` must parse as a compound assignment,
        // not as a binary `+` followed by a stray `=`.
        let src = r#"
program p {
    cell n : u32 = 0
    every 1s { n += 1 }
}
"#;
        let m = parse_src(src);
        if let Item::Program(prog) = &m.items[0] {
            let r = prog.items.iter().find_map(|i| match i {
                ProgramItem::Reaction(r) => Some(r),
                _ => None,
            }).expect("reaction");
            match &r.body.stmts[0] {
                Stmt::Expr(e) => assert!(matches!(e.kind, ExprKind::CompoundAssign(BinOp::Add, _, _))),
                other => panic!("expected compound assign, got {:?}", other),
            }
        }
    }

    #[test]
    fn sim_fault_injection_parses() {
        let src = r#"
sim s for p {
    inject fault 0x4001_0000 at 800ms
    run until 1000ms
}
"#;
        let m = parse_src(src);
        if let Item::Sim(s) = &m.items[0] {
            assert_eq!(s.faults.len(), 1);
            assert_eq!(s.faults[0].addr, 0x4001_0000);
            assert_eq!(s.faults[0].at.to_ns(), 800_000_000);
        } else {
            panic!("expected sim");
        }
    }

    #[test]
    fn use_board_parses() {
        let src = r#"
program blink {
    use board nucleo_f401re as nucleo
    let led = nucleo.led_user
}
"#;
        let m = parse_src(src);
        if let Item::Program(p) = &m.items[0] {
            if let ProgramItem::UseDecl(u) = &p.items[0] {
                assert_eq!(u.kind, UseKind::Board);
                assert_eq!(u.path[0].name, "nucleo_f401re");
                assert_eq!(u.alias.name, "nucleo");
            } else {
                panic!("expected use decl");
            }
        }
    }
}
