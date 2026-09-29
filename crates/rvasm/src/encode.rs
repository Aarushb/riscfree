//! Instruction definitions and bit encoders for RV32I (M/F/D and RV64 land
//! on this table in later phases).

/// Operand kinds an instruction accepts, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    Reg,
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
    I,
    S,
    B,
    U,
    J,
}

#[derive(Debug, Clone, Copy)]
pub struct InstructionInfo {
    pub name: &'static str,
    pub format: Format,
    pub opcode: u32,
    pub funct3: u32,
    pub funct7: u32,
}

impl InstructionInfo {
    const fn new(name: &'static str, format: Format, opcode: u32, funct3: u32, funct7: u32) -> Self {
        InstructionInfo { name, format, opcode, funct3, funct7 }
    }
}

pub const OP: u32 = 0x33;
pub const OP_IMM: u32 = 0x13;
pub const LOAD: u32 = 0x03;
pub const STORE: u32 = 0x23;
pub const BRANCH: u32 = 0x63;
pub const JAL: u32 = 0x6f;
pub const JALR: u32 = 0x67;
pub const LUI: u32 = 0x37;
pub const AUIPC: u32 = 0x17;
pub const SYSTEM: u32 = 0x73;
pub const MISC_MEM: u32 = 0x0f;

/// RV32I base instructions keyed by mnemonic.
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
    InstructionInfo::new("fence", Format::I, MISC_MEM, 0, 0),
    InstructionInfo::new("ecall", Format::I, SYSTEM, 0, 0),
    InstructionInfo::new("ebreak", Format::I, SYSTEM, 1, 0),
];

pub fn lookup(name: &str) -> Option<&'static InstructionInfo> {
    INSTRUCTIONS.iter().find(|i| i.name == name)
}

fn enc_r(info: &InstructionInfo, rd: u32, rs1: u32, rs2: u32) -> u32 {
    (info.funct7 & 0x7f) << 25
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
        // System instructions (ecall/ebreak) and fence carry all-zero
        // register fields; fence's canonical RARS encoding sets the IORW
        // predecessor/successor bits.
        Format::I if info.name == "fence" => enc_i(info, 0, 0, 0xff),
        Format::I if ops.is_empty() => enc_i(info, 0, 0, 0),
        Format::I => enc_i(info, ops[0], ops[1], ops[2]),
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
            crate::encode::OP_IMM | crate::encode::LOAD | crate::encode::JALR | crate::encode::SYSTEM => {
                sign_extend(imm_i(w), 12)
            }
            crate::encode::STORE => sign_extend(imm_s(w), 12),
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
}
