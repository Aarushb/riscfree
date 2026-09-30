//! Parsing and the two passes: collect/expand, then resolve/encode.

use crate::encode::{self, Format, InstructionInfo, OpKind, Range};
use crate::lexer::{lex_line, Tok, Token};
use crate::{
    AsmConfig, AsmResult, DataImage, Diagnostic, InputFile, Program, SourcePos, Statement, Symbol,
    SymbolTable,
};
use std::collections::{BTreeMap, HashSet};
use std::iter::Peekable;
use std::sync::Arc;

/// Maximum nesting of macro expansions; deeper chains report E-MACRO-DEPTH
/// instead of exhausting the stack.
const MACRO_DEPTH_LIMIT: u32 = 32;

#[derive(Debug, Clone)]
pub(crate) enum Operand {
    Reg(u8),
    /// Floating-point register (flw/fsw/F ops); keeps FP names rendering
    /// correctly and lets encode_one reject mixed register classes.
    FReg(u8),
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
    Mem {
        off: i64,
        base: u8,
    },
    /// Label-form load/store relocated through `at`: the `lw rd, sym` RARS
    /// pseudo-form. Encodes as `%lo(sym)(base)`.
    MemLo {
        sym: String,
        base: u8,
    },
}

#[derive(Debug, Clone)]
struct RawInstr {
    name: &'static str,
    ops: Vec<Operand>,
    addr: u32,
    source: SourcePos,
    expanded_from: Option<SourcePos>,
    /// 4 for regular instructions, 2 for compressed ones.
    size: u32,
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
    /// `.extern` reservations as (address, zero bytes); the cursor lives in
    /// `extern_next`, independent of the segment cursors.
    extern_chunks: Vec<(u32, Vec<u8>)>,
    extern_next: u32,
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
        extern_chunks: Vec::new(),
        extern_next: cfg.extern_base,
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
    AsmResult {
        program: Some(program),
        diagnostics: a.diags,
    }
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
            let pos = SourcePos {
                file: file_id,
                line: line_idx as u32 + 1,
                col: 0,
            };
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
            let lpos = SourcePos {
                file: pos.file,
                line: line_idx as u32 + 1,
                col: 0,
            };
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
        let Some(Token {
            tok: Tok::Ident(name),
            ..
        }) = rest.first()
        else {
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
                                self.err(
                                    "E-DIRECTIVE",
                                    "expected ',' or ')' in the .macro parameter list",
                                    pos,
                                );
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
            self.err(
                "E-DIRECTIVE",
                "unexpected tokens after the .macro parameter list",
                pos,
            );
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
                    args.last_mut()
                        .expect("always at least one arg")
                        .push(t.clone());
                }
                Tok::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        if k + 1 != toks.len() {
                            self.err(
                                "E-SYNTAX",
                                "unexpected tokens after the macro call",
                                toks[k + 1].pos,
                            );
                        }
                        // `name()` is a zero-argument call, not one empty one.
                        if args.len() == 1 && args[0].is_empty() {
                            args.clear();
                        }
                        return args;
                    }
                    args.last_mut()
                        .expect("always at least one arg")
                        .push(t.clone());
                }
                Tok::Comma if depth == 1 => args.push(Vec::new()),
                _ => args
                    .last_mut()
                    .expect("always at least one arg")
                    .push(t.clone()),
            }
        }
        self.err(
            "E-SYNTAX",
            format!("macro call '{name}' is missing ')'"),
            pos,
        );
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
        let Some(def) = self.macros.get(name).cloned() else {
            return;
        };
        if args.len() != def.params.len() {
            self.err(
                "E-OPERAND",
                format!(
                    "macro '{name}' expects {} parameter(s), found {}",
                    def.params.len(),
                    args.len()
                ),
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
        while i + 1 < toks.len()
            && matches!(&toks[i].tok, Tok::Ident(_))
            && toks[i + 1].tok == Tok::Colon
        {
            if let Tok::Ident(name) = &toks[i].tok {
                let name = name.clone();
                let addr = self.cur_addr();
                let global = self.globals.contains(&name);
                self.symbols.define(
                    name.clone(),
                    Symbol {
                        addr,
                        global,
                        source: toks[i].pos,
                    },
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
                    return (
                        Operand::Mem {
                            off: *v,
                            base: off_base,
                        },
                        i + 4,
                    );
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
            Tok::Ident(name) => {
                if let Some(r) = reg_by_name(name) {
                    (Operand::Reg(r), i + 1)
                } else if let Some(r) = freg_by_name(name) {
                    (Operand::FReg(r), i + 1)
                } else {
                    (Operand::Sym(name.clone()), i + 1)
                }
            }
            Tok::Float(_) => {
                self.err(
                    "E-OPERAND",
                    "a floating-point literal is only valid in .float/.double",
                    t.pos,
                );
                (Operand::Imm(0), i + 1)
            }
            _ => {
                self.err(
                    "E-OPERAND",
                    "expected a register, immediate, label, or offset(base)",
                    t.pos,
                );
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
            self.err(
                "E-OPERAND",
                "expected a base register inside offset(base)",
                base_tok.pos,
            );
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
                self.segment = if d == ".text" {
                    Segment::Text
                } else {
                    Segment::Data
                };
                if let Some(Token {
                    tok: Tok::Int(addr),
                    ..
                }) = rest.first()
                {
                    match self.segment {
                        Segment::Text => self.text_addr = *addr as u32,
                        Segment::Data => self.data_addr = *addr as u32,
                    }
                }
            }
            ".align" => {
                let Some(Token {
                    tok: Tok::Int(n), ..
                }) = rest.first()
                else {
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
                let Some(Token {
                    tok: Tok::Int(n), ..
                }) = rest.first()
                else {
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
                        _ => self.err(
                            "E-DIRECTIVE",
                            format!("{d} expects integers or labels"),
                            rest[j].pos,
                        ),
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
                let Tok::Ident(name) = &rest[0].tok else {
                    unreachable!()
                };
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
                if self.segment != Segment::Data {
                    self.err("E-DIRECTIVE", format!("{d} must appear inside .data"), pos);
                }
                let width = if d == ".float" { 4usize } else { 8 };
                let mut j = 0usize;
                while j < rest.len() {
                    // Values arrive as float literals; integer literals are
                    // accepted as exact values (RARS parity).
                    let v = match &rest[j].tok {
                        Tok::Float(v) => Some(*v),
                        Tok::Int(v) => Some(*v as f64),
                        Tok::Ident(name) => match name.as_str() {
                            "inf" => Some(f64::INFINITY),
                            "nan" => Some(f64::NAN),
                            other => {
                                self.err(
                                    "E-DIRECTIVE",
                                    format!("unknown float value '{other}'"),
                                    rest[j].pos,
                                );
                                None
                            }
                        },
                        Tok::Comma => None,
                        _ => {
                            self.err(
                                "E-DIRECTIVE",
                                format!("{d} expects floating-point values"),
                                rest[j].pos,
                            );
                            None
                        }
                    };
                    if let Some(v) = v {
                        let bytes = if width == 4 {
                            (v as f32).to_bits().to_le_bytes().to_vec()
                        } else {
                            v.to_bits().to_le_bytes().to_vec()
                        };
                        self.push_data(&bytes);
                        self.advance(width as u32);
                    }
                    j += 1;
                }
            }
            // Definitions are consumed directly by pass_one; seeing either
            // directive here means it was misplaced.
            ".macro" => self.err(
                "E-DIRECTIVE",
                "'.macro' must be the first token on its line",
                pos,
            ),
            ".end_macro" => self.err("E-DIRECTIVE", "'.end_macro' without a matching .macro", pos),
            ".include" => self.err("E-UNSUPPORTED", ".include lands in phase 1", pos),
            // `.extern name size` reserves `size` bytes at the head of the
            // data segment (RARS's extern segment) and defines `name` there.
            // The cursor is independent of the segment selectors, so the
            // directive is legal in .text and .data alike.
            ".extern" => {
                let Some(Token {
                    tok: Tok::Ident(name),
                    ..
                }) = rest.first()
                else {
                    self.err("E-DIRECTIVE", ".extern needs a name", pos);
                    return;
                };
                let name = name.clone();
                let Some(Token {
                    tok: Tok::Int(size),
                    ..
                }) = rest.get(1)
                else {
                    self.err("E-DIRECTIVE", ".extern needs a byte count", pos);
                    return;
                };
                if *size < 1 || *size > u32::MAX as i64 {
                    self.err(
                        "E-DIRECTIVE",
                        format!(".extern reservation must be at least 1 byte, found {size}"),
                        pos,
                    );
                    return;
                }
                if let Some(extra) = rest.get(2) {
                    self.err(
                        "E-DIRECTIVE",
                        "unexpected tokens after the .extern size",
                        extra.pos,
                    );
                    return;
                }
                let Some(addr) = self.extern_next.checked_add(*size as u32) else {
                    self.err(
                        "E-DIRECTIVE",
                        ".extern reservation overflows the address space",
                        pos,
                    );
                    return;
                };
                // A duplicate reports E-DUP-SYM from `define` and allocates
                // nothing new.
                let is_new = self.symbols.get(&name).is_none();
                // RARS declares extern symbols global.
                self.symbols.define(
                    name,
                    Symbol {
                        addr: self.extern_next,
                        global: true,
                        source: pos,
                    },
                    &mut self.diags,
                );
                if is_new {
                    self.extern_chunks
                        .push((self.extern_next, vec![0; *size as u32 as usize]));
                    self.extern_next = addr;
                }
            }
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
        // Compressed instructions: 2-byte encodings on their own path.
        if self.cfg.allow_compressed && mnemonic.starts_with("c.") {
            let name = crate::compressed::lookup_name(mnemonic);
            self.push_compressed(name, ops, pos);
            return;
        }
        if mnemonic.starts_with("c.") {
            self.err(
                "E-XLEN",
                format!("compressed instruction '{mnemonic}' requires enabling the C extension (allow_compressed)"),
                pos,
            );
            self.advance(4);
            return;
        }
        if let Some(info) = encode::lookup(mnemonic) {
            // RV64-only instructions (ld/sd/lwu, the *w ops, 64-bit FP
            // conversions) need 64-bit mode, like RARS's RV64 setting.
            if info.rv64_only && !self.cfg.rv64 {
                self.err(
                    "E-XLEN",
                    format!("'{mnemonic}' requires 64-bit mode (RV64); enable rv64 in the assembler settings"),
                    pos,
                );
                self.advance(4);
                return;
            }
            // RARS label-form loads/stores: `lw rd, sym` / `sw rt, sym`, and
            // the FP forms `flw fd, sym` / `fsw fs, sym`. In RV64 the same
            // path serves `ld rd, sym` / `sd rt, sym` (lui %hi + ld/sd %lo).
            let fp_mem = matches!(
                info.kind,
                encode::InstrKind::FpLoad | encode::InstrKind::FpStore
            );
            let mem_sym = matches!(ops.last(), Some(Operand::Sym(_)))
                && matches!(info.format, Format::I | Format::S)
                && (info.opcode == encode::LOAD
                    || info.opcode == encode::STORE
                    || info.opcode == encode::LOAD_FP
                    || info.opcode == encode::STORE_FP)
                && self.cfg.allow_pseudo;
            if mem_sym {
                let Operand::Sym(sym) = ops.remove(1) else {
                    unreachable!()
                };
                let rd = match ops.first() {
                    Some(Operand::Reg(r)) | Some(Operand::FReg(r)) => *r,
                    _ => 0,
                };
                let rd_op = if fp_mem {
                    Operand::FReg(rd)
                } else {
                    Operand::Reg(rd)
                };
                self.push_basic(
                    "lui",
                    vec![Operand::Reg(1), Operand::HiSym(sym.clone())],
                    pos,
                    pos,
                );
                self.push_basic(
                    info.name,
                    vec![rd_op, Operand::MemLo { sym, base: 1 }],
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
                size: 4,
            });
            self.advance(4);
            return;
        }
        if !self.cfg.allow_pseudo {
            self.err(
                "E-PSEUDO",
                format!("pseudo-instructions are disabled ('{mnemonic}')"),
                pos,
            );
            self.advance(4);
            return;
        }
        self.expand_pseudo(mnemonic, ops, pos);
    }

    /// Push one CSR-format instruction (helper for the CSR pseudo-ops).
    fn push_csr_basic(
        &mut self,
        name: &'static str,
        rd: Operand,
        csr: Operand,
        rs: Option<Operand>,
        from: SourcePos,
        pos: SourcePos,
    ) {
        let mut ops = vec![rd, csr];
        if let Some(rs) = rs {
            ops.push(rs);
        }
        self.raw.push(RawInstr {
            name,
            ops,
            addr: self.text_addr,
            source: pos,
            expanded_from: Some(from),
            size: 4,
        });
        self.advance(4);
    }

    fn push_basic(
        &mut self,
        name: &'static str,
        ops: Vec<Operand>,
        from: SourcePos,
        pos: SourcePos,
    ) {
        self.raw.push(RawInstr {
            name,
            ops,
            addr: self.text_addr,
            source: pos,
            expanded_from: Some(from),
            size: 4,
        });
        self.advance(4);
    }

    /// Push one compressed instruction (2 bytes).
    fn push_compressed(&mut self, name: &'static str, ops: Vec<Operand>, pos: SourcePos) {
        self.raw.push(RawInstr {
            name: self.intern_compressed(name),
            ops,
            addr: self.text_addr,
            source: pos,
            expanded_from: None,
            size: 2,
        });
        self.advance(2);
    }

    fn intern_compressed(&self, name: &str) -> &'static str {
        // Compressed names are fixed strings; leak-free via the compressed
        // module's known-name table lookup.
        crate::compressed::lookup_name(name)
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
                let v = *v;
                if (-2048..=2047).contains(&v) {
                    self.push_basic("addi", vec![r(rd), r(0), imm(v)], pos, pos);
                } else if self.cfg.rv64 {
                    // RARS's 64-bit li tiers (PseudoOps-64.txt): a 32-bit
                    // value sign-extends through lui+addiw, and anything
                    // wider uses the 8-instruction LIX chain
                    //   lui rd, LIA; addiw rd, rd, LIB;
                    //   slli rd, rd, 11; addi rd, rd, LIC;
                    //   slli rd, rd, 11; addi rd, rd, LID;
                    //   slli rd, rd, 10; addi rd, rd, LIE
                    // whose operands (rars ExtendedInstruction.java) are
                    //   LIA = (h >> 12) + bit11(h)   (sign-extended h>>12)
                    //   LIB = sign_ext12(h)          (the %lo pairing)
                    //   LIC = l[31:21]  LID = l[20:10]  LIE = l[9:0]
                    // with h the high and l the low 32 bits of the value.
                    // The chain shifts in two 11-bit and one 10-bit unsigned
                    // chunk, so only the first pair needs sign compensation.
                    if (-2147483648..=2147483647).contains(&v) {
                        let (hi, lo) = hi_lo(v);
                        self.push_basic("lui", vec![r(rd), imm(hi & 0xfffff)], pos, pos);
                        self.push_basic("addiw", vec![r(rd), r(rd), imm(lo)], pos, pos);
                    } else {
                        let (h, l) = ((v >> 32) as i32, v as i32);
                        let extra = i64::from((h as u32 >> 11) & 1);
                        let lia = ((i64::from(h >> 12) + extra) as u32) & 0xfffff;
                        let lib = i64::from((h << 20) >> 20);
                        let lic = i64::from((l as u32 >> 21) & 0x7ff);
                        let lid = i64::from((l as u32 >> 10) & 0x7ff);
                        let lie = i64::from(l as u32 & 0x3ff);
                        self.push_basic("lui", vec![r(rd), imm(i64::from(lia))], pos, pos);
                        self.push_basic("addiw", vec![r(rd), r(rd), imm(lib)], pos, pos);
                        self.push_basic("slli", vec![r(rd), r(rd), imm(11)], pos, pos);
                        self.push_basic("addi", vec![r(rd), r(rd), imm(lic)], pos, pos);
                        self.push_basic("slli", vec![r(rd), r(rd), imm(11)], pos, pos);
                        self.push_basic("addi", vec![r(rd), r(rd), imm(lid)], pos, pos);
                        self.push_basic("slli", vec![r(rd), r(rd), imm(10)], pos, pos);
                        self.push_basic("addi", vec![r(rd), r(rd), imm(lie)], pos, pos);
                    }
                } else {
                    let (hi, lo) = hi_lo(v);
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
            // CSR pseudo-ops. Read form keeps rd first; the write/set/clear
            // forms take the CSR first and route through x0 like RARS.
            (
                "csrr",
                [Operand::Reg(rd), csr @ (Operand::Sym(_) | Operand::Reg(_) | Operand::Imm(_))],
            ) => {
                let (rd, csr) = (*rd, csr.clone());
                self.push_csr_basic("csrrs", r(rd), csr, Some(r(0)), pos, pos);
            }
            (
                "csrw",
                [csr @ (Operand::Sym(_) | Operand::Reg(_) | Operand::Imm(_)), Operand::Reg(rs)],
            ) => {
                let (csr, rs) = (csr.clone(), *rs);
                self.push_csr_basic("csrrw", r(0), csr, Some(r(rs)), pos, pos);
            }
            (
                "csrs",
                [csr @ (Operand::Sym(_) | Operand::Reg(_) | Operand::Imm(_)), Operand::Reg(rs)],
            ) => {
                let (csr, rs) = (csr.clone(), *rs);
                self.push_csr_basic("csrrs", r(0), csr, Some(r(rs)), pos, pos);
            }
            (
                "csrc",
                [csr @ (Operand::Sym(_) | Operand::Reg(_) | Operand::Imm(_)), Operand::Reg(rs)],
            ) => {
                let (csr, rs) = (csr.clone(), *rs);
                self.push_csr_basic("csrrc", r(0), csr, Some(r(rs)), pos, pos);
            }
            (
                "csrwi",
                [csr @ (Operand::Sym(_) | Operand::Reg(_) | Operand::Imm(_)), Operand::Imm(v)],
            ) => {
                let (csr, v) = (csr.clone(), *v);
                self.push_csr_basic("csrrwi", r(0), csr, Some(imm(v)), pos, pos);
            }
            (
                "csrsi",
                [csr @ (Operand::Sym(_) | Operand::Reg(_) | Operand::Imm(_)), Operand::Imm(v)],
            ) => {
                let (csr, v) = (csr.clone(), *v);
                self.push_csr_basic("csrrsi", r(0), csr, Some(imm(v)), pos, pos);
            }
            (
                "csrci",
                [csr @ (Operand::Sym(_) | Operand::Reg(_) | Operand::Imm(_)), Operand::Imm(v)],
            ) => {
                let (csr, v) = (csr.clone(), *v);
                self.push_csr_basic("csrrci", r(0), csr, Some(imm(v)), pos, pos);
            }
            ("call", [Operand::Sym(l)]) => {
                let l = l.clone();
                let auipc_addr = self.text_addr;
                self.push_basic("auipc", vec![r(1), Operand::HiPcRel(l.clone())], pos, pos);
                self.push_basic(
                    "jalr",
                    vec![r(1), r(1), Operand::LoPcRel(l, auipc_addr)],
                    pos,
                    pos,
                );
            }
            ("tail", [Operand::Sym(l)]) => {
                let l = l.clone();
                let auipc_addr = self.text_addr;
                self.push_basic("auipc", vec![r(6), Operand::HiPcRel(l.clone())], pos, pos);
                self.push_basic(
                    "jalr",
                    vec![r(0), r(6), Operand::LoPcRel(l, auipc_addr)],
                    pos,
                    pos,
                );
            }
            // FP pseudo-ops via the sign-inject family (canonical GNU/RARS
            // expansions: xor the sign bit for abs, flip for neg, copy for mv).
            ("fabs.s", [Operand::FReg(rd), Operand::FReg(rs)]) => {
                let (rd, rs) = (*rd, *rs);
                self.push_basic(
                    "fsgnjx.s",
                    vec![Operand::FReg(rd), Operand::FReg(rs), Operand::FReg(rs)],
                    pos,
                    pos,
                );
            }
            ("fneg.s", [Operand::FReg(rd), Operand::FReg(rs)]) => {
                let (rd, rs) = (*rd, *rs);
                self.push_basic(
                    "fsgnjn.s",
                    vec![Operand::FReg(rd), Operand::FReg(rs), Operand::FReg(rs)],
                    pos,
                    pos,
                );
            }
            ("fmv.s", [Operand::FReg(rd), Operand::FReg(rs)]) => {
                let (rd, rs) = (*rd, *rs);
                self.push_basic(
                    "fsgnj.s",
                    vec![Operand::FReg(rd), Operand::FReg(rs), Operand::FReg(rs)],
                    pos,
                    pos,
                );
            }
            ("fabs.d", [Operand::FReg(rd), Operand::FReg(rs)]) => {
                let (rd, rs) = (*rd, *rs);
                self.push_basic(
                    "fsgnjx.d",
                    vec![Operand::FReg(rd), Operand::FReg(rs), Operand::FReg(rs)],
                    pos,
                    pos,
                );
            }
            ("fneg.d", [Operand::FReg(rd), Operand::FReg(rs)]) => {
                let (rd, rs) = (*rd, *rs);
                self.push_basic(
                    "fsgnjn.d",
                    vec![Operand::FReg(rd), Operand::FReg(rs), Operand::FReg(rs)],
                    pos,
                    pos,
                );
            }
            ("fmv.d", [Operand::FReg(rd), Operand::FReg(rs)]) => {
                let (rd, rs) = (*rd, *rs);
                self.push_basic(
                    "fsgnj.d",
                    vec![Operand::FReg(rd), Operand::FReg(rs), Operand::FReg(rs)],
                    pos,
                    pos,
                );
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
            if instr.size == 2 {
                match self.encode_compressed_one(instr) {
                    Ok((word, text)) => statements.push(Statement {
                        addr: instr.addr,
                        encoding: word as u32,
                        expanded_from: instr.expanded_from,
                        source: instr.source,
                        basic_text: Arc::from(text.as_str()),
                        size: 2,
                        encoding16: Some(word),
                    }),
                    Err((code, msg, pos)) => self.err(code, msg, pos),
                }
                continue;
            }
            let Some(info) = encode::lookup(instr.name) else {
                let (source, name) = (instr.source, instr.name);
                self.err(
                    "E-MNEMONIC",
                    format!("unknown instruction '{name}'"),
                    source,
                );
                continue;
            };
            match self.encode_one(info, instr) {
                Ok((word, text)) => statements.push(Statement {
                    addr: instr.addr,
                    encoding: word,
                    expanded_from: instr.expanded_from,
                    source: instr.source,
                    basic_text: Arc::from(text.as_str()),
                    size: 4,
                    encoding16: None,
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

        // Text addresses are linear (assembly only moves forward); collect
        // them alongside the statements and derive the end from the final
        // statement's full size.
        let addrs: Vec<u32> = statements.iter().map(|st| st.addr).collect();
        let text_end = statements
            .last()
            .map(|st| st.addr + st.size)
            .unwrap_or(self.cfg.text_base);

        Program {
            text_base: self.cfg.text_base,
            statements,
            addrs,
            text_end,
            data: DataImage {
                base: self.cfg.data_base,
                bytes: std::mem::take(&mut self.data),
            },
            extern_chunks: std::mem::take(&mut self.extern_chunks),
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
            Some(Operand::Mem { off, base }) => Some(MemSpec::Off {
                off: *off,
                base: *base,
            }),
            Some(Operand::MemLo { sym, base }) => Some(MemSpec::Lo {
                sym: sym.clone(),
                base: *base,
            }),
            _ => None,
        };
        if let Some(spec) = mem {
            if instr.ops.len() != 2 {
                return e(
                    "E-OPERAND",
                    format!(
                        "'{}' takes a register and an offset(base) operand",
                        info.name
                    ),
                    instr.source,
                );
            }
            let fp_mem = matches!(
                info.kind,
                encode::InstrKind::FpLoad | encode::InstrKind::FpStore
            );
            let rx = match &instr.ops[0] {
                Operand::Reg(r) | Operand::FReg(r) => *r,
                _ => {
                    return e(
                        "E-OPERAND",
                        format!("'{}' expects a register first", info.name),
                        instr.source,
                    )
                }
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
                return e(
                    "E-IMM",
                    format!("offset {off} does not fit in 12 bits"),
                    instr.source,
                );
            }
            let rx_text = if fp_mem {
                abi_freg_name(rx)
            } else {
                abi_name(rx)
            };
            return match info.format {
                Format::I => Ok((
                    encode::encode(info, &[rx as u32, base as u32, (off as u32) & 0xfff]),
                    format!(
                        "{} {}, {}({})",
                        info.name,
                        rx_text,
                        off_text,
                        abi_name(base)
                    ),
                )),
                Format::S => Ok((
                    encode::encode(info, &[base as u32, rx as u32, (off as u32) & 0xfff]),
                    format!(
                        "{} {}, {}({})",
                        info.name,
                        rx_text,
                        off_text,
                        abi_name(base)
                    ),
                )),
                _ => e(
                    "E-OPERAND",
                    format!("'{}' does not take offset(base) operands", info.name),
                    instr.source,
                ),
            };
        }

        let want: &[OpKind] = match (info.format, info.kind) {
            // FP register classes per slot: FP ops use FP registers
            // throughout; comparisons convert to an integer destination and
            // the fcvt/fmv forms mix the two classes.
            (Format::R, encode::InstrKind::FpOp) => &[OpKind::FReg, OpKind::FReg, OpKind::FReg],
            (Format::R, encode::InstrKind::FpCmp) => &[OpKind::Reg, OpKind::FReg, OpKind::FReg],
            (Format::R2, encode::InstrKind::FpSingle) => &[OpKind::FReg, OpKind::FReg],
            (Format::R2, encode::InstrKind::FpToI) => &[OpKind::Reg, OpKind::FReg],
            (Format::R2, encode::InstrKind::FpToX) => &[OpKind::FReg, OpKind::Reg],
            (Format::R4, encode::InstrKind::FpFma) => {
                &[OpKind::FReg, OpKind::FReg, OpKind::FReg, OpKind::FReg]
            }
            (Format::R, _) => &[OpKind::Reg, OpKind::Reg, OpKind::Reg],
            (Format::R2, _) => &[OpKind::Reg, OpKind::Reg],
            (Format::I, encode::InstrKind::FpLoad) => {
                &[OpKind::FReg, OpKind::Reg, OpKind::Imm(Range::I)]
            }
            (Format::S, encode::InstrKind::FpStore) => {
                &[OpKind::FReg, OpKind::Reg, OpKind::Imm(Range::I)]
            }
            (Format::I, _) if info.opcode == encode::SYSTEM => &[],
            (Format::I, _) => &[OpKind::Reg, OpKind::Reg, OpKind::Imm(Range::I)],
            (Format::Csr, _) => return self.encode_csr(info, instr),
            (Format::S, _) => &[OpKind::Reg, OpKind::Reg, OpKind::Imm(Range::I)],
            (Format::R4, _) => &[OpKind::Reg, OpKind::Reg, OpKind::Reg, OpKind::Reg],
            (Format::B, _) => &[OpKind::Reg, OpKind::Reg, OpKind::Branch],
            (Format::U, _) => &[OpKind::Reg, OpKind::Imm(Range::U)],
            (Format::J, _) => &[OpKind::Reg, OpKind::Branch],
        };
        if instr.ops.len() != want.len() {
            return e(
                "E-OPERAND",
                format!(
                    "'{}' expects {} operand(s), found {}",
                    info.name,
                    want.len(),
                    instr.ops.len()
                ),
                instr.source,
            );
        }

        let mut enc_ops: Vec<u32> = Vec::new();
        let mut text_ops: Vec<String> = Vec::new();
        // Shift-immediates carry funct7 in imm[11:5] and the shift amount in
        // imm[4:0]; the user writes just the amount. In RV64 the base shifts
        // widen shamt to six bits (imm[5:0]); the *iw forms stay 5-bit.
        let shift_imm = matches!(
            info.name,
            "slli" | "srli" | "srai" | "slliw" | "srliw" | "sraiw"
        );
        let shamt_wide = self.cfg.rv64 && matches!(info.name, "slli" | "srli" | "srai");
        for (k, kind) in want.iter().enumerate() {
            let op = &instr.ops[k];
            match (kind, op) {
                (OpKind::Reg, Operand::Reg(rx)) => {
                    enc_ops.push(*rx as u32);
                    text_ops.push(abi_name(*rx).to_string());
                }
                (OpKind::FReg, Operand::FReg(rx)) => {
                    enc_ops.push(*rx as u32);
                    text_ops.push(abi_freg_name(*rx).to_string());
                }
                (OpKind::Imm(_), Operand::Imm(v)) if shift_imm => {
                    let max = if shamt_wide { 63 } else { 31 };
                    if !(0..=max).contains(v) {
                        return e(
                            "E-IMM",
                            format!("shift amount {v} must be between 0 and {max}"),
                            instr.source,
                        );
                    }
                    let mask = if shamt_wide { 0x3f } else { 0x1f };
                    enc_ops.push((info.funct7 << 5) | ((*v as u32) & mask));
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

    fn resolve_sym(
        &self,
        name: &str,
        pos: SourcePos,
    ) -> Result<u32, (&'static str, String, SourcePos)> {
        self.symbols.get(name).map(|s| s.addr).ok_or((
            "E-UNDEF",
            format!("undefined symbol '{name}'"),
            pos,
        ))
    }

    /// Encode one compressed statement. Symbols resolve to pc-relative byte
    /// deltas for branches and jumps, absolute values elsewhere.
    fn encode_compressed_one(
        &self,
        instr: &RawInstr,
    ) -> Result<(u16, String), (&'static str, String, SourcePos)> {
        let mut cops: Vec<crate::compressed::COp> = Vec::with_capacity(instr.ops.len());
        // Loads/stores arrive as [reg, Mem]; flatten to the encoder's order.
        let mem = match instr.ops.last() {
            Some(o @ (Operand::Mem { .. } | Operand::MemLo { .. })) => Some(o.clone()),
            _ => None,
        };
        for op in &instr.ops {
            match op {
                // Trailing Mem operands are flattened separately below.
                Operand::Mem { .. } | Operand::MemLo { .. } => {}
                Operand::Reg(r) => cops.push(crate::compressed::COp::Reg(*r)),
                Operand::Imm(v) => cops.push(crate::compressed::COp::Imm(*v)),
                Operand::Sym(name) => {
                    let target = self.symbols.get(name).map(|s| s.addr as i64).ok_or((
                        "E-UNDEF",
                        format!("undefined symbol '{name}'"),
                        instr.source,
                    ))?;
                    cops.push(crate::compressed::COp::Imm(target - instr.addr as i64));
                }
                other => {
                    return Err((
                        "E-OPERAND",
                        format!("operand not supported by compressed instructions: {other:?}"),
                        instr.source,
                    ))
                }
            }
        }
        // For a trailing Mem operand, splice base and offset into place and
        // drop the marker so the encoder sees plain register/offset lists.
        let cops = if let Some(Operand::Mem { off, base }) = mem {
            let mut v = cops;
            match instr.name {
                // c.lw rd, off(base): [rd] -> [rd, base, off].
                "c.lw" | "c.ld" => {
                    v.push(crate::compressed::COp::Reg(base));
                    v.push(crate::compressed::COp::Imm(off));
                    v
                }
                // c.sw rs2, off(base): [rs2] -> [rs2, base, off].
                "c.sw" | "c.sd" => {
                    v.push(crate::compressed::COp::Reg(base));
                    v.push(crate::compressed::COp::Imm(off));
                    v
                }
                // c.lwsp rd, off(sp): [rd] -> [rd, off].
                "c.lwsp" | "c.ldsp" => {
                    v.push(crate::compressed::COp::Imm(off));
                    v
                }
                // c.swsp rs2, off(sp): [rs2] -> [rs2, off].
                "c.swsp" | "c.sdsp" => {
                    v.push(crate::compressed::COp::Imm(off));
                    v
                }
                _ => v,
            }
        } else {
            cops
        };
        let (word, text) = crate::compressed::encode(instr.name, &cops, self.cfg.rv64)
            .map_err(|(code, msg)| (code, msg, instr.source))?;
        Ok((word, text))
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
                format!(
                    "'{}' expects 3 operand(s), found {}",
                    info.name,
                    instr.ops.len()
                ),
                instr.source,
            );
        }
        let Operand::Reg(rd) = instr.ops[0] else {
            return e(
                "E-OPERAND",
                format!("'{}' expects a destination register first", info.name),
                instr.source,
            );
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
            _ => {
                return e(
                    "E-OPERAND",
                    format!("'{}' expects a CSR name or number second", info.name),
                    instr.source,
                )
            }
        };
        let immediate_form = info.funct3 >= 5;
        let third = match (&instr.ops[2], immediate_form) {
            (Operand::Reg(r), false) => *r as u32,
            (Operand::Sym(name), false) => match reg_by_name(name) {
                Some(r) => r as u32,
                None => {
                    return e(
                        "E-OPERAND",
                        format!("'{}' expects a source register third", info.name),
                        instr.source,
                    )
                }
            },
            (Operand::Imm(v), true) if (0..=31).contains(v) => *v as u32,
            (Operand::Imm(_), true) => {
                return e(
                    "E-IMM",
                    "the immediate form of this CSR instruction takes 0 to 31".into(),
                    instr.source,
                )
            }
            _ => {
                return e(
                    "E-OPERAND",
                    format!("'{}' expects a register or immediate third", info.name),
                    instr.source,
                )
            }
        };
        let word = encode::encode(info, &[rd as u32, csr_num, third]);
        let third_text = if immediate_form {
            third.to_string()
        } else {
            abi_name(third as u8).to_string()
        };
        Ok((
            word,
            format!(
                "{} {}, 0x{:03x}, {}",
                info.name,
                abi_name(rd),
                csr_num,
                third_text
            ),
        ))
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
        match params
            .iter()
            .position(|p| p == text)
            .and_then(|k| args.get(k))
        {
            Some(arg) => {
                for a in arg {
                    out.push(Token {
                        tok: a.tok.clone(),
                        pos: t.pos,
                    });
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
        return Err(format!(
            "branch target offset {delta} exceeds the 13-bit branch range"
        ));
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
    "zero", "ra", "sp", "gp", "tp", "t0", "t1", "t2", "s0", "s1", "a0", "a1", "a2", "a3", "a4",
    "a5", "a6", "a7", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11", "t3", "t4",
    "t5", "t6",
];

/// FP ABI register names in numeric order (RISC-V calling convention:
/// ft0-ft7, fs0-fs1, fa0-fa7, fs2-fs11, ft8-ft11).
const FP_ABI: &[&str] = &[
    "ft0", "ft1", "ft2", "ft3", "ft4", "ft5", "ft6", "ft7", "fs0", "fs1", "fa0", "fa1", "fa2",
    "fa3", "fa4", "fa5", "fa6", "fa7", "fs2", "fs3", "fs4", "fs5", "fs6", "fs7", "fs8", "fs9",
    "fs10", "fs11", "ft8", "ft9", "ft10", "ft11",
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

/// Floating-point register names: numeric `f0`-`f31` plus the FP ABI aliases.
pub fn freg_by_name(name: &str) -> Option<u8> {
    if let Some(rest) = name.strip_prefix('f') {
        if let Ok(n) = rest.parse::<u8>() {
            if n < 32 {
                return Some(n);
            }
        }
    }
    FP_ABI.iter().position(|n| *n == name).map(|i| i as u8)
}

pub fn abi_name(r: u8) -> &'static str {
    ABI[r as usize]
}

/// ABI name of a floating-point register, for disassembly-style rendering.
pub fn abi_freg_name(r: u8) -> &'static str {
    FP_ABI[r as usize]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Severity;

    fn asm(src: &str) -> crate::AsmResult {
        let files = vec![InputFile {
            name: "t.s".into(),
            source: src.into(),
        }];
        crate::assemble(&files, &AsmConfig::default())
    }

    fn asm_cfg(src: &str, rv64: bool) -> crate::AsmResult {
        let files = vec![InputFile {
            name: "t.s".into(),
            source: src.into(),
        }];
        crate::assemble(
            &files,
            &AsmConfig {
                rv64,
                ..AsmConfig::default()
            },
        )
    }

    fn asm64(src: &str) -> crate::AsmResult {
        asm_cfg(src, true)
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
        let r = asm("start: beqz a0, start\n j end\nend: bnez a0, start\n");
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
    fn csr_pseudo_ops_expand() {
        let r = asm(".text
csrr a0, uscratch
csrw uscratch, t0
csrs utvec, t1
csrc uip, a2
csrrwi x0, uscratch, 5
");
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let p = r.program.unwrap();
        // csrr -> csrrs a0, uscratch, x0; csrw -> csrrw x0, uscratch, t0.
        assert_eq!(p.statements[0].basic_text.as_ref(), "csrrs a0, 0x040, zero");
        assert_eq!(p.statements[1].basic_text.as_ref(), "csrrw zero, 0x040, t0");
        assert_eq!(p.statements[4].basic_text.as_ref(), "csrrwi zero, 0x040, 5");
    }

    #[test]
    fn uret_and_wfi_encode() {
        let r = asm("uret
wfi
");
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let p = r.program.unwrap();
        assert_eq!(p.statements[0].encoding, 0x0020_0073); // uret
        assert_eq!(p.statements[1].encoding, 0x1050_0073); // wfi
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
        assert!(r
            .diagnostics
            .iter()
            .any(|d| d.code == "E-OPERAND" && d.message.contains("pair")));
    }

    #[test]
    fn macro_calls_macro() {
        let r = asm(".macro inner(%r)\n    addi %r, %r, 4\n.end_macro\n\
             .macro outer(%r)\n    inner(%r)\n    addi %r, %r, 1\n.end_macro\nouter(a0)\n");
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
        let hits = r
            .diagnostics
            .iter()
            .filter(|d| d.code == "E-MACRO-DEPTH")
            .count();
        assert_eq!(
            hits, 1,
            "depth abort should stop the chain instead of erroring per frame"
        );
    }

    #[test]
    fn unknown_macro_call_keeps_mnemonic_error() {
        let r = asm("nosuch(a0, a1)\n");
        assert!(r.has_errors());
        assert!(r
            .diagnostics
            .iter()
            .any(|d| d.code == "E-MNEMONIC" && d.message.contains("nosuch")));
    }

    #[test]
    fn eqv_inside_macro_body() {
        // .eqv applies at expansion time and sticks for later lines too.
        let r = asm(
            ".macro setv(%r)\n    .eqv VAL 9\n    li %r, VAL\n.end_macro\nsetv(a0)\nli a1, VAL\n",
        );
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
        assert!(r
            .diagnostics
            .iter()
            .any(|d| d.code == "E-DIRECTIVE" && d.message.contains(".end_macro")));
    }

    #[test]
    fn fp_instructions_assemble_with_f_registers() {
        let r = asm("fadd.s f0, f1, f2\nfadd.d ft0, ft1, ft2\nfeq.d a0, fa0, fa1\n");
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let p = r.program.unwrap();
        // fa0/fa1 are f10/f11, matching the integer a0/a1 numbers:
        // feq.d x10, f10, f11 = funct7 1010001 | rs2 01011 | rs1 01010
        assert_eq!(p.statements[0].encoding, 0x0020_8053);
        assert_eq!(p.statements[1].encoding, 0x0220_8053);
        assert_eq!(p.statements[2].encoding, 0xa2b5_2553);
        // The rendered basic text uses FP ABI names.
        assert_eq!(p.statements[0].basic_text.as_ref(), "fadd.s ft0, ft1, ft2");
    }

    #[test]
    fn fp_register_kinds_are_enforced() {
        // FP ops reject integer registers and vice versa.
        let r = asm("fadd.s f0, x1, f2\n");
        assert!(r.has_errors());
        assert!(r.diagnostics.iter().any(|d| d.code == "E-OPERAND"));
        let r = asm("add f0, x1, x2\n");
        assert!(r.has_errors());
        let r = asm("fcvt.w.s f0, f1\n"); // destination is an integer register
        assert!(r.has_errors());
        let r = asm("fmv.s.x f0, f1\n"); // source must be integer
        assert!(r.has_errors());
    }

    #[test]
    fn fp_label_form_load_and_store() {
        let r = asm(".data\nv: .float 1.5\n.text\nflw f1, v\nfsd f2, w\n.data\nw: .double 2.0\n");
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let p = r.program.unwrap();
        assert_eq!(p.statements.len(), 4); // lui+flw, lui+fsd
        assert_eq!(p.statements[1].basic_text.as_ref(), "flw ft1, %lo(v)(ra)");
        assert_eq!(p.statements[3].basic_text.as_ref(), "fsd ft2, %lo(w)(ra)");
    }

    #[test]
    fn float_double_data_bit_exact() {
        let r = asm(".data\nf: .float 1.0, -2.5, inf, -inf\n.align 3\nd: .double 1.0, nan, 42\n");
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let p = r.program.unwrap();
        let b = &p.data.bytes;
        assert_eq!(&b[0..4], &1.0f32.to_bits().to_le_bytes());
        assert_eq!(&b[4..8], &(-2.5f32).to_bits().to_le_bytes());
        assert_eq!(&b[8..12], &f32::INFINITY.to_bits().to_le_bytes());
        assert_eq!(&b[12..16], &f32::NEG_INFINITY.to_bits().to_le_bytes());
        // .double 1.0 lands at the aligned offset 16.
        assert_eq!(&b[16..24], &1.0f64.to_bits().to_le_bytes());
        assert!(f64::from_bits(u64::from_le_bytes(b[24..32].try_into().unwrap())).is_nan());
        // Integer literals are accepted as exact values (42 → 42.0).
        assert_eq!(&b[32..40], &42.0f64.to_bits().to_le_bytes());
        assert_eq!(p.symbols.get("f").unwrap().addr, 0x1001_0000);
        assert_eq!(p.symbols.get("d").unwrap().addr, 0x1001_0010);
    }

    #[test]
    fn float_outside_data_is_error() {
        let r = asm(".text\n.float 1.0\n");
        assert!(r.has_errors());
        assert!(r.diagnostics.iter().any(|d| d.code == "E-DIRECTIVE"));
    }

    #[test]
    fn fp_pseudo_ops_expand_to_sign_inject() {
        let r = asm("fabs.s f0, f1\nfneg.s f2, f3\nfmv.s f4, f5\nfabs.d f6, f7\n");
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let p = r.program.unwrap();
        // Each pseudo expands to the sign-inject op applied to the same
        // register twice, matching the canonical GNU/RARS expansions.
        let enc = |name: &str, ops: &[u32]| {
            crate::encode::encode(crate::encode::lookup(name).unwrap(), ops)
        };
        assert_eq!(p.statements[0].encoding, enc("fsgnjx.s", &[0, 1, 1])); // fabs
        assert_eq!(p.statements[1].encoding, enc("fsgnjn.s", &[2, 3, 3])); // fneg
        assert_eq!(p.statements[2].encoding, enc("fsgnj.s", &[4, 5, 5])); // fmv
        assert_eq!(p.statements[3].encoding, enc("fsgnjx.d", &[6, 7, 7]));
        // Pin the spec fields: fsgnj funct7 = 0010000(+fmt), and the
        // selector in funct3 (2 = sgnjx, 1 = sgnjn, 0 = sgnj).
        let w = p.statements[0].encoding;
        assert_eq!((w >> 25) & 0x7f, 0x10);
        assert_eq!((w >> 12) & 0x7, 2);
        assert_eq!(p.statements[1].encoding >> 12 & 0x7, 1);
        assert_eq!(p.statements[3].encoding >> 25 & 0x7f, 0x11);
        assert_eq!(p.statements[0].expanded_from.map(|s| s.line), Some(1));
    }

    // ---- RV64 mode (AsmConfig::rv64) ----

    #[test]
    fn rv64_only_instructions_are_mode_gated() {
        // RV32 (default): every RV64-only mnemonic is an E-XLEN error.
        let r = asm("ld a0, 0(sp)\n");
        assert!(r.has_errors());
        let d = &r.diagnostics[0];
        assert_eq!(d.code, "E-XLEN");
        assert!(d.message.contains("64-bit"));
        for src in [
            "sd a0, 0(sp)\n",
            "lwu a0, 0(sp)\n",
            "addiw a0, a1, 1\n",
            "addw a0, a1, a2\n",
            "mulw a0, a1, a2\n",
            "slliw a0, a1, 3\n",
            "fcvt.l.s a0, f1\n",
            "fmv.x.d a0, f1\n",
        ] {
            let r = asm(src);
            assert!(r.has_errors(), "RV32 should reject: {src}");
            assert!(
                r.diagnostics.iter().any(|d| d.code == "E-XLEN"),
                "want E-XLEN for {src}"
            );
        }
        // RV64 mode accepts all of them.
        let r = asm64("ld a0, 0(sp)\nsd a0, 8(sp)\nlwu a1, -4(sp)\naddiw a0, a1, 1\naddw a2, a1, a1\nmulw a3, a1, a1\nfcvt.l.s a0, f1\nfmv.x.d a0, f1\nfmv.d.x f1, a0\n");
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        // Everything else assembles identically in both modes.
        let r = asm64("addi a0, zero, 5\nadd a1, a0, a0\necall\n");
        assert!(!r.has_errors());
        let p = r.program.unwrap();
        assert_eq!(p.statements[0].encoding, 0x0050_0513);
    }

    #[test]
    fn rv64_load_store_and_wide_shift_encodings() {
        let r =
            asm64("ld a0, 8(sp)\nsd a0, 8(sp)\nlwu a1, -4(sp)\nslli a2, a1, 33\nsrai a3, a1, 40\n");
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let p = r.program.unwrap();
        assert_eq!(p.statements[0].encoding, 0x0081_3503); // ld a0, 8(sp)
        assert_eq!(p.statements[1].encoding, 0x00a1_3423); // sd a0, 8(sp)
        assert_eq!(p.statements[2].encoding, 0xffc1_6583); // lwu a1, -4(sp): funct3 6
        assert_eq!(p.statements[3].encoding, 0x0215_9613); // slli a2, a1, 33
        assert_eq!(p.statements[4].encoding, 0x4285_d693); // srai a3, a1, 40
        assert_eq!(p.statements[3].basic_text.as_ref(), "slli a2, a1, 33");
    }

    #[test]
    fn rv32_still_rejects_wide_shifts() {
        // The 6-bit shamt is RV64-only; RV32 keeps the old diagnostic.
        let r = asm("slli a0, a1, 33\n");
        assert!(r.has_errors());
        assert_eq!(r.diagnostics[0].code, "E-IMM");
        assert!(r.diagnostics[0].message.contains("0 and 31"));
    }

    #[test]
    fn rv64_wide_shift_validation() {
        let r = asm64("slli a0, a1, 64\n");
        assert!(r.has_errors());
        assert!(r.diagnostics[0].message.contains("0 and 63"));
        // The *iw shift immediates stay 5-bit even in RV64.
        let r = asm64("slliw a0, a1, 32\n");
        assert!(r.has_errors());
        let r = asm64("slliw a0, a1, 31\n");
        assert!(!r.has_errors());
    }

    #[test]
    fn rv64_wide_li_matches_rars_template() {
        // li t1, 1000000000000000 goes through RARS's PseudoOps-64.txt
        // 8-instruction chain:
        //   lui t1, 57; addiw t1, t1, -642; slli t1, t1, 11; addi t1, t1, 1318;
        //   slli t1, t1, 11; addi t1, t1, 416; slli t1, t1, 10; addi t1, t1, 0
        // (h = 0x38D7E, l = 0xA4C68000 per the LIA/LIB/LIC/LID/LIE formulas).
        let r = asm64("li t1, 1000000000000000\n");
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let p = r.program.unwrap();
        assert_eq!(p.statements.len(), 8);
        let want = [
            0x0003_9337, // lui t1, 57
            0xd7e3_031b, // addiw t1, t1, -642
            0x00b3_1313, // slli t1, t1, 11
            0x5263_0313, // addi t1, t1, 1318
            0x00b3_1313, // slli t1, t1, 11
            0x1a03_0313, // addi t1, t1, 416
            0x00a3_1313, // slli t1, t1, 10
            0x0003_0313, // addi t1, t1, 0
        ];
        for (i, w) in want.iter().enumerate() {
            assert_eq!(p.statements[i].encoding, *w, "statement {i}");
        }
        // A negative wide constant exercises the LIA sign compensation:
        // h = -232831 (0xFFFC7281), bit 11 clear, so LIA = h>>12 = -57
        // carried as the 20-bit 0xFFFC7 and LIB = +641.
        let r = asm64("li t1, -1000000000000000\n");
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let p = r.program.unwrap();
        assert_eq!(p.statements.len(), 8);
        assert_eq!(p.statements[0].encoding, 0xfffc_7337); // lui t1, 0xfffc7
        assert_eq!(p.statements[1].encoding, 0x2813_031b); // addiw t1, t1, 641
    }

    #[test]
    fn rv64_li_tiers() {
        // 12-bit: single addi, same as RV32.
        let r = asm64("li a0, 2047\nli a1, -2048\n");
        assert!(!r.has_errors());
        assert_eq!(r.program.unwrap().statements.len(), 2);
        // 32-bit tier: lui + addiw (PseudoOps-64.txt overrides addi with
        // addiw because "addi is not correct and addiw does not work in rv32").
        let r = asm64("li a0, 2048\nli a1, 10000000\nli a2, 2147483647\nli a3, -2147483648\n");
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let p = r.program.unwrap();
        assert_eq!(p.statements.len(), 8);
        // lui a0, 1; addiw a0, a0, -2048: 2048 = 0x800, lo sign-extends to
        // -2048 so hi compensates to 1.
        assert_eq!(p.statements[0].encoding, 0x0000_1537); // lui a0, 1
        assert_eq!(p.statements[1].encoding, 0x8005_051b); // addiw a0, a0, -2048
                                                           // 64-bit tier kicks in outside the signed 32-bit range.
        let r = asm64("li a0, 2147483648\nli a1, -2147483649\n");
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        assert_eq!(r.program.unwrap().statements.len(), 16);
    }

    #[test]
    fn rv32_li_expansion_unchanged() {
        // In RV32 the 32-bit tier keeps lui+addi (byte-identical to before).
        let r = asm("li a1, 0x12345678\n");
        assert!(!r.has_errors());
        let p = r.program.unwrap();
        assert_eq!(p.statements.len(), 2);
        assert_eq!(p.statements[0].encoding, 0x1234_55b7);
        assert_eq!(p.statements[1].encoding, 0x6785_8593);
    }

    #[test]
    fn rv64_label_form_load_uses_ld() {
        let r = asm64(".data\nv: .word 7\n.text\nld a0, v\nsd a0, v\n");
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let p = r.program.unwrap();
        assert_eq!(p.statements.len(), 4); // lui+ld, lui+sd
        assert_eq!(p.statements[1].basic_text.as_ref(), "ld a0, %lo(v)(ra)");
        assert_eq!(p.statements[3].basic_text.as_ref(), "sd a0, %lo(v)(ra)");
    }

    #[test]
    fn rv64_la_matches_rv32() {
        // Addresses still live in the low 4 GB, so la stays the 2-instruction
        // lui %hi + addi %lo pair with identical encodings in both modes.
        let src = ".data\nmsg: .asciz \"Hi\"\n.text\nla a0, msg\n";
        let p32 = asm(src).program.unwrap();
        let p64 = asm64(src).program.unwrap();
        assert_eq!(p32.statements.len(), 2);
        assert_eq!(p64.statements.len(), 2);
        for (a, b) in p32.statements.iter().zip(&p64.statements) {
            assert_eq!(a.encoding, b.encoding);
            assert_eq!(a.basic_text, b.basic_text);
        }
    }

    // ---- .extern ----

    #[test]
    fn extern_symbols_reserve_and_advance() {
        // Each reservation starts where the previous one ended, and the
        // zero-filled chunks carry the layout without touching the data
        // image.
        let r = asm(".extern a 4\n.extern b 12\n");
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let p = r.program.unwrap();
        assert_eq!(p.symbols.get("a").unwrap().addr, 0x1000_0000);
        assert_eq!(p.symbols.get("b").unwrap().addr, 0x1000_0004);
        assert_eq!(p.extern_chunks.len(), 2);
        assert_eq!(p.extern_chunks[0].0, 0x1000_0000);
        assert_eq!(p.extern_chunks[0].1, vec![0u8; 4]);
        assert_eq!(p.extern_chunks[1].0, 0x1000_0004);
        assert_eq!(p.extern_chunks[1].1, vec![0u8; 12]);
        assert!(p.data.bytes.is_empty());
    }

    #[test]
    fn extern_symbols_are_global() {
        // RARS declares extern symbols global.
        let r = asm(".extern shared 4\n");
        assert!(!r.has_errors());
        assert!(r.program.unwrap().symbols.get("shared").unwrap().global);
    }

    #[test]
    fn extern_works_from_any_segment() {
        // The extern cursor is independent of .text/.data placement and the
        // static data cursor is unaffected.
        let r = asm(".text\n.extern var 4\nnop\n.data\nx: .word 1\n");
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let p = r.program.unwrap();
        assert_eq!(p.symbols.get("var").unwrap().addr, 0x1000_0000);
        assert_eq!(p.symbols.get("x").unwrap().addr, 0x1001_0000);
        assert_eq!(p.data.bytes, 1u32.to_le_bytes().to_vec());
    }

    #[test]
    fn extern_redefine_is_duplicate_symbol_error() {
        let r = asm(".extern a 4\n.extern a 8\n");
        assert!(r.has_errors());
        assert!(r.diagnostics.iter().any(|d| d.code == "E-DUP-SYM"));
        // A label over the same name collides too.
        let r = asm(".extern a 4\na: .word 1\n");
        assert!(r.has_errors());
        assert!(r.diagnostics.iter().any(|d| d.code == "E-DUP-SYM"));
    }

    #[test]
    fn extern_size_must_be_positive() {
        for src in [
            ".extern a\n",
            ".extern a 0\n",
            ".extern a -4\n",
            ".extern 8\n",
        ] {
            let r = asm(src);
            assert!(r.has_errors(), "should reject: {src}");
            assert!(
                r.diagnostics.iter().any(|d| d.code == "E-DIRECTIVE"),
                "want E-DIRECTIVE for {src}"
            );
        }
    }

    #[test]
    fn extern_label_form_access_resolves() {
        // Programs address extern symbols like any other label: la/lw/sw
        // expand and resolve against the extern addresses.
        let r = asm(".extern var 4\n.text\nla t0, var\nlw a0, var\nsw a0, var\n");
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let p = r.program.unwrap();
        // la expands to lui+addi and each label-form access to lui+load/store.
        assert_eq!(p.statements.len(), 6);
        // lui t0, %hi(0x10000000) = 0x10000: 0x10000 << 12 | rd t0(5) << 7 | op.
        assert_eq!(p.statements[0].encoding, 0x1000_02b7);
        assert_eq!(p.statements[1].basic_text.as_ref(), "addi t0, t0, %lo(var)");
    }

    #[test]
    fn extern_base_follows_config() {
        let files = vec![InputFile {
            name: "t.s".into(),
            source: ".extern a 4\n".into(),
        }];
        let cfg = AsmConfig {
            extern_base: 0x2000_0000,
            ..AsmConfig::default()
        };
        let r = crate::assemble(&files, &cfg);
        assert!(!r.has_errors());
        let p = r.program.unwrap();
        assert_eq!(p.symbols.get("a").unwrap().addr, 0x2000_0000);
        assert_eq!(p.extern_chunks[0].0, 0x2000_0000);
    }
}

#[cfg(test)]
mod compressed_tests {
    use super::*;

    fn asm_c(src: &str) -> crate::AsmResult {
        let files = vec![InputFile {
            name: "c.s".into(),
            source: src.into(),
        }];
        crate::assemble(
            &files,
            &AsmConfig {
                allow_compressed: true,
                ..AsmConfig::default()
            },
        )
    }

    #[test]
    fn compressed_program_sizes_and_encodings() {
        let r = asm_c(
            "c.addi a0, 1\nc.li a1, 5\nc.swsp a1, 4(sp)\nc.lwsp a0, 4(sp)\nc.beqz a0, skip\nc.j skip\nc.nop\nskip:\nc.ebreak\n",
        );
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let p = r.program.unwrap();
        // Sizes: everything is 2 bytes, including the compressed c.ebreak.
        assert_eq!(p.text_end(), p.text_base + 16);
        assert_eq!(p.statements[0].size, 2);
        assert_eq!(p.statements[7].size, 2);
        // c.addi a0, 1 = 0x0505.
        assert_eq!(p.statements[0].encoding16, Some(0x0505));
        // c.j skip: forward jump over 2 bytes (c.nop) + 2 (c.ebreak) = 4.
        assert_eq!(p.statements[5].basic_text.as_ref(), "j 4");
        // Addresses progress by size: 0, 2, 4, 6, 8, 10, 12, 14.
        assert_eq!(
            p.addrs,
            [0, 2, 4, 6, 8, 10, 12, 14].map(|v| p.text_base + v)
        );
    }

    #[test]
    fn compressed_disabled_rejects() {
        let files = vec![InputFile {
            name: "c.s".into(),
            source: "c.addi a0, 1\n".into(),
        }];
        let r = crate::assemble(&files, &AsmConfig::default());
        assert!(r.has_errors());
        assert_eq!(r.diagnostics[0].code, "E-XLEN");
    }

    #[test]
    fn compressed_register_slice_enforced() {
        let r = asm_c("c.addi4spn t0, 4\n"); // a0 = x10 is not in x8..x15
        assert!(r.has_errors());
        assert_eq!(r.diagnostics[0].code, "E-OPERAND");
        // a0 (x10) is inside the slice and assembles.
        let ok = asm_c("c.addi4spn a0, 4");
        assert!(!ok.has_errors(), "diags: {:?}", ok.diagnostics);
        // c.lw requires a sliced base; sp is rejected (c.lwsp is the sp-relative form).
        let sp = asm_c("c.lw a0, 4(sp)");
        assert!(sp.has_errors());
        let ok_lw = asm_c("c.lw a0, 0(a0)");
        assert!(!ok_lw.has_errors(), "diags: {:?}", ok_lw.diagnostics);
    }
}
