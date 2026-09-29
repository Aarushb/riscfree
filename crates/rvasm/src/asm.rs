//! Parsing and the two passes: collect/expand, then resolve/encode.

use crate::encode::{self, Format, InstructionInfo, OpKind, Range};
use crate::lexer::{lex_line, Tok, Token};
use crate::{AsmConfig, AsmResult, DataImage, Diagnostic, InputFile, Program, SourcePos, Statement, Symbol, SymbolTable};
use std::collections::{BTreeMap, HashSet};
use std::iter::Peekable;
use std::sync::Arc;

/// Maximum nesting of macro expansions; deeper chains report E-MACRO-DEPTH
/// instead of exhausting the stack.
const MACRO_DEPTH_LIMIT: u32 = 32;

#[derive(Debug, Clone)]
enum Operand {
    Reg(u8),
    Imm(i64),
    /// Label reference, resolved to an absolute address in pass 2.
    Sym(String),
    /// Relocations used by expansions; resolved in pass 2 against the symbol
    /// (and the instruction address for pc-relative forms). `HiPcRel` sits on
    /// the `auipc` itself; `LoPcRel` records the `auipc`'s address because
    /// %pcrel_lo is always relative to that instruction, not the `jalr`.
    HiSym(String),
    LoSym(String),
    HiPcRel(String),
    LoPcRel(String, u32),
    /// `offset(base)` for loads and stores.
    Mem { off: i64, base: u8 },
    /// Label-form load/store relocated through `at`: the `lw rd, sym` RARS
    /// pseudo-form. Encodes as `%lo(sym)(base)`.
    MemLo { sym: String, base: u8 },
}

#[derive(Debug, Clone)]
struct RawInstr {
    name: &'static str,
    ops: Vec<Operand>,
    addr: u32,
    source: SourcePos,
    expanded_from: Option<SourcePos>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Segment {
    Text,
    Data,
}

/// A `.macro` definition: parameters carry their leading `%`, and the body
/// is kept as raw token lines, substituted at call time.
#[derive(Debug, Clone)]
struct MacroDef {
    params: Vec<String>,
    body: Vec<Vec<Token>>,
}

struct Assembler {
    cfg: AsmConfig,
    diags: Vec<Diagnostic>,
    sources: Vec<Arc<str>>,
    file_names: Vec<String>,
    symbols: SymbolTable,
    globals: HashSet<String>,
    raw: Vec<RawInstr>,
    data: Vec<u8>,
    data_relocs: Vec<(u32, String, SourcePos)>, // (offset in data, label, pos)
    text_addr: u32,
    data_addr: u32,
    segment: Segment,
    /// `.eqv` name to replacement tokens.
    equates: BTreeMap<String, Vec<Tok>>,
    /// `.macro` name to definition, collected during pass one.
    macros: BTreeMap<String, MacroDef>,
    /// Current macro expansion nesting.
    macro_depth: u32,
    /// Set when the depth limit aborts an expansion so enclosing frames
    /// stop emitting instead of piling on duplicate errors.
    macro_abort: bool,
}

pub fn assemble_impl(files: &[InputFile], cfg: &AsmConfig) -> AsmResult {
    let mut a = Assembler {
        cfg: cfg.clone(),
        diags: Vec::new(),
        sources: Vec::new(),
        file_names: Vec::new(),
        symbols: SymbolTable::default(),
        globals: HashSet::new(),
        raw: Vec::new(),
        data: Vec::new(),
        data_relocs: Vec::new(),
        text_addr: cfg.text_base,
        data_addr: cfg.data_base,
        segment: Segment::Text,
        equates: BTreeMap::new(),
        macros: BTreeMap::new(),
        macro_depth: 0,
        macro_abort: false,
    };

    for (file_id, f) in files.iter().enumerate() {
        a.sources.push(Arc::from(f.source.as_str()));
        a.file_names.push(f.name.clone());
        a.pass_one(f, file_id);
    }
    let program = a.pass_two();
    AsmResult { program: Some(program), diagnostics: a.diags }
}

impl Assembler {
    fn err(&mut self, code: &'static str, msg: impl Into<String>, pos: SourcePos) {
        self.diags.push(Diagnostic::error(code, msg, pos));
    }

    fn cur_addr(&self) -> u32 {
        match self.segment {
            Segment::Text => self.text_addr,
            Segment::Data => self.data_addr,
        }
    }

    fn advance(&mut self, n: u32) {
        match self.segment {
            Segment::Text => self.text_addr += n,
            Segment::Data => self.data_addr += n,
        }
    }

    /// Keep `data` contiguous with `data_addr`: pad with zeros over any hole
    /// (created by `.data <addr>` jumps) before the next append.
    fn data_pad_to_cursor(&mut self) {
        let cursor = (self.data_addr - self.cfg.data_base) as usize;
        if cursor > self.data.len() {
            self.data.resize(cursor, 0);
        }
    }

    fn pass_one(&mut self, file: &InputFile, file_id: usize) {
        // Peekable so `.macro` can swallow whole raw lines up to
        // `.end_macro` before normal per-line parsing resumes.
        let mut lines = file.source.lines().enumerate().peekable();
        while let Some((line_idx, line)) = lines.next() {
            let pos = SourcePos { file: file_id, line: line_idx as u32 + 1, col: 0 };
            let toks = lex_line(line, pos, &mut self.diags);
            if matches!(toks.first().map(|t| &t.tok), Some(Tok::Ident(d)) if d == ".macro") {
                self.collect_macro(&mut lines, toks);
                continue;
            }
            let mut toks = toks;
            self.substitute_equates(&mut toks);
            if toks.is_empty() {
                continue;
            }
            self.parse_line(toks);
        }
    }

    /// Gather a `.macro` definition, consuming raw source lines up to the
    /// matching `.end_macro`. Body lines are lexed but otherwise untouched:
    /// parameters are substituted when the macro is called. Nested `.macro`
    /// definitions are rejected, and everything up to the outer `.end_macro`
    /// is skipped so the rest of the file still assembles.
    fn collect_macro<'a, I>(&mut self, lines: &mut Peekable<I>, header: Vec<Token>)
    where
        I: Iterator<Item = (usize, &'a str)>,
    {
        let pos = header[0].pos;
        let parsed = self.macro_header(&header[1..], pos);
        let mut body: Vec<Vec<Token>> = Vec::new();
        let mut nested = 0u32;
        let mut discard = parsed.is_none();
        loop {
            let Some((line_idx, line)) = lines.next() else {
                self.err("E-DIRECTIVE", "'.macro' is missing its .end_macro", pos);
                return;
            };
            let lpos = SourcePos { file: pos.file, line: line_idx as u32 + 1, col: 0 };
            let toks = lex_line(line, lpos, &mut self.diags);
            match toks.first().map(|t| &t.tok) {
                Some(Tok::Ident(d)) if d == ".macro" => {
                    if nested == 0 {
                        discard = true;
                        self.err("E-UNSUPPORTED", "nested macros are not supported", lpos);
                    }
                    nested += 1;
                }
                Some(Tok::Ident(d)) if d == ".end_macro" => {
                    if nested == 0 {
                        break;
                    }
                    nested -= 1;
                }
                _ => {}
            }
            if !discard {
                body.push(toks);
            }
        }
        if let Some((name, params)) = parsed {
            self.macros.insert(name, MacroDef { params, body });
        }
    }

    /// Parse what follows `.macro` on the header line: a name and a
    /// comma-separated parameter list, inside optional parentheses.
    fn macro_header(&mut self, rest: &[Token], pos: SourcePos) -> Option<(String, Vec<String>)> {
        let Some(Token { tok: Tok::Ident(name), .. }) = rest.first() else {
            self.err("E-DIRECTIVE", ".macro needs a name", pos);
            return None;
        };
        let name = name.clone();
        let mut params = Vec::new();
        let mut j = 1usize;
        if rest.get(j).map(|t| &t.tok) == Some(&Tok::LParen) {
            j += 1;
            loop {
                match rest.get(j).map(|t| &t.tok) {
                    Some(Tok::Ident(p)) if p.starts_with('%') => {
                        params.push(p.clone());
                        j += 1;
                        match rest.get(j).map(|t| &t.tok) {
                            Some(Tok::Comma) => j += 1,
                            Some(Tok::RParen) => {
                                j += 1;
                                break;
                            }
                            _ => {
                                self.err("E-DIRECTIVE", "expected ',' or ')' in the .macro parameter list", pos);
                                return None;
                            }
                        }
                    }
                    Some(Tok::RParen) => {
                        j += 1;
                        break;
                    }
                    _ => {
                        self.err("E-DIRECTIVE", "macro parameters start with '%'", pos);
                        return None;
                    }
                }
            }
        } else {
            while j < rest.len() {
                match &rest[j].tok {
                    Tok::Ident(p) if p.starts_with('%') => params.push(p.clone()),
                    Tok::Comma => {}
                    _ => {
                        self.err("E-DIRECTIVE", "macro parameters start with '%'", pos);
                        return None;
                    }
                }
                j += 1;
            }
        }
        if j < rest.len() {
            self.err("E-DIRECTIVE", "unexpected tokens after the .macro parameter list", pos);
            return None;
        }
        Some((name, params))
    }

    /// Split a macro call's `(arg1, arg2)` into top-level comma-separated
    /// token sequences. Nested parentheses stay inside their argument.
    fn parse_macro_args(&mut self, toks: &[Token], name: &str, pos: SourcePos) -> Vec<Vec<Token>> {
        let mut args: Vec<Vec<Token>> = vec![Vec::new()];
        let mut depth = 1usize;
        for (k, t) in toks.iter().enumerate() {
            match t.tok {
                Tok::LParen => {
                    depth += 1;
                    args.last_mut().expect("always at least one arg").push(t.clone());
                }
                Tok::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        if k + 1 != toks.len() {
                            self.err("E-SYNTAX", "unexpected tokens after the macro call", toks[k + 1].pos);
                        }
                        // `name()` is a zero-argument call, not one empty one.
                        if args.len() == 1 && args[0].is_empty() {
                            args.clear();
                        }
                        return args;
                    }
                    args.last_mut().expect("always at least one arg").push(t.clone());
                }
                Tok::Comma if depth == 1 => args.push(Vec::new()),
                _ => args.last_mut().expect("always at least one arg").push(t.clone()),
            }
        }
        self.err("E-SYNTAX", format!("macro call '{name}' is missing ')'"), pos);
        args
    }

    /// Expand a call to a defined macro: substitute each `%param` token
    /// with its argument's tokens, then run the body lines through the
    /// normal line processing so labels, directives, and instructions all
    /// work, including calls to other macros.
    fn expand_macro(&mut self, name: &str, args: Vec<Vec<Token>>, pos: SourcePos) {
        if self.macro_depth >= MACRO_DEPTH_LIMIT {
            self.err(
                "E-MACRO-DEPTH",
                format!("macro expansion of '{name}' nested deeper than {MACRO_DEPTH_LIMIT}"),
                pos,
            );
            self.macro_abort = true;
            return;
        }
        let Some(def) = self.macros.get(name).cloned() else { return };
        if args.len() != def.params.len() {
            self.err(
                "E-OPERAND",
                format!("macro '{name}' expects {} parameter(s), found {}", def.params.len(), args.len()),
                pos,
            );
            return;
        }
        self.macro_depth += 1;
        for line in &def.body {
            let mut toks = substitute_params(line, &def.params, &args);
            self.substitute_equates(&mut toks);
            if !toks.is_empty() {
                self.parse_line(toks);
            }
            if self.macro_abort {
                break;
            }
        }
        self.macro_depth -= 1;
        if self.macro_depth == 0 {
            self.macro_abort = false;
        }
    }

    /// Replace `.eqv`-defined identifiers with their token lists.
    fn substitute_equates(&mut self, toks: &mut Vec<Token>) {
        if self.equates.is_empty() || toks.is_empty() {
            return;
        }
        let mut out = Vec::with_capacity(toks.len());
        for t in toks.drain(..) {
            match &t.tok {
                Tok::Ident(name) if name != ".eqv" => match self.equates.get(name) {
                    Some(exp) => {
                        for e in exp.clone() {
                            out.push(Token { tok: e, pos: t.pos });
                        }
                    }
                    None => out.push(t),
                },
                _ => out.push(t),
            }
        }
        *toks = out;
    }

    fn parse_line(&mut self, toks: Vec<Token>) {
        let mut i = 0usize;
        // Labels.
        while i + 1 < toks.len() && matches!(&toks[i].tok, Tok::Ident(_)) && toks[i + 1].tok == Tok::Colon {
            if let Tok::Ident(name) = &toks[i].tok {
                let name = name.clone();
                let addr = self.cur_addr();
                let global = self.globals.contains(&name);
                self.symbols.define(
                    name.clone(),
                    Symbol { addr, global, source: toks[i].pos },
                    &mut self.diags,
                );
            }
            i += 2;
        }
        let Some(t) = toks.get(i) else { return };

        match &t.tok {
            Tok::Ident(d) if d.starts_with('.') => self.directive(d, &toks[i + 1..], t.pos),
            Tok::Ident(mnemonic) => {
                // `name(...)` for a defined macro expands; unknown names
                // keep the ordinary instruction path and its E-MNEMONIC.
                if matches!(toks.get(i + 1).map(|t| &t.tok), Some(Tok::LParen))
                    && self.macros.contains_key(mnemonic)
                {
                    let args = self.parse_macro_args(&toks[i + 2..], mnemonic, t.pos);
                    self.expand_macro(mnemonic, args, t.pos);
                } else {
                    let ops = self.parse_operands(&toks[i + 1..]);
                    self.instruction(mnemonic, ops, t.pos);
                }
            }
            _ => self.err("E-SYNTAX", "expected an instruction or directive", t.pos),
        }
    }

    fn parse_operands(&mut self, toks: &[Token]) -> Vec<Operand> {
        let mut ops = Vec::new();
        let mut i = 0usize;
        while i < toks.len() {
            let (op, next) = self.parse_operand(toks, i);
            ops.push(op);
            i = next;
            if i < toks.len() {
                if toks[i].tok == Tok::Comma {
                    i += 1;
                } else {
                    self.err("E-SYNTAX", "expected ',' between operands", toks[i].pos);
                    break;
                }
            }
        }
        ops
    }

    fn parse_operand(&mut self, toks: &[Token], i: usize) -> (Operand, usize) {
        let t = &toks[i];
        match &t.tok {
            Tok::Int(v) => {
                if let Some(off_base) = self.try_mem(toks, i) {
                    return (Operand::Mem { off: *v, base: off_base }, i + 4);
                }
                (Operand::Imm(*v), i + 1)
            }
            Tok::LParen => match self.try_mem_bare(toks, i) {
                Some((base, next)) => (Operand::Mem { off: 0, base }, next),
                None => {
                    self.err("E-OPERAND", "expected offset(base) or (base)", t.pos);
                    (Operand::Imm(0), i + 1)
                }
            },
            Tok::Ident(name) => match reg_by_name(name) {
                Some(r) => (Operand::Reg(r), i + 1),
                None => (Operand::Sym(name.clone()), i + 1),
            },
            _ => {
                self.err("E-OPERAND", "expected a register, immediate, label, or offset(base)", t.pos);
                (Operand::Imm(0), i + 1)
            }
        }
    }

    /// Recognize `Int ( base )` starting at `i`; returns the base register.
    /// Indices are into the operand slice the caller passes (mnemonic
    /// already stripped).
    fn try_mem(&mut self, toks: &[Token], i: usize) -> Option<u8> {
        let lparen = toks.get(i + 1)?;
        if lparen.tok != Tok::LParen {
            return None;
        }
        self.mem_base_at(toks, i + 2, i + 3)
    }

    /// Recognize `( base )` starting at the `(`; returns (base, next index).
    fn try_mem_bare(&mut self, toks: &[Token], i: usize) -> Option<(u8, usize)> {
        let base = self.mem_base_at(toks, i + 1, i + 3)?;
        Some((base, i + 3))
    }

    /// Shared tail of the two mem forms: base register at `base_idx`,
    /// `)` expected at `rparen_idx`.
    fn mem_base_at(&mut self, toks: &[Token], base_idx: usize, rparen_idx: usize) -> Option<u8> {
        let base_tok = toks.get(base_idx)?;
        let rparen = toks.get(rparen_idx)?;
        if rparen.tok != Tok::RParen {
            return None;
        }
        let Tok::Ident(base_name) = &base_tok.tok else {
            self.err("E-OPERAND", "expected a base register inside offset(base)", base_tok.pos);
            return None;
        };
        match reg_by_name(base_name) {
            Some(r) => Some(r),
            None => {
                self.err(
                    "E-OPERAND",
                    format!("'{base_name}' is not a register (in offset(base))"),
                    base_tok.pos,
                );
                None
            }
        }
    }

    fn directive(&mut self, d: &str, rest: &[Token], pos: SourcePos) {
        match d {
            ".text" | ".data" => {
                self.segment = if d == ".text" { Segment::Text } else { Segment::Data };
                if let Some(Token { tok: Tok::Int(addr), .. }) = rest.first() {
                    match self.segment {
                        Segment::Text => self.text_addr = *addr as u32,
                        Segment::Data => self.data_addr = *addr as u32,
                    }
                }
            }
            ".align" => {
                let Some(Token { tok: Tok::Int(n), .. }) = rest.first() else {
                    self.err("E-DIRECTIVE", ".align needs an alignment value", pos);
                    return;
                };
                let align = 1u32 << (*n).clamp(0, 20);
                let rem = self.cur_addr() % align;
                if rem != 0 {
                    let pad = align - rem;
                    if self.segment == Segment::Data {
                        self.data_pad_to_cursor();
                        for _ in 0..pad {
                            self.data.push(0);
                        }
                    }
                    self.advance(pad);
                }
            }
            ".space" => {
                let Some(Token { tok: Tok::Int(n), .. }) = rest.first() else {
                    self.err("E-DIRECTIVE", ".space needs a byte count", pos);
                    return;
                };
                if self.segment == Segment::Data {
                    self.data_pad_to_cursor();
                    for _ in 0..*n {
                        self.data.push(0);
                    }
                }
                self.advance(*n as u32);
            }
            ".word" | ".half" | ".byte" => {
                if self.segment != Segment::Data {
                    self.err("E-DIRECTIVE", format!("{d} must appear inside .data"), pos);
                }
                let width = match d {
                    ".word" => 4usize,
                    ".half" => 2,
                    _ => 1,
                };
                let mut j = 0usize;
                while j < rest.len() {
                    match &rest[j].tok {
                        Tok::Int(v) => {
                            self.push_data(&int_le(*v, width));
                            self.advance(width as u32);
                        }
                        Tok::Ident(label) => {
                            let off = self.data_addr - self.cfg.data_base;
                            self.data_relocs.push((off, label.clone(), rest[j].pos));
                            self.push_data(&[0; 8][..width]);
                            self.advance(width as u32);
                        }
                        Tok::Comma => {}
                        _ => self.err("E-DIRECTIVE", format!("{d} expects integers or labels"), rest[j].pos),
                    }
                    j += 1;
                }
            }
            ".ascii" | ".asciz" | ".string" => {
                if self.segment != Segment::Data {
                    self.err("E-DIRECTIVE", format!("{d} must appear inside .data"), pos);
                }
                for t in rest {
                    if let Tok::Str(s) = &t.tok {
                        let mut bytes = s.clone().into_bytes();
                        if d != ".ascii" {
                            bytes.push(0);
                        }
                        self.push_data(&bytes);
                        self.advance(bytes.len() as u32);
                    }
                }
            }
            ".eqv" => {
                if rest.is_empty() || !matches!(&rest[0].tok, Tok::Ident(_)) {
                    self.err("E-DIRECTIVE", ".eqv needs a name", pos);
                    return;
                }
                let Tok::Ident(name) = &rest[0].tok else { unreachable!() };
                let expansion: Vec<Tok> = rest[1..].iter().map(|t| t.tok.clone()).collect();
                self.equates.insert(name.clone(), expansion);
            }
            ".globl" | ".global" => {
                for t in rest {
                    if let Tok::Ident(name) = &t.tok {
                        self.globals.insert(name.clone());
                        if let Some(sym) = self.symbols.get_mut(name) {
                            sym.global = true;
                        }
                    }
                }
            }
            // gcc compat: accepted and ignored, layout unchanged, matching
            // how RARS treats .section.
            ".section" => {}
            ".float" | ".double" => {
                self.err("E-UNSUPPORTED", format!("{d} lands with floating point support (phase 2)"), pos)
            }
            // Definitions are consumed directly by pass_one; seeing either
            // directive here means it was misplaced.
            ".macro" => self.err("E-DIRECTIVE", "'.macro' must be the first token on its line", pos),
            ".end_macro" => self.err("E-DIRECTIVE", "'.end_macro' without a matching .macro", pos),
            ".include" => self.err("E-UNSUPPORTED", ".include lands in phase 1", pos),
            ".extern" => self.err("E-UNSUPPORTED", ".extern lands in phase 1", pos),
            other => self.err("E-DIRECTIVE", format!("unknown directive '{other}'"), pos),
        }
    }

    fn push_data(&mut self, bytes: &[u8]) {
        self.data_pad_to_cursor();
        self.data.extend_from_slice(bytes);
    }

    fn instruction(&mut self, mnemonic: &str, mut ops: Vec<Operand>, pos: SourcePos) {
        if self.segment != Segment::Text {
            self.err("E-SEGMENT", "instructions must appear inside .text", pos);
        }
        if let Some(info) = encode::lookup(mnemonic) {
            // RARS label-form loads/stores: `lw rd, sym` / `sw rt, sym`.
            let mem_sym = matches!(ops.last(), Some(Operand::Sym(_)))
                && matches!(info.format, Format::I | Format::S)
                && (info.opcode == encode::LOAD || info.opcode == encode::STORE)
                && self.cfg.allow_pseudo;
            if mem_sym {
                let Operand::Sym(sym) = ops.remove(1) else { unreachable!() };
                let rd = match ops.first() {
                    Some(Operand::Reg(r)) => *r,
                    _ => 0,
                };
                self.push_basic("lui", vec![Operand::Reg(1), Operand::HiSym(sym.clone())], pos, pos);
                self.push_basic(
                    info.name,
                    vec![Operand::Reg(rd), Operand::MemLo { sym, base: 1 }],
                    pos,
                    pos,
                );
                return;
            }
            self.raw.push(RawInstr {
                name: info.name,
                ops,
                addr: self.text_addr,
                source: pos,
                expanded_from: None,
            });
            self.advance(4);
            return;
        }
        if !self.cfg.allow_pseudo {
            self.err("E-PSEUDO", format!("pseudo-instructions are disabled ('{mnemonic}')"), pos);
            self.advance(4);
            return;
        }
        self.expand_pseudo(mnemonic, ops, pos);
    }

    fn push_basic(&mut self, name: &'static str, ops: Vec<Operand>, from: SourcePos, pos: SourcePos) {
        self.raw.push(RawInstr { name, ops, addr: self.text_addr, source: pos, expanded_from: Some(from) });
        self.advance(4);
    }

    /// Expand one pseudo-instruction into basic instructions at the current
    /// address. Expansion sizes are decided here so addresses stay exact;
    /// reversed-operand branches use `at` (x1) exactly as RARS does.
    fn expand_pseudo(&mut self, mnemonic: &str, ops: Vec<Operand>, pos: SourcePos) {
        let imm = |v: i64| Operand::Imm(v);
        let r = |n: u8| Operand::Reg(n);
        match (mnemonic, ops.as_slice()) {
            ("li", [Operand::Reg(rd), Operand::Imm(v)]) => {
                let rd = *rd;
                if (-2048..=2047).contains(v) {
                    self.push_basic("addi", vec![r(rd), r(0), imm(*v)], pos, pos);
                } else {
                    let (hi, lo) = hi_lo(*v);
                    self.push_basic("lui", vec![r(rd), imm(hi & 0xfffff)], pos, pos);
                    self.push_basic("addi", vec![r(rd), r(rd), imm(lo)], pos, pos);
                }
            }
            ("la", [Operand::Reg(rd), Operand::Sym(s)]) => {
                let (rd, s) = (*rd, s.clone());
                self.push_basic("lui", vec![r(rd), Operand::HiSym(s.clone())], pos, pos);
                self.push_basic("addi", vec![r(rd), r(rd), Operand::LoSym(s)], pos, pos);
            }
            ("mv", [Operand::Reg(rd), Operand::Reg(rs)]) => {
                let (rd, rs) = (*rd, *rs);
                self.push_basic("addi", vec![r(rd), r(rs), imm(0)], pos, pos);
            }
            ("nop", []) => self.push_basic("addi", vec![r(0), r(0), imm(0)], pos, pos),
            ("not", [Operand::Reg(rd), Operand::Reg(rs)]) => {
                let (rd, rs) = (*rd, *rs);
                self.push_basic("xori", vec![r(rd), r(rs), imm(-1)], pos, pos);
            }
            ("neg", [Operand::Reg(rd), Operand::Reg(rs)]) => {
                let (rd, rs) = (*rd, *rs);
                self.push_basic("sub", vec![r(rd), r(0), r(rs)], pos, pos);
            }
            ("seqz", [Operand::Reg(rd), Operand::Reg(rs)]) => {
                let (rd, rs) = (*rd, *rs);
                self.push_basic("sltiu", vec![r(rd), r(rs), imm(1)], pos, pos);
            }
            ("snez", [Operand::Reg(rd), Operand::Reg(rs)]) => {
                let (rd, rs) = (*rd, *rs);
                self.push_basic("sltu", vec![r(rd), r(0), r(rs)], pos, pos);
            }
            ("sltz", [Operand::Reg(rd), Operand::Reg(rs)]) => {
                let (rd, rs) = (*rd, *rs);
                self.push_basic("slt", vec![r(rd), r(rs), r(0)], pos, pos);
            }
            ("sgtz", [Operand::Reg(rd), Operand::Reg(rs)]) => {
                let (rd, rs) = (*rd, *rs);
                self.push_basic("slt", vec![r(rd), r(0), r(rs)], pos, pos);
            }
            ("sgt", [Operand::Reg(rd), Operand::Reg(rs), Operand::Reg(rt)]) => {
                let (rd, rs, rt) = (*rd, *rs, *rt);
                self.push_basic("slt", vec![r(rd), r(rt), r(rs)], pos, pos);
            }
            ("sgtu", [Operand::Reg(rd), Operand::Reg(rs), Operand::Reg(rt)]) => {
                let (rd, rs, rt) = (*rd, *rs, *rt);
                self.push_basic("sltu", vec![r(rd), r(rt), r(rs)], pos, pos);
            }
            ("beqz", [Operand::Reg(rs), Operand::Sym(l)]) => {
                let (rs, l) = (*rs, l.clone());
                self.push_basic("beq", vec![r(rs), r(0), Operand::Sym(l)], pos, pos);
            }
            ("bnez", [Operand::Reg(rs), Operand::Sym(l)]) => {
                let (rs, l) = (*rs, l.clone());
                self.push_basic("bne", vec![r(rs), r(0), Operand::Sym(l)], pos, pos);
            }
            ("blez", [Operand::Reg(rs), Operand::Sym(l)]) => {
                let (rs, l) = (*rs, l.clone());
                self.push_basic("bge", vec![r(0), r(rs), Operand::Sym(l)], pos, pos);
            }
            ("bgez", [Operand::Reg(rs), Operand::Sym(l)]) => {
                let (rs, l) = (*rs, l.clone());
                self.push_basic("bge", vec![r(rs), r(0), Operand::Sym(l)], pos, pos);
            }
            ("bltz", [Operand::Reg(rs), Operand::Sym(l)]) => {
                let (rs, l) = (*rs, l.clone());
                self.push_basic("blt", vec![r(rs), r(0), Operand::Sym(l)], pos, pos);
            }
            ("bgtz", [Operand::Reg(rs), Operand::Sym(l)]) => {
                let (rs, l) = (*rs, l.clone());
                self.push_basic("blt", vec![r(0), r(rs), Operand::Sym(l)], pos, pos);
            }
            // Reversed-operand forms: slt into `at` then compare against zero.
            ("bgt", [Operand::Reg(rs), Operand::Reg(rt), Operand::Sym(l)]) => {
                let (rs, rt, l) = (*rs, *rt, l.clone());
                self.push_basic("slt", vec![r(1), r(rt), r(rs)], pos, pos);
                self.push_basic("bne", vec![r(1), r(0), Operand::Sym(l)], pos, pos);
            }
            ("ble", [Operand::Reg(rs), Operand::Reg(rt), Operand::Sym(l)]) => {
                let (rs, rt, l) = (*rs, *rt, l.clone());
                self.push_basic("slt", vec![r(1), r(rt), r(rs)], pos, pos);
                self.push_basic("beq", vec![r(1), r(0), Operand::Sym(l)], pos, pos);
            }
            ("bgtu", [Operand::Reg(rs), Operand::Reg(rt), Operand::Sym(l)]) => {
                let (rs, rt, l) = (*rs, *rt, l.clone());
                self.push_basic("sltu", vec![r(1), r(rt), r(rs)], pos, pos);
                self.push_basic("bne", vec![r(1), r(0), Operand::Sym(l)], pos, pos);
            }
            ("bleu", [Operand::Reg(rs), Operand::Reg(rt), Operand::Sym(l)]) => {
                let (rs, rt, l) = (*rs, *rt, l.clone());
                self.push_basic("sltu", vec![r(1), r(rt), r(rs)], pos, pos);
                self.push_basic("beq", vec![r(1), r(0), Operand::Sym(l)], pos, pos);
            }
            ("j", [Operand::Sym(l)]) => {
                let l = l.clone();
                self.push_basic("jal", vec![r(0), Operand::Sym(l)], pos, pos);
            }
            ("jr", [Operand::Reg(rs)]) => {
                let rs = *rs;
                self.push_basic("jalr", vec![r(0), r(rs), imm(0)], pos, pos);
            }
            ("jal", [Operand::Reg(rs)]) => {
                let rs = *rs;
                self.push_basic("jalr", vec![r(1), r(rs), imm(0)], pos, pos);
            }
            ("ret", []) => self.push_basic("jalr", vec![r(0), r(1), imm(0)], pos, pos),
            ("call", [Operand::Sym(l)]) => {
                let l = l.clone();
                let auipc_addr = self.text_addr;
                self.push_basic("auipc", vec![r(1), Operand::HiPcRel(l.clone())], pos, pos);
                self.push_basic("jalr", vec![r(1), r(1), Operand::LoPcRel(l, auipc_addr)], pos, pos);
            }
            ("tail", [Operand::Sym(l)]) => {
                let l = l.clone();
                let auipc_addr = self.text_addr;
                self.push_basic("auipc", vec![r(6), Operand::HiPcRel(l.clone())], pos, pos);
                self.push_basic("jalr", vec![r(0), r(6), Operand::LoPcRel(l, auipc_addr)], pos, pos);
            }
            (name, _) => {
                self.err(
                    "E-MNEMONIC",
                    format!("unknown instruction '{name}' (operand forms may not match)"),
                    pos,
                );
                self.advance(4);
            }
        }
    }

    fn pass_two(&mut self) -> Program {
        let raw = std::mem::take(&mut self.raw);
        let mut statements = Vec::with_capacity(raw.len());
        for instr in &raw {
            let Some(info) = encode::lookup(instr.name) else {
                let (source, name) = (instr.source, instr.name);
                self.err("E-MNEMONIC", format!("unknown instruction '{name}'"), source);
                continue;
            };
            match self.encode_one(info, instr) {
                Ok((word, text)) => statements.push(Statement {
                    addr: instr.addr,
                    encoding: word,
                    expanded_from: instr.expanded_from,
                    source: instr.source,
                    basic_text: Arc::from(text.as_str()),
                }),
                Err((code, msg, pos)) => self.err(code, msg, pos),
            }
        }

        // Patch `.word label` relocations now that all symbols exist.
        let relocs = std::mem::take(&mut self.data_relocs);
        for (off, label, pos) in &relocs {
            match self.symbols.get(label) {
                Some(sym) => {
                    let b = sym.addr.to_le_bytes();
                    let start = *off as usize;
                    for (k, byte) in b.iter().enumerate() {
                        if start + k < self.data.len() {
                            self.data[start + k] = *byte;
                        }
                    }
                }
                None => self.err("E-UNDEF", format!("undefined symbol '{label}'"), *pos),
            }
        }

        Program {
            text_base: self.cfg.text_base,
            statements,
            data: DataImage { base: self.cfg.data_base, bytes: std::mem::take(&mut self.data) },
            symbols: std::mem::take(&mut self.symbols),
            sources: std::mem::take(&mut self.sources),
            file_names: std::mem::take(&mut self.file_names),
        }
    }

    fn encode_one(
        &self,
        info: &InstructionInfo,
        instr: &RawInstr,
    ) -> Result<(u32, String), (&'static str, String, SourcePos)> {
        let e = |code: &'static str, msg: String, pos: SourcePos| Err((code, msg, pos));
        let bad_op = |k: usize| {
            (
                "E-OPERAND",
                format!("operand {} of '{}' has the wrong kind", k + 1, info.name),
                instr.source,
            )
        };

        // Loads and stores arrive as `[reg, Mem]` or `[reg, MemLo]`;
        // everything else is the flat register/immediate list its format
        // declares.
        enum MemSpec {
            Off { off: i64, base: u8 },
            Lo { sym: String, base: u8 },
        }
        let mem = match instr.ops.last() {
            Some(Operand::Mem { off, base }) => Some(MemSpec::Off { off: *off, base: *base }),
            Some(Operand::MemLo { sym, base }) => Some(MemSpec::Lo { sym: sym.clone(), base: *base }),
            _ => None,
        };
        if let Some(spec) = mem {
            if instr.ops.len() != 2 {
                return e(
                    "E-OPERAND",
                    format!("'{}' takes a register and an offset(base) operand", info.name),
                    instr.source,
                );
            }
            let Operand::Reg(rx) = instr.ops[0] else {
                return e("E-OPERAND", format!("'{}' expects a register first", info.name), instr.source);
            };
            let (off, base, off_text) = match spec {
                MemSpec::Off { off, base } => (off, base, off.to_string()),
                MemSpec::Lo { sym, base } => {
                    let addr = self.resolve_sym(&sym, instr.source)?;
                    let lo = (((addr << 20) as i32) >> 20) as i64;
                    (lo, base, format!("%lo({sym})"))
                }
            };
            if !(-2048..=2047).contains(&off) {
                return e("E-IMM", format!("offset {off} does not fit in 12 bits"), instr.source);
            }
            return match info.format {
                Format::I => Ok((
                    encode::encode(info, &[rx as u32, base as u32, (off as u32) & 0xfff]),
                    format!("{} {}, {}({})", info.name, abi_name(rx), off_text, abi_name(base)),
                )),
                Format::S => Ok((
                    encode::encode(info, &[base as u32, rx as u32, (off as u32) & 0xfff]),
                    format!("{} {}, {}({})", info.name, abi_name(rx), off_text, abi_name(base)),
                )),
                _ => e(
                    "E-OPERAND",
                    format!("'{}' does not take offset(base) operands", info.name),
                    instr.source,
                ),
            };
        }

        let want: &[OpKind] = match info.format {
            Format::R => &[OpKind::Reg, OpKind::Reg, OpKind::Reg],
            Format::I if info.opcode == encode::SYSTEM => &[],
            Format::I => &[OpKind::Reg, OpKind::Reg, OpKind::Imm(Range::I)],
            Format::Csr => return self.encode_csr(info, instr),
            Format::S => &[OpKind::Reg, OpKind::Reg, OpKind::Imm(Range::I)],
            Format::B => &[OpKind::Reg, OpKind::Reg, OpKind::Branch],
            Format::U => &[OpKind::Reg, OpKind::Imm(Range::U)],
            Format::J => &[OpKind::Reg, OpKind::Branch],
        };
        if instr.ops.len() != want.len() {
            return e(
                "E-OPERAND",
                format!("'{}' expects {} operand(s), found {}", info.name, want.len(), instr.ops.len()),
                instr.source,
            );
        }

        let mut enc_ops: Vec<u32> = Vec::new();
        let mut text_ops: Vec<String> = Vec::new();
        // Shift-immediates carry funct7 in imm[11:5] and the shift amount in
        // imm[4:0]; the user writes just the amount.
        let shift_imm = matches!(info.name, "slli" | "srli" | "srai");
        for (k, kind) in want.iter().enumerate() {
            let op = &instr.ops[k];
            match (kind, op) {
                (OpKind::Reg, Operand::Reg(rx)) => {
                    enc_ops.push(*rx as u32);
                    text_ops.push(abi_name(*rx).to_string());
                }
                (OpKind::Imm(_), Operand::Imm(v)) if shift_imm => {
                    if !(0..=31).contains(v) {
                        return e("E-IMM", format!("shift amount {v} must be between 0 and 31"), instr.source);
                    }
                    enc_ops.push((info.funct7 << 5) | (*v as u32));
                    text_ops.push(v.to_string());
                }
                (OpKind::Imm(range), Operand::Imm(v)) => {
                    let u = check_imm(*range, *v).map_err(|msg| ("E-IMM", msg, instr.source))?;
                    enc_ops.push(u);
                    text_ops.push(v.to_string());
                }
                (OpKind::Branch, Operand::Sym(s)) => {
                    let addr = self.resolve_sym(s, instr.source)?;
                    let delta = (addr as i64) - (instr.addr as i64);
                    check_branch(delta).map_err(|msg| ("E-BRANCH", msg, instr.source))?;
                    enc_ops.push(delta as u32);
                    text_ops.push(s.clone());
                }
                (OpKind::Imm(Range::U), Operand::Sym(s)) => {
                    let addr = self.resolve_sym(s, instr.source)?;
                    enc_ops.push((addr >> 12) & 0xfffff);
                    text_ops.push(s.clone());
                }
                (OpKind::Imm(Range::U), Operand::HiSym(s)) => {
                    let addr = self.resolve_sym(s, instr.source)?;
                    let lo = (((addr << 20) as i32) >> 20) as i64;
                    let hi = ((addr as i64) - lo) >> 12;
                    enc_ops.push((hi as u32) & 0xfffff);
                    text_ops.push(format!("%hi({s})"));
                }
                (OpKind::Imm(Range::I), Operand::LoSym(s)) => {
                    let addr = self.resolve_sym(s, instr.source)?;
                    enc_ops.push(((((addr << 20) as i32) >> 20) as u32) & 0xfff);
                    text_ops.push(format!("%lo({s})"));
                }
                (OpKind::Imm(Range::U), Operand::HiPcRel(s)) => {
                    let addr = self.resolve_sym(s, instr.source)?;
                    let delta = (addr as i64) - (instr.addr as i64);
                    enc_ops.push((((delta + 0x800) >> 12) as u32) & 0xfffff);
                    text_ops.push(format!("%pcrel_hi({s})"));
                }
                (OpKind::Imm(Range::I), Operand::LoPcRel(s, base_addr)) => {
                    let addr = self.resolve_sym(s, instr.source)?;
                    let delta = (addr as i64) - (*base_addr as i64);
                    enc_ops.push(((((delta << 20) as i32) >> 20) as u32) & 0xfff);
                    text_ops.push(format!("%pcrel_lo({s})"));
                }
                _ => return Err(bad_op(k)),
            }
        }

        let word = encode::encode(info, &enc_ops);
        let mut text = String::from(info.name);
        if !text_ops.is_empty() {
            text.push(' ');
            text.push_str(&text_ops.join(", "));
        } else if info.opcode == encode::SYSTEM {
            // ecall / ebreak render bare.
        }
        Ok((word, text))
    }

    fn resolve_sym(&self, name: &str, pos: SourcePos) -> Result<u32, (&'static str, String, SourcePos)> {
        self.symbols
            .get(name)
            .map(|s| s.addr)
            .ok_or(("E-UNDEF", format!("undefined symbol '{name}'"), pos))
    }

    /// `csrrw rd, csr, rs1` family. The CSR operand accepts names from the
    /// RARS CSR set or a number; the final operand is a register, or a
    /// 5-bit immediate for the `i` variants.
    fn encode_csr(
        &self,
        info: &InstructionInfo,
        instr: &RawInstr,
    ) -> Result<(u32, String), (&'static str, String, SourcePos)> {
        let e = |code: &'static str, msg: String, pos: SourcePos| Err((code, msg, pos));
        if instr.ops.len() != 3 {
            return e(
                "E-OPERAND",
                format!("'{}' expects 3 operand(s), found {}", info.name, instr.ops.len()),
                instr.source,
            );
        }
        let Operand::Reg(rd) = instr.ops[0] else {
            return e("E-OPERAND", format!("'{}' expects a destination register first", info.name), instr.source);
        };
        let csr_num = match &instr.ops[1] {
            // RARS lets a register-style number name the CSR (x0 = 0).
            Operand::Reg(r) => *r as u32,
            Operand::Imm(v) if (0..=4095).contains(v) => *v as u32,
            Operand::Sym(name) => match csr_by_name(name) {
                Some(n) => n,
                None => {
                    return e(
                        "E-CSR",
                        format!("unknown CSR '{name}' (use a name like 'uscratch' or 0 to 4095)"),
                        instr.source,
                    )
                }
            },
            _ => return e("E-OPERAND", format!("'{}' expects a CSR name or number second", info.name), instr.source),
        };
        let immediate_form = info.funct3 >= 5;
        let third = match (&instr.ops[2], immediate_form) {
            (Operand::Reg(r), false) => *r as u32,
            (Operand::Sym(name), false) => match reg_by_name(name) {
                Some(r) => r as u32,
                None => return e("E-OPERAND", format!("'{}' expects a source register third", info.name), instr.source),
            },
            (Operand::Imm(v), true) if (0..=31).contains(v) => *v as u32,
            (Operand::Imm(_), true) => {
                return e("E-IMM", "the immediate form of this CSR instruction takes 0 to 31".into(), instr.source)
            }
            _ => return e("E-OPERAND", format!("'{}' expects a register or immediate third", info.name), instr.source),
        };
        let word = encode::encode(info, &[rd as u32, csr_num, third]);
        let third_text = if immediate_form { third.to_string() } else { abi_name(third as u8).to_string() };
        Ok((word, format!("{} {}, 0x{:03x}, {}", info.name, abi_name(rd), csr_num, third_text)))
    }
}

fn int_le(v: i64, width: usize) -> Vec<u8> {
    match width {
        4 => (v as u32).to_le_bytes().to_vec(),
        2 => (v as u16).to_le_bytes().to_vec(),
        _ => vec![v as u8],
    }
}

/// Textual parameter substitution: every body token whose text equals a
/// `%param` is replaced by that argument's token sequence; other tokens pass
/// through untouched. Substituted tokens keep the parameter's position so
/// diagnostics point at the offending argument slot.
fn substitute_params(line: &[Token], params: &[String], args: &[Vec<Token>]) -> Vec<Token> {
    let mut out = Vec::with_capacity(line.len());
    for t in line {
        let Tok::Ident(text) = &t.tok else {
            out.push(t.clone());
            continue;
        };
        match params.iter().position(|p| p == text).and_then(|k| args.get(k)) {
            Some(arg) => {
                for a in arg {
                    out.push(Token { tok: a.tok.clone(), pos: t.pos });
                }
            }
            None => out.push(t.clone()),
        }
    }
    out
}

fn check_imm(range: Range, v: i64) -> Result<u32, String> {
    match range {
        Range::I => {
            if !(-2048..=2047).contains(&v) {
                return Err(format!("immediate {v} does not fit in 12 signed bits"));
            }
            Ok((v as u32) & 0xfff)
        }
        Range::U => {
            if !(0..=0xfffff).contains(&v) {
                return Err(format!("immediate {v} does not fit in 20 bits"));
            }
            Ok(v as u32)
        }
    }
}

fn check_branch(delta: i64) -> Result<(), String> {
    if delta % 2 != 0 {
        return Err(format!("branch target offset {delta} is not even"));
    }
    if !(-4096..=4094).contains(&delta) {
        return Err(format!("branch target offset {delta} exceeds the 13-bit branch range"));
    }
    Ok(())
}

/// Split a value into the paired %hi/%lo that lui+addi expansions need:
/// `lo` is the sign-extended low 12 bits and `hi` compensates so the sum
/// is exact.
fn hi_lo(v: i64) -> (i64, i64) {
    let lo = (((v as i32) << 20) >> 20) as i64;
    let hi = (v - lo) >> 12;
    (hi, lo)
}

const ABI: &[&str] = &[
    "zero", "ra", "sp", "gp", "tp", "t0", "t1", "t2", "s0", "s1", "a0", "a1", "a2", "a3", "a4", "a5",
    "a6", "a7", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11", "t3", "t4", "t5", "t6",
];

/// CSR names from RARS's set, usable as the CSR operand.
pub fn csr_by_name(name: &str) -> Option<u32> {
    const CSRS: &[(&str, u32)] = &[
        ("ustatus", 0x000),
        ("fflags", 0x001),
        ("frm", 0x002),
        ("fcsr", 0x003),
        ("uie", 0x004),
        ("utvec", 0x005),
        ("uscratch", 0x040),
        ("uepc", 0x041),
        ("ucause", 0x042),
        ("utval", 0x043),
        ("uip", 0x044),
        ("cycle", 0xC00),
        ("time", 0xC01),
        ("instret", 0xC02),
        ("cycleh", 0xC80),
        ("timeh", 0xC81),
        ("instreth", 0xC82),
    ];
    CSRS.iter().find(|(n, _)| *n == name).map(|(_, v)| *v)
}

/// Register names, numeric and ABI (RARS accepts both sets).
pub fn reg_by_name(name: &str) -> Option<u8> {
    if let Some(rest) = name.strip_prefix('x') {
        if let Ok(n) = rest.parse::<u8>() {
            if n < 32 {
                return Some(n);
            }
        }
    }
    if name == "fp" {
        return Some(8);
    }
    if name == "at" {
        // RARS's assembler-temporary alias for x1.
        return Some(1);
    }
    ABI.iter().position(|n| *n == name).map(|i| i as u8)
}

pub fn abi_name(r: u8) -> &'static str {
    ABI[r as usize]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Severity;

    fn asm(src: &str) -> crate::AsmResult {
        let files = vec![InputFile { name: "t.s".into(), source: src.into() }];
        crate::assemble(&files, &AsmConfig::default())
    }

    #[test]
    fn basic_program_encodes() {
        let r = asm("addi a0, zero, 5\nadd a1, a0, a0\necall\n");
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let p = r.program.unwrap();
        assert_eq!(p.statements.len(), 3);
        assert_eq!(p.statements[0].encoding, 0x0050_0513); // addi a0, zero, 5
        assert_eq!(p.statements[1].encoding, 0x00a5_05b3); // add a1, a0, a0
        assert_eq!(p.statements[2].encoding, 0x0000_0073);
        assert_eq!(p.statements[0].basic_text.as_ref(), "addi a0, zero, 5");
    }

    #[test]
    fn li_small_and_large() {
        let r = asm("li a0, 5\nli a1, 0x12345678\n");
        assert!(!r.has_errors());
        let p = r.program.unwrap();
        assert_eq!(p.statements.len(), 3); // addi, lui, addi
        assert_eq!(p.statements[0].encoding, 0x0050_0513);
        // lui a1, 0x12345
        assert_eq!(p.statements[1].encoding, 0x1234_55b7);
        // addi a1, a1, 0x678
        assert_eq!(p.statements[2].encoding, 0x6785_8593);
        assert_eq!(p.statements[1].expanded_from.map(|s| s.line), Some(2));
    }

    #[test]
    fn la_and_data_layout() {
        let r = asm(".data\nmsg: .asciz \"Hi\"\n.align 2\nval: .word 42, msg\n.text\nla a0, msg\nlw a1, val\n");
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let p = r.program.unwrap();
        assert_eq!(p.data.base, 0x1001_0000);
        // "Hi\0" + word 42 + word msg_addr
        assert_eq!(&p.data.bytes[0..3], b"Hi\0");
        assert_eq!(p.symbols.get("msg").unwrap().addr, 0x1001_0000);
        assert_eq!(p.symbols.get("val").unwrap().addr, 0x1001_0004);
        assert_eq!(&p.data.bytes[4..8], &42u32.to_le_bytes());
        assert_eq!(&p.data.bytes[8..12], &0x1001_0000u32.to_le_bytes());
        // la expands to lui+addi; label-form `lw a1, val` expands to
        // lui at + lw at-relative, matching RARS.
        assert_eq!(p.statements.len(), 4);
    }

    #[test]
    fn branch_forward_and_back() {
        let r = asm(
            "start: beqz a0, start\n j end\nend: bnez a0, start\n",
        );
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let p = r.program.unwrap();
        let base = p.text_base;
        // beq a0, zero, delta 0
        assert_eq!(p.statement_at(base).unwrap().encoding, 0x0005_0063);
        // j end: jal x0, +4
        assert_eq!(p.statement_at(base + 4).unwrap().encoding, 0x0040_006f);
        // bnez a0, start: bne a0, zero, -8
        assert_eq!(p.statement_at(base + 8).unwrap().encoding, 0xfe05_1ce3);
    }

    #[test]
    fn call_uses_auipc_jalr() {
        let r = asm("main:\n call fn\n ret\nfn: ret\n");
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let p = r.program.unwrap();
        let base = p.text_base;
        // call is 8 bytes (auipc + jalr), ret 4, so fn sits at base+12.
        // auipc ra, %pcrel_hi(fn): hi = (12+0x800)>>12 = 0
        assert_eq!(p.statement_at(base).unwrap().encoding, 0x0000_0097);
        // jalr ra, %pcrel_lo(fn)(ra): lo = 12
        assert_eq!(p.statement_at(base + 4).unwrap().encoding, 0x00c0_80e7);
    }

    #[test]
    fn diagnostics_carry_positions() {
        let r = asm("addi a0, zero, 9999\n");
        assert!(r.has_errors());
        let d = &r.diagnostics[0];
        assert_eq!(d.code, "E-IMM");
        assert_eq!(d.pos.line, 1);
        assert_eq!(d.severity, Severity::Error);
    }

    #[test]
    fn undefined_symbol_reported() {
        let r = asm("j nowhere\n");
        assert!(r.has_errors());
        assert_eq!(r.diagnostics[0].code, "E-UNDEF");
    }

    #[test]
    fn eqv_substitution() {
        let r = asm(".eqv SIZE 10\nli a0, SIZE\n");
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let p = r.program.unwrap();
        assert_eq!(p.statements[0].encoding, 0x00a0_0513); // addi a0, zero, 10
    }

    #[test]
    fn globl_marks_symbol() {
        let r = asm(".globl main\nmain: ret\n");
        assert!(!r.has_errors());
        let p = r.program.unwrap();
        assert!(p.symbols.get("main").unwrap().global);
    }

    #[test]
    fn unaligned_halfword_data() {
        let r = asm(".data\na: .byte 1\n.align 2\nb: .word 0x11223344\n");
        assert!(!r.has_errors());
        let p = r.program.unwrap();
        // byte at 0, three pad bytes, word at 4
        assert_eq!(p.data.bytes[0], 1);
        assert_eq!(p.data.bytes[4], 0x44);
        assert_eq!(p.symbols.get("b").unwrap().addr, 0x1001_0004);
    }

    #[test]
    fn store_offsets_encode() {
        let r = asm("sw a0, 8(sp)\nlw a1, -4(sp)\n");
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let p = r.program.unwrap();
        assert_eq!(p.statements[0].encoding, 0x00a1_2423); // sw a0, 8(sp)
        assert_eq!(p.statements[1].encoding, 0xffc1_2583); // lw a1, -4(sp)
        assert_eq!(p.statements[0].basic_text.as_ref(), "sw a0, 8(sp)");
    }

    #[test]
    fn x_names_and_abi_names_both_work() {
        let r = asm("addi x10, x0, 5\naddi a0, zero, 5\n");
        assert!(!r.has_errors());
        let p = r.program.unwrap();
        assert_eq!(p.statements[0].encoding, p.statements[1].encoding);
    }

    #[test]
    fn shift_immediates_carry_funct7() {
        let r = asm("srai a0, a1, 3\nslli a1, a1, 4\n");
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let p = r.program.unwrap();
        assert_eq!(p.statements[0].encoding, 0x4035_d513); // srai a0, a1, 3
        assert_eq!(p.statements[1].encoding, 0x0045_9593); // slli a1, a1, 4
    }

    #[test]
    fn negative_li_split() {
        // -100000 = 0xFFFE7960; low 12 bits 0x960 sign-extend to -1696,
        // so hi compensates: (-100000 + 1696) >> 12 = -24 = 0xFFFE8 (20-bit).
        let r = asm("li a0, -100000\n");
        assert!(!r.has_errors());
        let p = r.program.unwrap();
        assert_eq!(p.statements[0].encoding, 0xfffe_8537); // lui a0, 0xfffe8
        assert_eq!(p.statements[1].encoding, 0x9605_0513); // addi a0, a0, -1696
    }

    #[test]
    fn label_form_load_expands() {
        let r = asm(".data\nv: .word 7\n.text\nlw a0, v\n");
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let p = r.program.unwrap();
        assert_eq!(p.statements.len(), 2);
        assert_eq!(p.statements[1].basic_text.as_ref(), "lw a0, %lo(v)(ra)");
    }

    #[test]
    fn macro_with_two_params() {
        let r = asm(
            ".macro loadpair(%a, %b)\n    addi %a, %b, 1\n    addi %a, %a, 2\n.end_macro\nloadpair(a0, a1)\n",
        );
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let p = r.program.unwrap();
        assert_eq!(p.statements.len(), 2);
        assert_eq!(p.statements[0].encoding, 0x0015_8513); // addi a0, a1, 1
        assert_eq!(p.statements[1].encoding, 0x0025_0513); // addi a0, a0, 2
    }

    #[test]
    fn macro_params_without_parens() {
        let r = asm(".macro bump %r, %n\n    addi %r, %r, %n\n.end_macro\nbump(sp, 8)\n");
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let p = r.program.unwrap();
        assert_eq!(p.statements[0].encoding, 0x0081_0113); // addi sp, sp, 8
    }

    #[test]
    fn macro_arity_mismatch_is_error() {
        let r = asm(".macro pair(%a, %b)\n    add %a, %b, zero\n.end_macro\npair(a0)\n");
        assert!(r.has_errors());
        assert!(r.diagnostics.iter().any(|d| d.code == "E-OPERAND" && d.message.contains("pair")));
    }

    #[test]
    fn macro_calls_macro() {
        let r = asm(
            ".macro inner(%r)\n    addi %r, %r, 4\n.end_macro\n\
             .macro outer(%r)\n    inner(%r)\n    addi %r, %r, 1\n.end_macro\nouter(a0)\n",
        );
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let p = r.program.unwrap();
        assert_eq!(p.statements.len(), 2);
        assert_eq!(p.statements[0].encoding, 0x0045_0513); // addi a0, a0, 4
        assert_eq!(p.statements[1].encoding, 0x0015_0513); // addi a0, a0, 1
    }

    #[test]
    fn macro_recursion_hits_depth_cap() {
        let r = asm(".macro spin\n    spin()\n.end_macro\nspin()\n");
        assert!(r.has_errors());
        let hits = r.diagnostics.iter().filter(|d| d.code == "E-MACRO-DEPTH").count();
        assert_eq!(hits, 1, "depth abort should stop the chain instead of erroring per frame");
    }

    #[test]
    fn unknown_macro_call_keeps_mnemonic_error() {
        let r = asm("nosuch(a0, a1)\n");
        assert!(r.has_errors());
        assert!(r.diagnostics.iter().any(|d| d.code == "E-MNEMONIC" && d.message.contains("nosuch")));
    }

    #[test]
    fn eqv_inside_macro_body() {
        // .eqv applies at expansion time and sticks for later lines too.
        let r = asm(".macro setv(%r)\n    .eqv VAL 9\n    li %r, VAL\n.end_macro\nsetv(a0)\nli a1, VAL\n");
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let p = r.program.unwrap();
        assert_eq!(p.statements[0].encoding, 0x0090_0513); // addi a0, zero, 9
        assert_eq!(p.statements[1].encoding, 0x0090_0593); // addi a1, zero, 9
    }

    #[test]
    fn macro_body_with_label() {
        let r = asm(
            ".macro skipnext\n    beq zero, zero, done\n    addi zero, zero, 1\ndone:\n.end_macro\nskipnext()\n",
        );
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let p = r.program.unwrap();
        assert_eq!(p.statements.len(), 2); // the label itself emits nothing
        assert_eq!(p.symbols.get("done").unwrap().addr, p.text_base + 8);
        assert_eq!(p.statements[0].encoding, 0x0000_0463); // beq zero, zero, +8
    }

    #[test]
    fn unterminated_macro_is_error() {
        let r = asm(".macro loose\n    nop\n");
        assert!(r.has_errors());
        assert!(r.diagnostics.iter().any(|d| d.code == "E-DIRECTIVE" && d.message.contains(".end_macro")));
    }
}
