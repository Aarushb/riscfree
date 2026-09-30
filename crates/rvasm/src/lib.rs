//! rvasm: the AsAccess RISC-V assembler.
//!
//! Assembles RARS-style RISC-V assembly source into a [`Program`]: text
//! statements with encodings and source spans, a data image, and a symbol
//! table. Diagnostics carry file, line, and column so UIs can navigate to
//! the offending source and read the message aloud.

mod asm;
pub mod compressed;
mod encode;
mod lexer;

pub use compressed::COp;
pub use encode::{decode_fields, opcode_representative, DecodedFields, InstructionInfo};

use std::collections::BTreeMap;
use std::sync::Arc;

pub type FileId = usize;

/// One-based line and zero-based column of a token in its source file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourcePos {
    pub file: FileId,
    pub line: u32,
    pub col: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: &'static str,
    pub message: String,
    pub pos: SourcePos,
}

impl Diagnostic {
    fn error(code: &'static str, message: impl Into<String>, pos: SourcePos) -> Self {
        Diagnostic {
            severity: Severity::Error,
            code,
            message: message.into(),
            pos,
        }
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

/// One assembled instruction placed in the text segment.
#[derive(Debug, Clone)]
pub struct Statement {
    pub addr: u32,
    pub encoding: u32,
    /// The pseudo-instruction source line this came from, if it was expanded.
    pub expanded_from: Option<SourcePos>,
    /// Source position of the instruction itself (or of the pseudo-op).
    pub source: SourcePos,
    /// Rendered basic-instruction text, e.g. `addi a0, zero, 55`.
    pub basic_text: Arc<str>,
    /// Bytes in this instruction: 4, or 2 when the compressed set is enabled.
    pub size: u32,
    /// The 16-bit halfword for compressed instructions.
    pub encoding16: Option<u16>,
}

/// Assembled static data, contiguous from `base`.
#[derive(Debug, Clone, Default)]
pub struct DataImage {
    pub base: u32,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct Symbol {
    pub addr: u32,
    pub global: bool,
    pub source: SourcePos,
}

/// Per-file plus global symbols, keyed by name. Addresses are also indexed
/// for the labels view and narration.
#[derive(Debug, Clone, Default)]
pub struct SymbolTable {
    by_name: BTreeMap<String, Symbol>,
    by_addr: BTreeMap<u32, String>,
}

impl SymbolTable {
    pub fn get(&self, name: &str) -> Option<&Symbol> {
        self.by_name.get(name)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut Symbol> {
        self.by_name.get_mut(name)
    }

    pub fn name_at(&self, addr: u32) -> Option<&str> {
        self.by_addr.get(&addr).map(String::as_str)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Symbol)> {
        self.by_name.iter()
    }

    fn define(&mut self, name: String, sym: Symbol, diags: &mut Vec<Diagnostic>) {
        if self.by_name.contains_key(&name) {
            diags.push(Diagnostic::error(
                "E-DUP-SYM",
                format!("symbol '{name}' is already defined"),
                sym.source,
            ));
            return;
        }
        self.by_addr.insert(sym.addr, name.clone());
        self.by_name.insert(name, sym);
    }
}

/// A fully assembled program ready to load into [`rvm`].
#[derive(Debug, Clone)]
pub struct Program {
    /// First text address (RARS default layout: 0x00400000).
    pub text_base: u32,
    /// Text statements in address order; each statement sits at its own
    /// `addr` (4-byte instructions predominate; the compressed set adds
    /// 2-byte instructions).
    pub statements: Vec<Statement>,
    pub data: DataImage,
    pub symbols: SymbolTable,
    /// `.extern name size` reservations as (address, zero bytes). An extern
    /// symbol is a label over zeroed memory, not an initializer: no bytes are
    /// emitted for it beyond this zero fill, which rvm writes at load so the
    /// reserved region exists in the image like RARS's extern segment.
    pub extern_chunks: Vec<(u32, Vec<u8>)>,
    /// Per-statement text addresses, in statement order (binary-search
    /// support for `statement_at` with variable instruction sizes).
    pub addrs: Vec<u32>,
    /// First address past the last statement.
    pub text_end: u32,
    /// Original source lines, indexed by `FileId` then line (0-based).
    pub sources: Vec<Arc<str>>,
    pub file_names: Vec<String>,
}

impl Program {
    pub fn statement_at(&self, addr: u32) -> Option<&Statement> {
        self.addrs
            .binary_search(&addr)
            .ok()
            .and_then(|i| self.statements.get(i))
    }

    pub fn text_end(&self) -> u32 {
        self.text_end
    }
}

#[derive(Debug, Clone)]
pub struct AsmConfig {
    pub text_base: u32,
    pub data_base: u32,
    /// Base of the extern region where `.extern name size` reserves space.
    /// RARS puts the extern segment at the data-segment base, just below the
    /// static data this config's `data_base` points at.
    pub extern_base: u32,
    /// When false, pseudo-instructions are rejected like RARS's `np` flag.
    pub allow_pseudo: bool,
    /// Assemble for RV64 (RARS's 64-bit setting; the default is RV32, as in
    /// RARS). In 64-bit mode the RV64-only instructions are accepted (`ld`,
    /// `sd`, `lwu`, the `*w` word ops, 6-bit base-shift immediates, and the
    /// 64-bit FP conversions), `li` widens to the RARS 64-bit templates, and
    /// pseudo-op semantics match `PseudoOps-64.txt`. Register/memory layout
    /// is unchanged.
    pub rv64: bool,
    /// Accept the C (compressed) 16-bit instruction set. Off by default,
    /// matching RARS which has no compressed support at all.
    pub allow_compressed: bool,
}

impl Default for AsmConfig {
    fn default() -> Self {
        AsmConfig {
            text_base: 0x0040_0000,
            data_base: 0x1001_0000,
            extern_base: 0x1000_0000,
            allow_pseudo: true,
            rv64: false,
            allow_compressed: false,
        }
    }
}

pub struct InputFile {
    pub name: String,
    pub source: String,
}

pub struct AsmResult {
    /// Present when assembly produced a loadable program. Statements are
    /// emitted best-effort even when errors occurred, so partial views work;
    /// check `has_errors` before running.
    pub program: Option<Program>,
    pub diagnostics: Vec<Diagnostic>,
}

impl AsmResult {
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(Diagnostic::is_error)
    }
}

/// Assemble one or more files. Symbols are visible across files.
pub fn assemble(files: &[InputFile], cfg: &AsmConfig) -> AsmResult {
    crate::asm::assemble_impl(files, cfg)
}
