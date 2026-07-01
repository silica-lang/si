//! DTS→Silica importer — MVP spike (audit #35 P7-8a, §8).
//!
//! Harvests hardware *facts* (base addresses, memory regions) from a **flat**
//! Device Tree Source subset and emits a `board`/`soc` skeleton (§3.3).  This is
//! the minimal spike: it parses an already-preprocessed `.dts` (the `cpp` phase
//! of §8's ingestion pipeline is out of scope here) covering the root node,
//! `model`, a `soc` node, `memory` nodes, and device nodes with `reg`.
//!
//! Per §8/D10 the mapping is **typed and diagnosed, never a silent scrape**: a
//! device node whose `compatible` has no Silica device type becomes a commented
//! stub *and* emits a diagnostic — nothing is dropped without a trace.  Node
//! coverage for pins/clocks and a round-trip test are the P7-8b follow-up.

/// A parsed Device Tree node.
#[derive(Debug, Clone, Default)]
pub struct DtsNode {
    /// The `label:` before the node name, if any (e.g. `gpio0`).
    pub label: Option<String>,
    /// The node name without its unit address (e.g. `gpio`; `/` for the root).
    pub name: String,
    /// The `@<addr>` unit address, if present.
    pub unit_addr: Option<u64>,
    pub props: Vec<(String, DtsValue)>,
    pub children: Vec<DtsNode>,
}

/// A property value in the supported subset.
#[derive(Debug, Clone, PartialEq)]
pub enum DtsValue {
    /// `<a b c>` — a list of 32-bit cells (phandles / unparsable cells are 0).
    Cells(Vec<u64>),
    /// `"…"` — a string.
    Str(String),
    /// A bare property (`foo;`).
    Bool,
}

impl DtsNode {
    fn prop(&self, name: &str) -> Option<&DtsValue> {
        self.props.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }
    fn str_prop(&self, name: &str) -> Option<&str> {
        match self.prop(name) {
            Some(DtsValue::Str(s)) => Some(s.as_str()),
            _ => None,
        }
    }
    fn child(&self, name: &str) -> Option<&DtsNode> {
        self.children.iter().find(|c| c.name == name)
    }
    /// `true` if this node is a memory node (`device_type = "memory"` or the
    /// node is named `memory`).
    fn is_memory(&self) -> bool {
        self.name == "memory" || self.str_prop("device_type") == Some("memory")
    }
    /// `reg = <addr size>` → `(addr, size)` under the common #address-cells=1 /
    /// #size-cells=1 layout.
    fn reg(&self) -> Option<(u64, u64)> {
        match self.prop("reg") {
            Some(DtsValue::Cells(c)) if c.len() >= 2 => Some((c[0], c[1])),
            Some(DtsValue::Cells(c)) if c.len() == 1 => Some((c[0], 0)),
            _ => None,
        }
    }
}

// ─── Parser ───────────────────────────────────────────────────────────────────

/// Parse a flat DTS source into its root node.  Returns an error string on a
/// malformed subset.
pub fn parse(src: &str) -> Result<DtsNode, String> {
    let toks = lex(src)?;
    let mut p = Parser { toks: &toks, pos: 0 };
    // Skip leading directives like `/dts-v1/;`.
    p.skip_directives();
    // The root is `/ { … };`.
    let root = p.parse_node()?;
    Ok(root)
}

#[derive(Debug, PartialEq, Clone)]
enum Tok {
    LBrace,
    RBrace,
    Semi,
    Eq,
    Lt,
    Gt,
    Colon,
    Comma,
    Str(String),
    Word(String),
}

fn lex(src: &str) -> Result<Vec<Tok>, String> {
    let b = src.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        match c {
            _ if c.is_ascii_whitespace() => i += 1,
            b'/' if i + 1 < b.len() && b[i + 1] == b'/' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            b'{' => { out.push(Tok::LBrace); i += 1; }
            b'}' => { out.push(Tok::RBrace); i += 1; }
            b';' => { out.push(Tok::Semi); i += 1; }
            b'=' => { out.push(Tok::Eq); i += 1; }
            b'<' => { out.push(Tok::Lt); i += 1; }
            b'>' => { out.push(Tok::Gt); i += 1; }
            b':' => { out.push(Tok::Colon); i += 1; }
            b',' => { out.push(Tok::Comma); i += 1; }
            b'"' => {
                i += 1;
                let start = i;
                while i < b.len() && b[i] != b'"' {
                    i += 1;
                }
                if i >= b.len() {
                    return Err("unterminated string".into());
                }
                out.push(Tok::Str(src[start..i].to_string()));
                i += 1;
            }
            _ => {
                // A word: node/property name, number, phandle, or the `/` root.
                let start = i;
                while i < b.len() {
                    let d = b[i];
                    if d.is_ascii_whitespace() || matches!(d, b'{' | b'}' | b';' | b'=' | b'<' | b'>' | b':' | b',' | b'"') {
                        break;
                    }
                    i += 1;
                }
                out.push(Tok::Word(src[start..i].to_string()));
            }
        }
    }
    Ok(out)
}

struct Parser<'a> {
    toks: &'a [Tok],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }
    fn next(&mut self) -> Option<&Tok> {
        let t = self.toks.get(self.pos);
        if t.is_some() {
            self.pos += 1;
        }
        t
    }
    /// Skip `/dts-v1/;`, `/include/ …;`, `/plugin/;` and similar leading
    /// directives (a `Word` starting and ending with `/`, then `;`).
    fn skip_directives(&mut self) {
        while let Some(Tok::Word(w)) = self.peek() {
            if w.starts_with('/') && w.len() > 1 && w.ends_with('/') {
                self.pos += 1;
                if self.peek() == Some(&Tok::Semi) {
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
    }

    fn parse_node(&mut self) -> Result<DtsNode, String> {
        let mut node = DtsNode::default();
        // Optional `label:`.
        if let (Some(Tok::Word(w)), Some(Tok::Colon)) = (self.toks.get(self.pos), self.toks.get(self.pos + 1)) {
            node.label = Some(w.clone());
            self.pos += 2;
        }
        // Node name (with optional `@addr`).
        let name = match self.next() {
            Some(Tok::Word(w)) => w.clone(),
            other => return Err(format!("expected a node name, got {other:?}")),
        };
        if let Some((n, addr)) = split_unit_addr(&name) {
            node.name = n;
            node.unit_addr = addr;
        } else {
            node.name = name;
        }
        if self.next() != Some(&Tok::LBrace) {
            return Err(format!("expected `{{` after node `{}`", node.name));
        }
        self.parse_body(&mut node)?;
        Ok(node)
    }

    fn parse_body(&mut self, node: &mut DtsNode) -> Result<(), String> {
        loop {
            match self.peek() {
                Some(Tok::RBrace) => {
                    self.pos += 1;
                    // optional trailing `;`.
                    if self.peek() == Some(&Tok::Semi) {
                        self.pos += 1;
                    }
                    return Ok(());
                }
                None => return Err("unexpected EOF inside a node body".into()),
                _ => {
                    // Distinguish a child node from a property by looking past an
                    // optional `label:` and the name for `{` (node) vs `=`/`;`.
                    let mut look = self.pos;
                    if matches!(self.toks.get(look), Some(Tok::Word(_))) && self.toks.get(look + 1) == Some(&Tok::Colon) {
                        look += 2;
                    }
                    look += 1; // the name
                    match self.toks.get(look) {
                        Some(Tok::LBrace) => {
                            let child = self.parse_node()?;
                            node.children.push(child);
                        }
                        _ => self.parse_property(node)?,
                    }
                }
            }
        }
    }

    fn parse_property(&mut self, node: &mut DtsNode) -> Result<(), String> {
        let name = match self.next() {
            Some(Tok::Word(w)) => w.clone(),
            other => return Err(format!("expected a property name, got {other:?}")),
        };
        match self.peek() {
            Some(Tok::Semi) => {
                self.pos += 1;
                node.props.push((name, DtsValue::Bool));
            }
            Some(Tok::Eq) => {
                self.pos += 1;
                // A property can be a comma-separated list (e.g. `compatible =
                // "a", "b";` or concatenated cell blocks).  Keep the first value
                // (DTS lists `compatible` most-specific first) and skip the rest.
                let val = self.parse_value()?;
                while self.peek() == Some(&Tok::Comma) {
                    self.pos += 1;
                    let _ = self.parse_value()?;
                }
                if self.peek() == Some(&Tok::Semi) {
                    self.pos += 1;
                }
                node.props.push((name, val));
            }
            other => return Err(format!("expected `;` or `=` after property `{name}`, got {other:?}")),
        }
        Ok(())
    }

    fn parse_value(&mut self) -> Result<DtsValue, String> {
        match self.peek() {
            Some(Tok::Str(s)) => {
                let s = s.clone();
                self.pos += 1;
                Ok(DtsValue::Str(s))
            }
            Some(Tok::Lt) => {
                self.pos += 1;
                let mut cells = Vec::new();
                loop {
                    match self.next() {
                        Some(Tok::Gt) => break,
                        Some(Tok::Word(w)) => cells.push(parse_cell(w)),
                        Some(Tok::Comma) => {}
                        other => return Err(format!("expected a cell or `>`, got {other:?}")),
                    }
                }
                Ok(DtsValue::Cells(cells))
            }
            // A bare word value (e.g. a reference) — treat as a single string.
            Some(Tok::Word(w)) => {
                let w = w.clone();
                self.pos += 1;
                Ok(DtsValue::Str(w))
            }
            other => Err(format!("unsupported property value: {other:?}")),
        }
    }
}

/// `name@addr` → `(name, Some(addr))`; a plain name → `None`.
fn split_unit_addr(name: &str) -> Option<(String, Option<u64>)> {
    let at = name.find('@')?;
    let base = name[..at].to_string();
    let addr = u64::from_str_radix(name[at + 1..].trim_start_matches("0x"), 16).ok();
    Some((base, addr))
}

/// Parse a DTS cell (`0x…` hex, decimal, or a `&phandle`/expression → 0).
fn parse_cell(w: &str) -> u64 {
    if let Some(hex) = w.strip_prefix("0x").or_else(|| w.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).unwrap_or(0)
    } else {
        w.parse::<u64>().unwrap_or(0)
    }
}

// ─── Converter ──────────────────────────────────────────────────────────────

/// The result of importing a DTS: the emitted board skeleton and any
/// diagnostics (unmapped devices, missing facts) — never a silent drop (§8/D10).
#[derive(Debug, Clone)]
pub struct Import {
    pub board_si: String,
    pub diagnostics: Vec<String>,
}

/// The Silica device types known to the importer — the std-lib `device`
/// definitions.  Loading the names from the std lib (rather than hardcoding a
/// table) keeps the compiler *core* free of concrete peripheral names (§2, "no
/// privileged built-ins"): the `compatible`→type mapping is data-driven.
pub fn known_device_types() -> Vec<String> {
    match crate::load_std_items(&crate::default_std_dir()) {
        Ok(items) => items
            .iter()
            .filter_map(|it| match it {
                crate::ast::Item::Device(d) => Some(d.name.name.clone()),
                _ => None,
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Match a device node to a known Silica device type by its `compatible` (most
/// specific segment first) or node name — data-driven, no hardcoded table.
fn match_device(node: &DtsNode, known: &[String]) -> Option<String> {
    let mut candidates = Vec::new();
    if let Some(compat) = node.str_prop("compatible") {
        // "vendor,part" → the part after the last comma is the most specific.
        if let Some(part) = compat.rsplit(',').next() {
            candidates.push(ident(part));
        }
        candidates.push(ident(compat));
    }
    candidates.push(ident(&node.name));
    for c in candidates {
        if let Some(k) = known.iter().find(|k| k.eq_ignore_ascii_case(&c)) {
            return Some(k.clone());
        }
    }
    None
}

/// Sanitise a DTS string into a valid Silica identifier.
fn ident(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
        .collect();
    out = out.trim_matches('_').to_string();
    if out.is_empty() || out.as_bytes()[0].is_ascii_digit() {
        out.insert(0, '_');
    }
    out
}

/// Give a memory region a Silica name from its base address (nRF-style
/// heuristic): flash at 0, ram in the SRAM window, else `mem_<addr>`.
fn region_name(addr: u64) -> String {
    match addr {
        0 => "flash".into(),
        0x2000_0000..=0x2FFF_FFFF => "ram".into(),
        _ => format!("mem_{addr:08x}"),
    }
}

fn fmt_size(bytes: u64) -> String {
    if bytes != 0 && bytes % (1024 * 1024) == 0 {
        format!("{}M", bytes / (1024 * 1024))
    } else if bytes != 0 && bytes % 1024 == 0 {
        format!("{}K", bytes / 1024)
    } else {
        format!("{bytes}")
    }
}

/// Import a parsed DTS root into a Silica board skeleton (§8, P7-8a).  `known`
/// is the set of Silica device types a `compatible` may map to (see
/// [`known_device_types`]); an unmatched device becomes a diagnosed stub.
pub fn to_silica(root: &DtsNode, known: &[String]) -> Import {
    let mut diagnostics = Vec::new();
    let board = root.str_prop("model").map(ident).unwrap_or_else(|| "imported_board".into());
    let soc = root.child("soc");
    let soc_name = soc.map(|s| ident(&s.name)).unwrap_or_else(|| "soc".into());
    let scope = soc.unwrap_or(root);

    // Memory regions from `memory` nodes with a `reg`.
    let mut regions = Vec::new();
    for n in &scope.children {
        if n.is_memory() {
            if let Some((addr, size)) = n.reg().or_else(|| n.unit_addr.map(|a| (a, 0))) {
                regions.push((region_name(addr), addr, size));
            } else {
                diagnostics.push(format!("memory node `{}` has no `reg` — skipped", n.name));
            }
        }
    }
    if regions.is_empty() {
        diagnostics.push("no memory regions found (`memory` node with `reg`) — the soc has an empty memory block".into());
    }

    // Device instances from non-memory nodes with a `reg`.
    let mut instances = Vec::new();
    for n in &scope.children {
        if n.is_memory() {
            continue;
        }
        let Some((addr, _)) = n.reg() else { continue };
        let inst = n.label.clone().unwrap_or_else(|| ident(&n.name));
        let compat = n.str_prop("compatible").unwrap_or("");
        match match_device(n, known) {
            Some(ty) => instances.push(format!("  {} : {} at 0x{:08x}", ident(&inst), ty, addr)),
            None => {
                diagnostics.push(format!(
                    "device `{}` (compatible \"{}\") has no Silica device type — emitted as a commented `raw` stub",
                    n.name, compat
                ));
                instances.push(format!(
                    "  // TODO(raw stub): {} at 0x{:08x} — compatible \"{}\" (no Silica device type yet)",
                    ident(&inst), addr, compat
                ));
            }
        }
    }

    // Emit the skeleton.
    let mut s = String::new();
    s.push_str(&format!("// Imported from DTS by silicac dts_import (§8, P7-8a).\n"));
    s.push_str(&format!("board {board} {{\n"));
    s.push_str(&format!("  soc {soc_name} {{\n"));
    s.push_str("    memory {\n");
    for (name, addr, size) in &regions {
        s.push_str(&format!("      {:<6}: region at 0x{:08x} size {}\n", name, addr, fmt_size(*size)));
    }
    s.push_str("    }\n");
    s.push_str("    clocks {\n");
    s.push_str("      sysclk : clock_source = 64MHz  // TODO: import the real clock topology (P7-8b)\n");
    s.push_str("    }\n");
    s.push_str("  }\n");
    if !instances.is_empty() {
        s.push('\n');
        for inst in &instances {
            s.push_str(inst);
            s.push('\n');
        }
    }
    s.push_str("}\n");

    Import { board_si: s, diagnostics }
}
