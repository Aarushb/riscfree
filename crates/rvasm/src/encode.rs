//! Instruction definitions and bit encoders for RV32I/M/F/D plus the
//! RV64-only additions (marked `rv64_only`, accepted when `AsmConfig::rv64`
//! is set).

/// Operand kinds an instruction accepts, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    Reg,
    /// Floating-point register (f0-f31, numeric or ABI name).
    FReg,
    /// Immediate validated against `Range`.
    Imm(Range),
    /// Two's complement immediate checked to be even and in range.
    Branch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    /// 12-bit signed, e.g. I-format immediates and store offsets.
    I,
    /// 20-bit upper immediate for `lui`/`auipc`.
    U,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    R,
    /// FP single-source R-type (`fsqrt.s fd, fs1`): the fixed funct7+rs2
    /// pair packs into the I-type immediate slot at encode time.
    R2,
    /// Fused multiply-add: `fmadd fd, fs1, fs2, fs3` with rs3 in the
    /// funct7 slot and the format bit below it.
    R4,
    I,
    /// `csrrw rd, csr, rs1` family: the CSR address occupies imm[11:0] and
    /// the source register follows the CSR number in source order.
    Csr,
    S,
    B,
    U,
    J,
}

/// What an instruction does with its register operands. The assembler uses
/// this to demand FP vs integer registers per slot; UIs can use it for
/// disassembly styling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstrKind {
    /// Integer instruction (default).
    Int,
    /// `fadd.s fd, fs1, fs2` family: three FP registers.
    FpOp,
    /// `feq.s rd, fs1, fs2`: integer destination, FP sources.
    FpCmp,
    /// `fsqrt.s fd, fs1` / `fcvt.s.d fd, fs1`: two FP registers.
    FpSingle,
    /// FP source, integer destination, fixed rs2 (`fcvt.w.s`, `fmv.x.s`,
    /// `fclass.s`).
    FpToI,
    /// Integer source, FP destination, fixed rs2 (`fcvt.s.w`, `fmv.s.x`).
    FpToX,
    /// `flw`/`fld`.
    FpLoad,
    /// `fsw`/`fsd`.
    FpStore,
    /// Fused multiply-add: four FP registers.
    FpFma,
}

#[derive(Debug, Clone, Copy)]
pub struct InstructionInfo {
    pub name: &'static str,
    pub format: Format,
    pub kind: InstrKind,
    pub opcode: u32,
    pub funct3: u32,
    pub funct7: u32,
    /// Fixed rs2 field for the single-source FP forms (e.g. `fcvt.wu.s`
    /// carries rs2 = 1); unused otherwise.
    pub rs2_fixed: u8,
    /// True for instructions the base ISA only defines in 64-bit mode
    /// (`ld`/`sd`/`lwu`, the `*w` ops, the 64-bit FP conversions). The
    /// assembler rejects these with E-XLEN unless `AsmConfig::rv64` is set.
    pub rv64_only: bool,
}

impl InstructionInfo {
    const fn new(name: &'static str, format: Format, opcode: u32, funct3: u32, funct7: u32) -> Self {
        InstructionInfo { name, format, kind: InstrKind::Int, opcode, funct3, funct7, rs2_fixed: 0, rv64_only: false }
    }

    /// RV64-only integer instruction: same layout as `new`, flagged so the
    /// assembler can gate it to 64-bit mode.
    const fn new64(name: &'static str, format: Format, opcode: u32, funct3: u32, funct7: u32) -> Self {
        InstructionInfo { rv64_only: true, ..Self::new(name, format, opcode, funct3, funct7) }
    }

    /// FP instruction: `funct3` is either the operation selector or the
    /// default rounding mode baked into the encoding (RNE, or RTZ for the
    /// truncating float→int conversions, matching RARS); `funct7` carries
    /// the operation plus the format bit (0 = .s, 1 = .d).
    const fn new_fp(
        name: &'static str,
        format: Format,
        kind: InstrKind,
        opcode: u32,
        funct3: u32,
        funct7: u32,
        rs2_fixed: u8,
    ) -> Self {
        InstructionInfo { name, format, kind, opcode, funct3, funct7, rs2_fixed, rv64_only: false }
    }

    /// RV64-only FP instruction (64-bit int↔float conversions, double
    /// bit-moves), flagged so the assembler can gate it to 64-bit mode.
    const fn new_fp64(
        name: &'static str,
        format: Format,
        kind: InstrKind,
        opcode: u32,
        funct3: u32,
        funct7: u32,
        rs2_fixed: u8,
    ) -> Self {
        InstructionInfo { rv64_only: true, ..Self::new_fp(name, format, kind, opcode, funct3, funct7, rs2_fixed) }
    }
}

pub const OP: u32 = 0x33;
pub const OP_IMM: u32 = 0x13;
/// RV64 OP-IMM-32 (`addiw`, the `*iw` shift immediates).
pub const OP_IMM_32: u32 = 0x1b;
/// RV64 OP-32 (`addw`/`subw`/`sllw`/`srlw`/`sraw` and the M `w`-suffixes).
pub const OP_32: u32 = 0x3b;
pub const LOAD: u32 = 0x03;
pub const STORE: u32 = 0x23;
/// FP loads/stores use their own major opcodes, not the integer ones.
pub const LOAD_FP: u32 = 0x07;
pub const STORE_FP: u32 = 0x27;
pub const BRANCH: u32 = 0x63;
pub const JAL: u32 = 0x6f;
pub const JALR: u32 = 0x67;
pub const LUI: u32 = 0x37;
pub const AUIPC: u32 = 0x17;
pub const SYSTEM: u32 = 0x73;
pub const MISC_MEM: u32 = 0x0f;
/// FP arithmetic/comparison major opcode.
pub const FP: u32 = 0x53;
/// Fused multiply-add opcodes, distinguished by the low two major-opcode
/// bits; the format bit (.s/.d) sits in funct7[0].
pub const FMADD: u32 = 0x43;
pub const FMSUB: u32 = 0x47;
pub const FNMSUB: u32 = 0x4b;
pub const FNMADD: u32 = 0x4f;

/// RV32I base and M-extension instructions keyed by mnemonic.
pub static INSTRUCTIONS: &[InstructionInfo] = &[
    InstructionInfo::new("lui", Format::U, LUI, 0, 0),
    InstructionInfo::new("auipc", Format::U, AUIPC, 0, 0),
    InstructionInfo::new("jal", Format::J, JAL, 0, 0),
    InstructionInfo::new("jalr", Format::I, JALR, 0, 0),
    InstructionInfo::new("beq", Format::B, BRANCH, 0, 0),
    InstructionInfo::new("bne", Format::B, BRANCH, 1, 0),
    InstructionInfo::new("blt", Format::B, BRANCH, 4, 0),
    InstructionInfo::new("bge", Format::B, BRANCH, 5, 0),
    InstructionInfo::new("bltu", Format::B, BRANCH, 6, 0),
    InstructionInfo::new("bgeu", Format::B, BRANCH, 7, 0),
    InstructionInfo::new("lb", Format::I, LOAD, 0, 0),
    InstructionInfo::new("lh", Format::I, LOAD, 1, 0),
    InstructionInfo::new("lw", Format::I, LOAD, 2, 0),
    InstructionInfo::new("lbu", Format::I, LOAD, 4, 0),
    InstructionInfo::new("lhu", Format::I, LOAD, 5, 0),
    InstructionInfo::new("sb", Format::S, STORE, 0, 0),
    InstructionInfo::new("sh", Format::S, STORE, 1, 0),
    InstructionInfo::new("sw", Format::S, STORE, 2, 0),
    InstructionInfo::new("addi", Format::I, OP_IMM, 0, 0),
    InstructionInfo::new("slti", Format::I, OP_IMM, 2, 0),
    InstructionInfo::new("sltiu", Format::I, OP_IMM, 3, 0),
    InstructionInfo::new("xori", Format::I, OP_IMM, 4, 0),
    InstructionInfo::new("ori", Format::I, OP_IMM, 6, 0),
    InstructionInfo::new("andi", Format::I, OP_IMM, 7, 0),
    InstructionInfo::new("slli", Format::I, OP_IMM, 1, 0x00),
    InstructionInfo::new("srli", Format::I, OP_IMM, 5, 0x00),
    InstructionInfo::new("srai", Format::I, OP_IMM, 5, 0x20),
    InstructionInfo::new("add", Format::R, OP, 0, 0x00),
    InstructionInfo::new("sub", Format::R, OP, 0, 0x20),
    InstructionInfo::new("sll", Format::R, OP, 1, 0x00),
    InstructionInfo::new("slt", Format::R, OP, 2, 0x00),
    InstructionInfo::new("sltu", Format::R, OP, 3, 0x00),
    InstructionInfo::new("xor", Format::R, OP, 4, 0x00),
    InstructionInfo::new("srl", Format::R, OP, 5, 0x00),
    InstructionInfo::new("sra", Format::R, OP, 5, 0x20),
    InstructionInfo::new("or", Format::R, OP, 6, 0x00),
    InstructionInfo::new("and", Format::R, OP, 7, 0x00),
    // RV32M: same R layout, distinguished by funct7 0x01.
    InstructionInfo::new("mul", Format::R, OP, 0, 0x01),
    InstructionInfo::new("mulh", Format::R, OP, 1, 0x01),
    InstructionInfo::new("mulhsu", Format::R, OP, 2, 0x01),
    InstructionInfo::new("mulhu", Format::R, OP, 3, 0x01),
    InstructionInfo::new("div", Format::R, OP, 4, 0x01),
    InstructionInfo::new("divu", Format::R, OP, 5, 0x01),
    InstructionInfo::new("rem", Format::R, OP, 6, 0x01),
    InstructionInfo::new("remu", Format::R, OP, 7, 0x01),
    InstructionInfo::new("fence", Format::I, MISC_MEM, 0, 0),
    InstructionInfo::new("ecall", Format::I, SYSTEM, 0, 0),
    InstructionInfo::new("ebreak", Format::I, SYSTEM, 0, 0),
    InstructionInfo::new("csrrw", Format::Csr, SYSTEM, 1, 0),
    InstructionInfo::new("csrrs", Format::Csr, SYSTEM, 2, 0),
    InstructionInfo::new("csrrc", Format::Csr, SYSTEM, 3, 0),
    InstructionInfo::new("csrrwi", Format::Csr, SYSTEM, 5, 0),
    InstructionInfo::new("csrrsi", Format::Csr, SYSTEM, 6, 0),
    InstructionInfo::new("csrrci", Format::Csr, SYSTEM, 7, 0),
    // ---- FP loads/stores (word and doubleword) ----
    InstructionInfo::new_fp("flw", Format::I, InstrKind::FpLoad, LOAD_FP, 2, 0, 0),
    InstructionInfo::new_fp("fsw", Format::S, InstrKind::FpStore, STORE_FP, 2, 0, 0),
    InstructionInfo::new_fp("fld", Format::I, InstrKind::FpLoad, LOAD_FP, 3, 0, 0),
    InstructionInfo::new_fp("fsd", Format::S, InstrKind::FpStore, STORE_FP, 3, 0, 0),
    // ---- Fused multiply-add (R4): funct3 bakes RNE, funct7 the format bit ----
    InstructionInfo::new_fp("fmadd.s", Format::R4, InstrKind::FpFma, FMADD, 0, 0x00, 0),
    InstructionInfo::new_fp("fmsub.s", Format::R4, InstrKind::FpFma, FMSUB, 0, 0x00, 0),
    InstructionInfo::new_fp("fnmsub.s", Format::R4, InstrKind::FpFma, FNMSUB, 0, 0x00, 0),
    InstructionInfo::new_fp("fnmadd.s", Format::R4, InstrKind::FpFma, FNMADD, 0, 0x00, 0),
    InstructionInfo::new_fp("fmadd.d", Format::R4, InstrKind::FpFma, FMADD, 0, 0x01, 0),
    InstructionInfo::new_fp("fmsub.d", Format::R4, InstrKind::FpFma, FMSUB, 0, 0x01, 0),
    InstructionInfo::new_fp("fnmsub.d", Format::R4, InstrKind::FpFma, FNMSUB, 0, 0x01, 0),
    InstructionInfo::new_fp("fnmadd.d", Format::R4, InstrKind::FpFma, FNMADD, 0, 0x01, 0),
    // ---- F arithmetic (funct7 = op<<1 | 0, funct3 bakes RNE or the
    // selector for the sign-inject/min-max groups) ----
    InstructionInfo::new_fp("fadd.s", Format::R, InstrKind::FpOp, FP, 0, 0x00, 0),
    InstructionInfo::new_fp("fsub.s", Format::R, InstrKind::FpOp, FP, 0, 0x04, 0),
    InstructionInfo::new_fp("fmul.s", Format::R, InstrKind::FpOp, FP, 0, 0x08, 0),
    InstructionInfo::new_fp("fdiv.s", Format::R, InstrKind::FpOp, FP, 0, 0x0c, 0),
    InstructionInfo::new_fp("fsgnj.s", Format::R, InstrKind::FpOp, FP, 0, 0x10, 0),
    InstructionInfo::new_fp("fsgnjn.s", Format::R, InstrKind::FpOp, FP, 1, 0x10, 0),
    InstructionInfo::new_fp("fsgnjx.s", Format::R, InstrKind::FpOp, FP, 2, 0x10, 0),
    InstructionInfo::new_fp("fmin.s", Format::R, InstrKind::FpOp, FP, 0, 0x14, 0),
    InstructionInfo::new_fp("fmax.s", Format::R, InstrKind::FpOp, FP, 1, 0x14, 0),
    InstructionInfo::new_fp("fsqrt.s", Format::R2, InstrKind::FpSingle, FP, 0, 0x2c, 0),
    // Truncating float→int conversions bake RTZ in the rm field, as RARS does.
    InstructionInfo::new_fp("fcvt.w.s", Format::R2, InstrKind::FpToI, FP, 1, 0x60, 0),
    InstructionInfo::new_fp("fcvt.wu.s", Format::R2, InstrKind::FpToI, FP, 1, 0x60, 1),
    InstructionInfo::new_fp("fmv.x.s", Format::R2, InstrKind::FpToI, FP, 0, 0x70, 0),
    InstructionInfo::new_fp("fclass.s", Format::R2, InstrKind::FpToI, FP, 1, 0x70, 0),
    InstructionInfo::new_fp("fcvt.s.w", Format::R2, InstrKind::FpToX, FP, 0, 0x68, 0),
    InstructionInfo::new_fp("fcvt.s.wu", Format::R2, InstrKind::FpToX, FP, 0, 0x68, 1),
    InstructionInfo::new_fp("fmv.s.x", Format::R2, InstrKind::FpToX, FP, 0, 0x78, 0),
    InstructionInfo::new_fp("feq.s", Format::R, InstrKind::FpCmp, FP, 2, 0x50, 0),
    InstructionInfo::new_fp("flt.s", Format::R, InstrKind::FpCmp, FP, 1, 0x50, 0),
    InstructionInfo::new_fp("fle.s", Format::R, InstrKind::FpCmp, FP, 0, 0x50, 0),
    // ---- D arithmetic (funct7 = op<<1 | 1) ----
    InstructionInfo::new_fp("fadd.d", Format::R, InstrKind::FpOp, FP, 0, 0x01, 0),
    InstructionInfo::new_fp("fsub.d", Format::R, InstrKind::FpOp, FP, 0, 0x05, 0),
    InstructionInfo::new_fp("fmul.d", Format::R, InstrKind::FpOp, FP, 0, 0x09, 0),
    InstructionInfo::new_fp("fdiv.d", Format::R, InstrKind::FpOp, FP, 0, 0x0d, 0),
    InstructionInfo::new_fp("fsgnj.d", Format::R, InstrKind::FpOp, FP, 0, 0x11, 0),
    InstructionInfo::new_fp("fsgnjn.d", Format::R, InstrKind::FpOp, FP, 1, 0x11, 0),
    InstructionInfo::new_fp("fsgnjx.d", Format::R, InstrKind::FpOp, FP, 2, 0x11, 0),
    InstructionInfo::new_fp("fmin.d", Format::R, InstrKind::FpOp, FP, 0, 0x15, 0),
    InstructionInfo::new_fp("fmax.d", Format::R, InstrKind::FpOp, FP, 1, 0x15, 0),
    InstructionInfo::new_fp("fsqrt.d", Format::R2, InstrKind::FpSingle, FP, 0, 0x2d, 0),
    InstructionInfo::new_fp("fcvt.w.d", Format::R2, InstrKind::FpToI, FP, 1, 0x61, 0),
    InstructionInfo::new_fp("fcvt.wu.d", Format::R2, InstrKind::FpToI, FP, 1, 0x61, 1),
    InstructionInfo::new_fp("fclass.d", Format::R2, InstrKind::FpToI, FP, 1, 0x71, 0),
    InstructionInfo::new_fp("fcvt.d.w", Format::R2, InstrKind::FpToX, FP, 0, 0x69, 0),
    InstructionInfo::new_fp("fcvt.d.wu", Format::R2, InstrKind::FpToX, FP, 0, 0x69, 1),
    InstructionInfo::new_fp("feq.d", Format::R, InstrKind::FpCmp, FP, 2, 0x51, 0),
    InstructionInfo::new_fp("flt.d", Format::R, InstrKind::FpCmp, FP, 1, 0x51, 0),
    InstructionInfo::new_fp("fle.d", Format::R, InstrKind::FpCmp, FP, 0, 0x51, 0),
    // ---- Precision conversions ----
    InstructionInfo::new_fp("fcvt.s.d", Format::R2, InstrKind::FpSingle, FP, 0, 0x20, 0),
    InstructionInfo::new_fp("fcvt.d.s", Format::R2, InstrKind::FpSingle, FP, 0, 0x21, 0),
    // ---- RV64-only instructions (gated by AsmConfig::rv64) ----
    // Doubleword loads/stores and the unsigned word load.
    InstructionInfo::new64("ld", Format::I, LOAD, 3, 0),
    InstructionInfo::new64("lwu", Format::I, LOAD, 6, 0),
    InstructionInfo::new64("sd", Format::S, STORE, 3, 0),
    // Word-width arithmetic: compute the low 32 bits, sign-extend to 64.
    // OP-IMM-32/OP-32 are their own major opcodes (0x1b / 0x3b).
    InstructionInfo::new64("addiw", Format::I, OP_IMM_32, 0, 0),
    InstructionInfo::new64("slliw", Format::I, OP_IMM_32, 1, 0x00),
    InstructionInfo::new64("srliw", Format::I, OP_IMM_32, 5, 0x00),
    InstructionInfo::new64("sraiw", Format::I, OP_IMM_32, 5, 0x20),
    InstructionInfo::new64("addw", Format::R, OP_32, 0, 0x00),
    InstructionInfo::new64("subw", Format::R, OP_32, 0, 0x20),
    InstructionInfo::new64("sllw", Format::R, OP_32, 1, 0x00),
    InstructionInfo::new64("srlw", Format::R, OP_32, 5, 0x00),
    InstructionInfo::new64("sraw", Format::R, OP_32, 5, 0x20),
    // M-extension w-suffixes: same R layout, funct7 0x01 on OP-32.
    InstructionInfo::new64("mulw", Format::R, OP_32, 0, 0x01),
    InstructionInfo::new64("divw", Format::R, OP_32, 4, 0x01),
    InstructionInfo::new64("divuw", Format::R, OP_32, 5, 0x01),
    InstructionInfo::new64("remw", Format::R, OP_32, 6, 0x01),
    InstructionInfo::new64("remuw", Format::R, OP_32, 7, 0x01),
    // 64-bit FP conversions: rs2 = 2 selects the signed 64-bit form and
    // rs2 = 3 the unsigned one; float→int keeps RTZ, int→float RNE.
    InstructionInfo::new_fp64("fcvt.l.s", Format::R2, InstrKind::FpToI, FP, 1, 0x60, 2),
    InstructionInfo::new_fp64("fcvt.lu.s", Format::R2, InstrKind::FpToI, FP, 1, 0x60, 3),
    InstructionInfo::new_fp64("fcvt.l.d", Format::R2, InstrKind::FpToI, FP, 1, 0x61, 2),
    InstructionInfo::new_fp64("fcvt.lu.d", Format::R2, InstrKind::FpToI, FP, 1, 0x61, 3),
    InstructionInfo::new_fp64("fcvt.s.l", Format::R2, InstrKind::FpToX, FP, 0, 0x68, 2),
    InstructionInfo::new_fp64("fcvt.s.lu", Format::R2, InstrKind::FpToX, FP, 0, 0x68, 3),
    InstructionInfo::new_fp64("fcvt.d.l", Format::R2, InstrKind::FpToX, FP, 0, 0x69, 2),
    InstructionInfo::new_fp64("fcvt.d.lu", Format::R2, InstrKind::FpToX, FP, 0, 0x69, 3),
    // Double bit-moves (funct7 carries the .d format bit).
    InstructionInfo::new_fp64("fmv.x.d", Format::R2, InstrKind::FpToI, FP, 0, 0x71, 0),
    InstructionInfo::new_fp64("fmv.d.x", Format::R2, InstrKind::FpToX, FP, 0, 0x79, 0),
];

pub fn lookup(name: &str) -> Option<&'static InstructionInfo> {
    INSTRUCTIONS.iter().find(|i| i.name == name)
}

/// A representative mnemonic for an opcode field value, for tools that group
/// execution counts by opcode (several instructions share one opcode).
pub fn opcode_representative(opcode: u32) -> Option<&'static str> {
    INSTRUCTIONS
        .iter()
        .find(|i| i.opcode == opcode && !i.rv64_only)
        .map(|i| i.name)
}

/// Per-field decode breakdown of one instruction word, for the disassembly
/// tools: every field the formats use, plus which base format it decodes as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedFields {
    pub opcode: u32,
    pub rd: u32,
    pub funct3: u32,
    pub rs1: u32,
    pub rs2: u32,
    pub funct7: u32,
    /// Immediate field for I/S/B/U/J formats, raw (unshifted for U).
    pub immediate: i32,
    /// One of "R", "I", "S", "B", "U", "J" — the closest base format.
    pub format: &'static str,
}

pub fn decode_fields(word: u32) -> DecodedFields {
    let opcode = word & 0x7f;
    let rd = (word >> 7) & 0x1f;
    let funct3 = (word >> 12) & 0x7;
    let rs1 = (word >> 15) & 0x1f;
    let rs2 = (word >> 20) & 0x1f;
    let funct7 = (word >> 25) & 0x7f;
    let (immediate, format) = match opcode {
        OP_IMM | LOAD | JALR | SYSTEM => (decode::imm_for(word), "I"),
        STORE => (decode::imm_for(word), "S"),
        BRANCH => (decode::imm_for(word), "B"),
        LUI | AUIPC => ((word & 0xffff_f000) as i32, "U"),
        JAL => (decode::imm_for(word), "J"),
        _ => (0, "R"),
    };
    DecodedFields { opcode, rd, funct3, rs1, rs2, funct7, immediate, format }
}

fn enc_r(info: &InstructionInfo, rd: u32, rs1: u32, rs2: u32) -> u32 {
    (info.funct7 & 0x7f) << 25
        | (rs2 & 0x1f) << 20
        | (rs1 & 0x1f) << 15
        | (info.funct3 & 0x7) << 12
        | (rd & 0x1f) << 7
        | info.opcode
}

/// FMA layout: rs3 rides above rs2 with the format bit (funct7[0]) wedged
/// between them at bit 25.
fn enc_r4(info: &InstructionInfo, rd: u32, rs1: u32, rs2: u32, rs3: u32) -> u32 {
    (rs3 & 0x1f) << 27
        | (info.funct7 & 0x3) << 25
        | (rs2 & 0x1f) << 20
        | (rs1 & 0x1f) << 15
        | (info.funct3 & 0x7) << 12
        | (rd & 0x1f) << 7
        | info.opcode
}

fn enc_i(info: &InstructionInfo, rd: u32, rs1: u32, imm: u32) -> u32 {
    (imm & 0xfff) << 20
        | (rs1 & 0x1f) << 15
        | (info.funct3 & 0x7) << 12
        | (rd & 0x1f) << 7
        | info.opcode
}

fn enc_s(info: &InstructionInfo, rs1: u32, rs2: u32, imm: u32) -> u32 {
    ((imm >> 5) & 0x7f) << 25
        | (rs2 & 0x1f) << 20
        | (rs1 & 0x1f) << 15
        | (info.funct3 & 0x7) << 12
        | (imm & 0x1f) << 7
        | info.opcode
}

fn enc_b(info: &InstructionInfo, rs1: u32, rs2: u32, imm: u32) -> u32 {
    // imm is the byte offset; bit 0 is always zero.
    ((imm >> 12) & 0x1) << 31
        | ((imm >> 5) & 0x3f) << 25
        | (rs2 & 0x1f) << 20
        | (rs1 & 0x1f) << 15
        | (info.funct3 & 0x7) << 12
        | ((imm >> 1) & 0xf) << 8
        | ((imm >> 11) & 0x1) << 7
        | info.opcode
}

fn enc_u(info: &InstructionInfo, rd: u32, imm20: u32) -> u32 {
    (imm20 & 0xfffff) << 12 | (rd & 0x1f) << 7 | info.opcode
}

fn enc_j(info: &InstructionInfo, rd: u32, imm: u32) -> u32 {
    ((imm >> 20) & 0x1) << 31
        | ((imm >> 1) & 0x3ff) << 21
        | ((imm >> 11) & 0x1) << 20
        | ((imm >> 12) & 0xff) << 12
        | (rd & 0x1f) << 7
        | info.opcode
}

/// Encode one instruction. Offsets for B/J are absolute byte deltas
/// (target - pc); the caller computes them.
pub fn encode(info: &InstructionInfo, ops: &[u32]) -> u32 {
    match info.format {
        Format::R => enc_r(info, ops[0], ops[1], ops[2]),
        // Single-source FP forms reuse the I-type layout: the fixed
        // funct7+rs2 pair fills imm[11:0] exactly as slli's shift field does.
        Format::R2 => enc_i(info, ops[0], ops[1], ((info.funct7 & 0x7f) << 5) | (info.rs2_fixed & 0x1f) as u32),
        Format::R4 => enc_r4(info, ops[0], ops[1], ops[2], ops[3]),
        // ecall/ebreak are identified by their immediate field (0 or 1);
        // fence's canonical RARS encoding sets the IORW bits.
        Format::I if info.name == "fence" => enc_i(info, 0, 0, 0xff),
        Format::I if ops.is_empty() => {
            let imm = u32::from(info.name == "ebreak");
            enc_i(info, 0, 0, imm)
        }
        Format::I => enc_i(info, ops[0], ops[1], ops[2]),
        Format::Csr => enc_i(info, ops[0], ops[2], ops[1]),
        Format::S => enc_s(info, ops[0], ops[1], ops[2]),
        Format::B => enc_b(info, ops[0], ops[1], ops[2]),
        Format::U => enc_u(info, ops[0], ops[1]),
        Format::J => enc_j(info, ops[0], ops[1]),
    }
}

/// Sign-extend the immediate field of a decoded word. Used by the
/// disassembler and machine views; exercised via round-trip tests.
#[allow(dead_code)]
pub mod decode {
    fn imm_i(w: u32) -> u32 {
        w >> 20
    }
    fn imm_s(w: u32) -> u32 {
        ((w >> 25) << 5) | ((w >> 7) & 0x1f)
    }
    fn imm_b(w: u32) -> u32 {
        (((w >> 31) & 0x1) << 12)
            | (((w >> 7) & 0x1) << 11)
            | (((w >> 25) & 0x3f) << 5)
            | (((w >> 8) & 0xf) << 1)
    }
    fn imm_j(w: u32) -> u32 {
        (((w >> 31) & 0x1) << 20)
            | (((w >> 12) & 0xff) << 12)
            | (((w >> 20) & 0x1) << 11)
            | (((w >> 21) & 0x3ff) << 1)
    }

    /// Sign-extend a value of `bits` width.
    pub fn sign_extend(val: u32, bits: u32) -> i32 {
        let shift = 32 - bits;
        ((val << shift) as i32) >> shift
    }

    pub fn imm_for(w: u32) -> i32 {
        let opcode = w & 0x7f;
        match opcode {
            crate::encode::OP_IMM
            | crate::encode::LOAD
            | crate::encode::LOAD_FP
            | crate::encode::JALR
            | crate::encode::SYSTEM => sign_extend(imm_i(w), 12),
            crate::encode::STORE | crate::encode::STORE_FP => sign_extend(imm_s(w), 12),
            crate::encode::BRANCH => sign_extend(imm_b(w), 13),
            crate::encode::LUI | crate::encode::AUIPC => (w & 0xffff_f000) as i32,
            crate::encode::JAL => sign_extend(imm_j(w), 21),
            _ => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(name: &str, ops: &[u32]) -> u32 {
        encode(lookup(name).unwrap(), ops)
    }

    #[test]
    fn golden_encodings() {
        // addi x1, x0, 5
        assert_eq!(enc("addi", &[1, 0, 5]), 0x0050_0093);
        // add x3, x1, x2
        assert_eq!(enc("add", &[3, 1, 2]), 0x0020_81b3);
        // sub x1, x2, x3
        assert_eq!(enc("sub", &[1, 2, 3]), 0x4031_00b3);
        // jal ra, +8
        assert_eq!(enc("jal", &[1, 8]), 0x0080_00ef);
        // jal x0, -4
        assert_eq!(enc("jal", &[0, 0xffff_fffcu32]), 0xffdf_f06f);
        // beq x0, x0, +8: imm[4:1] = 4 lands in instr[11:8]
        assert_eq!(enc("beq", &[0, 0, 8]), 0x0000_0463);
        // bne a0, a1, -4
        assert_eq!(enc("bne", &[10, 11, 0xffff_fffcu32]), 0xfeb5_1ee3);
        // lui a0, 0x12345
        assert_eq!(enc("lui", &[10, 0x12345]), 0x1234_5537);
        // sw x5, 8(x2)
        assert_eq!(enc("sw", &[2, 5, 8]), 0x0051_2423);
        // lw x6, -4(x2)
        assert_eq!(enc("lw", &[6, 2, 0xffff_fffcu32]), 0xffc1_2303);
        // ecall
        assert_eq!(enc("ecall", &[0, 0, 0]), 0x0000_0073);
        // srai x1, x2, 3: raw encode takes the full shifted immediate,
        // funct7 in imm[11:5]
        assert_eq!(enc("srai", &[1, 2, 0x403]), 0x4031_5093);
    }

    #[test]
    fn m_extension_encodings() {
        // mul a0, a1, a2: funct7 0x01 fills instr[31:25]
        assert_eq!(enc("mul", &[10, 11, 12]), 0x02c5_8533);
        assert_eq!(enc("mulh", &[10, 11, 12]), 0x02c5_9533);
        assert_eq!(enc("mulhsu", &[10, 11, 12]), 0x02c5_a533);
        assert_eq!(enc("mulhu", &[10, 11, 12]), 0x02c5_b533);
        // div a1, a2, a3: funct3 4 selects signed division
        assert_eq!(enc("div", &[11, 12, 13]), 0x02d6_45b3);
        assert_eq!(enc("divu", &[11, 12, 13]), 0x02d6_55b3);
        // rem a2, a3, a4
        assert_eq!(enc("rem", &[12, 13, 14]), 0x02e6_e633);
        assert_eq!(enc("remu", &[12, 13, 14]), 0x02e6_f633);
    }

    /// FP register numbers for the golden tests: f0/f1/f2/f3/f4.
    #[test]
    fn f_extension_encodings() {
        // fadd.s f0, f1, f2: funct7 0000000, rm (funct3) 000 = RNE.
        assert_eq!(enc("fadd.s", &[0, 1, 2]), 0x0020_8053);
        assert_eq!(enc("fadd.d", &[0, 1, 2]), 0x0220_8053);
        // fsub.s f1, f2, f3: funct7 0000100.
        assert_eq!(enc("fsub.s", &[1, 2, 3]), 0x0831_00d3);
        assert_eq!(enc("fmul.d", &[1, 2, 3]), 0x1231_00d3);
        assert_eq!(enc("fdiv.d", &[1, 2, 3]), 0x1a31_00d3);
        // Sign-inject and min/max use funct3 as the operation selector.
        assert_eq!(enc("fsgnjn.s", &[3, 1, 2]), 0x2020_91d3);
        assert_eq!(enc("fsgnjx.d", &[3, 1, 2]), 0x2220_a1d3);
        assert_eq!(enc("fmin.s", &[3, 1, 2]), 0x2820_81d3);
        assert_eq!(enc("fmax.d", &[3, 1, 2]), 0x2a20_91d3);
        // Single-source forms: the fixed funct7+rs2 pair rides in imm[11:0].
        // fsqrt.s f3, f1: funct7 0101100, rs2 00000, rm 000.
        assert_eq!(enc("fsqrt.s", &[3, 1]), 0x5800_81d3);
        assert_eq!(enc("fsqrt.d", &[3, 1]), 0x5a00_81d3);
        // fcvt.w.s a0, f1: funct7 1100000, rs2 0, rm 001 = RTZ.
        assert_eq!(enc("fcvt.w.s", &[10, 1]), 0xc000_9553);
        // fcvt.wu.s a0, f1: same funct7 with rs2 = 1.
        assert_eq!(enc("fcvt.wu.s", &[10, 1]), 0xc010_9553);
        assert_eq!(enc("fcvt.w.d", &[10, 1]), 0xc200_9553);
        assert_eq!(enc("fcvt.wu.d", &[10, 1]), 0xc210_9553);
        // fmv.x.s a0, f1 and fclass.s a0, f1: funct7 1110000, funct3 0/1.
        assert_eq!(enc("fmv.x.s", &[10, 1]), 0xe000_8553);
        assert_eq!(enc("fclass.s", &[10, 1]), 0xe000_9553);
        assert_eq!(enc("fclass.d", &[10, 1]), 0xe200_9553);
        // Int→FP forms: fcvt.s.w f1, a0 (funct7 1101000) and fmv.s.x f1, a0
        // (funct7 1111000, integer source in rs1).
        assert_eq!(enc("fcvt.s.w", &[1, 10]), 0xd005_00d3);
        assert_eq!(enc("fcvt.s.wu", &[1, 10]), 0xd015_00d3);
        assert_eq!(enc("fcvt.d.w", &[1, 10]), 0xd005_00d3 | 0x0200_0000);
        assert_eq!(enc("fmv.s.x", &[1, 10]), 0xf005_00d3);
        // Comparisons write an integer register.
        assert_eq!(enc("feq.s", &[10, 1, 2]), 0xa020_a553);
        assert_eq!(enc("flt.s", &[10, 1, 2]), 0xa020_9553);
        assert_eq!(enc("fle.d", &[10, 1, 2]), 0xa220_8553);
        // Precision conversions between .s and .d.
        assert_eq!(enc("fcvt.s.d", &[3, 1]), 0x4000_81d3);
        assert_eq!(enc("fcvt.d.s", &[3, 1]), 0x4200_81d3);
    }

    #[test]
    fn fma_and_fp_load_store_encodings() {
        // fmadd.s f3, f1, f2, f4: rs3 in bits 31:27, format bit 0 at bit 25.
        assert_eq!(enc("fmadd.s", &[3, 1, 2, 4]), 0x2020_81c3);
        assert_eq!(enc("fmsub.s", &[3, 1, 2, 4]), 0x2020_81c7);
        assert_eq!(enc("fnmsub.s", &[3, 1, 2, 4]), 0x2020_81cb);
        // fnmadd.d: opcode 1001111 plus format bit 1 at bit 25.
        assert_eq!(enc("fnmadd.d", &[3, 1, 2, 4]), 0x2220_81cf);
        // FP loads/stores use the LOAD-FP/STORE-FP opcodes (0x07/0x27).
        assert_eq!(enc("flw", &[1, 2, 8]), 0x0081_2087); // flw f1, 8(f2)
        assert_eq!(enc("fsw", &[2, 1, 8]), 0x0011_2427); // fsw f1, 8(f2)
        assert_eq!(enc("fld", &[1, 2, 8]), 0x0081_3087);
        assert_eq!(enc("fsd", &[2, 1, 8]), 0x0011_3427);
    }

    #[test]
    fn decode_roundtrip() {
        let w = enc("addi", &[1, 0, 5]);
        assert_eq!(decode::imm_for(w), 5);
        let w = enc("addi", &[1, 0, 0xffff_fffcu32]);
        assert_eq!(decode::imm_for(w), -4);
        let w = enc("bne", &[10, 11, 0xffff_fffcu32]);
        assert_eq!(decode::imm_for(w), -4);
        let w = enc("jal", &[0, 2046]);
        assert_eq!(decode::imm_for(w), 2046);
    }

    #[test]
    fn rv64_load_store_encodings() {
        // ld a0, 8(sp): LOAD funct3 3
        assert_eq!(enc("ld", &[10, 2, 8]), 0x0081_3503);
        // lwu a0, 8(sp): LOAD funct3 6 (zero-extending word load)
        assert_eq!(enc("lwu", &[10, 2, 8]), 0x0081_6503);
        // sd a0, 8(sp): STORE funct3 3
        assert_eq!(enc("sd", &[2, 10, 8]), 0x00a1_3423);
    }

    #[test]
    fn rv64_word_op_encodings() {
        // addiw a0, a1, 5: OP-IMM-32 (0x1b) funct3 0
        assert_eq!(enc("addiw", &[10, 11, 5]), 0x0055_851b);
        // addw a1, a2, a3: OP-32 (0x3b) funct7 0x00
        assert_eq!(enc("addw", &[11, 12, 13]), 0x00d6_05bb);
        assert_eq!(enc("subw", &[11, 12, 13]), 0x40d6_05bb);
        assert_eq!(enc("sllw", &[11, 12, 13]), 0x00d6_15bb);
        assert_eq!(enc("srlw", &[11, 12, 13]), 0x00d6_55bb);
        assert_eq!(enc("sraw", &[11, 12, 13]), 0x40d6_55bb);
        // M w-suffixes: funct7 0x01 on OP-32.
        assert_eq!(enc("mulw", &[11, 12, 13]), 0x02d6_05bb);
        assert_eq!(enc("divw", &[11, 12, 13]), 0x02d6_45bb);
        assert_eq!(enc("divuw", &[11, 12, 13]), 0x02d6_55bb);
        assert_eq!(enc("remw", &[11, 12, 13]), 0x02d6_65bb);
        assert_eq!(enc("remuw", &[11, 12, 13]), 0x02d6_75bb);
    }

    #[test]
    fn rv64_wide_shift_immediates() {
        // Like the 32-bit goldens, the raw encoder takes the full imm12:
        // funct7 in imm[11:5] (imm[11:6] in RV64) plus the shamt.
        // slli a0, a1, 33: shamt rides in imm[5:0] in RV64
        assert_eq!(enc("slli", &[10, 11, 0x021]), 0x0215_9513);
        // srai a0, a1, 40: funct6 010000 in imm[11:6], shamt[5] set
        assert_eq!(enc("srai", &[10, 11, 0x428]), 0x4285_d513);
        assert_eq!(enc("srli", &[10, 11, 0x03f]), 0x03f5_d513);
        // The *iw shift immediates stay 5-bit even in RV64.
        assert_eq!(enc("slliw", &[10, 11, 0x01f]), 0x01f5_951b);
        assert_eq!(enc("sraiw", &[10, 11, 0x403]), 0x4035_d51b);
    }

    #[test]
    fn rv64_fp_conversion_encodings() {
        // fcvt.l.s a0, f1: funct7 1100000, rs2 2, rm RTZ.
        assert_eq!(enc("fcvt.l.s", &[10, 1]), 0xc020_9553);
        assert_eq!(enc("fcvt.lu.s", &[10, 1]), 0xc030_9553);
        assert_eq!(enc("fcvt.l.d", &[10, 1]), 0xc220_9553);
        assert_eq!(enc("fcvt.lu.d", &[10, 1]), 0xc230_9553);
        // Int→float forms take RNE like their 32-bit siblings.
        assert_eq!(enc("fcvt.s.l", &[1, 10]), 0xd025_00d3);
        assert_eq!(enc("fcvt.s.lu", &[1, 10]), 0xd035_00d3);
        assert_eq!(enc("fcvt.d.l", &[1, 10]), 0xd225_00d3);
        assert_eq!(enc("fcvt.d.lu", &[1, 10]), 0xd235_00d3);
        // Double bit-moves: funct7 1110001 / 1111001 carry the format bit.
        assert_eq!(enc("fmv.x.d", &[10, 1]), 0xe200_8553);
        assert_eq!(enc("fmv.d.x", &[1, 10]), 0xf205_00d3);
    }
}
