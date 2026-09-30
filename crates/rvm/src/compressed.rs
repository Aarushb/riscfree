//! C (compressed) instruction expansion: a 16-bit word maps to its 32-bit
//! equivalent, which then flows through the normal decode path. Layouts
//! follow the RISC-V C chapter; register slices x8-x15 decode with +8.

/// Extract `word[msb:lsb]` (inclusive).
fn bits(word: u16, msb: u32, lsb: u32) -> u32 {
    let width = msb - lsb + 1;
    ((word >> lsb) as u32) & ((1 << width) - 1)
}

/// The 3-bit compressed register slice x8-x15.
fn creg3(word: u16, shift: u32) -> u32 {
    bits(word, shift + 2, shift) + 8
}

fn sext_n(v: u32, bits: u32) -> i32 {
    let shift = 32 - bits;
    ((v << shift) as i32) >> shift
}

// Offset-bit scrambles live in rvasm::compressed — the expander unpacks
// through the same table the assembler packs with, so the two sides cannot
// drift apart.
use rvasm::compressed::{
    ADDI4SPN_OFFSET, LDSP_OFFSET, LD_SD_OFFSET, LWSP_OFFSET, LW_SW_OFFSET, SDSP_OFFSET, SWSP_OFFSET,
};

fn unpack_offset(table: &[(u32, u32)], word: u16) -> u32 {
    let mut out = 0u32;
    for (bit, pos) in table {
        out |= (((word >> pos) & 1) as u32) << bit;
    }
    out
}

fn jal(rd: u32, offset: u32) -> u32 {
    0x6f | (rd << 7)
        | (((offset >> 1) & 0x3ff) << 21)
        | (((offset >> 11) & 1) << 20)
        | (((offset >> 12) & 0xff) << 12)
        | (((offset >> 20) & 1) << 31)
}

fn i_type(funct3: u32, rd: u32, rs1: u32, imm: u32) -> u32 {
    0x13 | (funct3 << 12) | (rs1 << 15) | (rd << 7) | ((imm & 0xfff) << 20)
}

fn branch(rs1: u32, offset: u32, funct3: u32) -> u32 {
    0x63 | (funct3 << 12)
        | (rs1 << 15)
        | (((offset >> 12) & 1) << 31)
        | (((offset >> 5) & 0x3f) << 25)
        | (((offset >> 1) & 0xf) << 8)
        | (((offset >> 11) & 1) << 7)
}

/// CJ-type immediate extraction: instruction bits [12:2] hold offset bits
/// {11, 4, 9:8, 10, 6, 7, 3:1, 5}; the offset's low bit is implied zero.
fn cj_offset(word: u16) -> u32 {
    (bits(word, 12, 12) << 11)
        | (bits(word, 11, 11) << 4)
        | (bits(word, 10, 10) << 9)
        | (bits(word, 9, 9) << 8)
        | (bits(word, 8, 8) << 10)
        | (bits(word, 7, 7) << 6)
        | (bits(word, 6, 6) << 7)
        | (bits(word, 5, 3) << 1)
        | (bits(word, 2, 2) << 5)
}

/// CB-type (c.beqz/c.bnez) immediate: offset bits {8, 4:3} at [12:10],
/// {7:6} at [6:5], {2:1} at [4:3], {5} at [2].
fn cb_offset(word: u16) -> u32 {
    (bits(word, 12, 12) << 8)
        | (bits(word, 11, 10) << 3)
        | (bits(word, 6, 5) << 6)
        | (bits(word, 4, 3) << 1)
        | (bits(word, 2, 2) << 5)
}

/// Expand a 16-bit compressed instruction to its 32-bit equivalent, or None
/// for reserved encodings. `rv64` selects between the RV32-only and RV64-only
/// spellings that share an encoding slot (c.jal vs c.addiw, c.lw vs c.ld...).
pub fn expand_compressed(word: u16, rv64: bool) -> Option<u32> {
    let q = word & 0x3;
    match q {
        // Quadrant 0: loads and stores with an x8-x15 base.
        0x0 => match bits(word, 15, 13) {
            0x0 => {
                // c.addi4spn: addi rd', x2, nzuimm
                let nzuimm = unpack_offset(ADDI4SPN_OFFSET, word);
                if nzuimm == 0 {
                    return None;
                }
                let rd = creg3(word, 2);
                Some(0x13 | (nzuimm << 20) | (2 << 15) | (rd << 7))
            }
            0x2 => {
                // c.lw
                let offset = unpack_offset(LW_SW_OFFSET, word);
                let word32 = 0x03
                    | (2 << 12)
                    | (creg3(word, 9) << 15)
                    | (creg3(word, 2) << 7)
                    | (((offset >> 3) & 7) << 23)
                    | (((offset >> 2) & 1) << 22)
                    | (((offset >> 6) & 1) << 26);
                Some(word32)
            }
            0x6 => {
                // c.sw
                let offset = unpack_offset(LW_SW_OFFSET, word);
                let word32 = 0x23
                    | (2 << 12)
                    | (creg3(word, 9) << 15)
                    | (creg3(word, 2) << 20)
                    | (((offset >> 3) & 7) << 23)
                    | (((offset >> 2) & 1) << 9)
                    | (((offset >> 6) & 1) << 26);
                Some(word32)
            }
            0x3 if rv64 => {
                // c.ld
                let offset = unpack_offset(LD_SD_OFFSET, word);
                let word32 = 0x03
                    | (3 << 12)
                    | (creg3(word, 9) << 15)
                    | (creg3(word, 2) << 7)
                    | (((offset >> 3) & 7) << 22)
                    | (((offset >> 6) & 3) << 24);
                Some(word32)
            }
            0x7 if rv64 => {
                // c.sd
                let offset = unpack_offset(LD_SD_OFFSET, word);
                let word32 = 0x23
                    | (3 << 12)
                    | (creg3(word, 9) << 15)
                    | (creg3(word, 2) << 20)
                    | (((offset >> 3) & 7) << 25)
                    | (((offset >> 6) & 3) << 26);
                Some(word32)
            }
            _ => None,
        },
        // Quadrant 1: arithmetic, branches, jump.
        0x1 => {
            let rd = bits(word, 11, 7);
            let imm6 = (bits(word, 12, 12) << 5) | bits(word, 6, 2);
            match bits(word, 15, 13) {
                0x0 => {
                    // c.addi (c.nop when rd = x0)
                    let imm = sext_n(imm6, 6);
                    if rd == 0 {
                        return Some(0x13);
                    }
                    Some(i_type(0x0, rd, rd, imm as u32))
                }
                0x1 if rv64 => {
                    // c.addiw
                    let imm = sext_n(imm6, 6);
                    Some(0x1b | ((imm as u32 & 0xfff) << 20) | (rd << 15) | (rd << 7))
                }
                0x1 => {
                    // c.jal (RV32)
                    Some(jal(1, cj_offset(word)))
                }
                0x2 => {
                    // c.li
                    let imm = sext_n(imm6, 6);
                    if rd == 0 {
                        return None;
                    }
                    Some(i_type(0x0, rd, 0, imm as u32))
                }
                0x3 => {
                    if rd == 2 {
                        // c.addi16sp
                        let imm10 = (bits(word, 12, 12) << 9)
                            | (bits(word, 6, 6) << 6)
                            | (bits(word, 5, 5) << 5)
                            | (bits(word, 4, 3) << 7)
                            | (bits(word, 2, 2) << 4);
                        let imm = sext_n(imm10, 10);
                        if imm == 0 {
                            return None;
                        }
                        Some(i_type(0x0, 2, 2, imm as u32))
                    } else {
                        // c.lui: nzimm[17] at bit 12, nzimm[16:12] at [6:2].
                        let imm17 = (bits(word, 12, 12) << 5) | bits(word, 6, 2);
                        let imm = sext_n(imm17, 6);
                        if imm == 0 {
                            return None;
                        }
                        let value = (imm as u32) & 0xfffff;
                        Some(0x37 | (rd << 7) | (value << 12))
                    }
                }
                0x4 => {
                    // In this group rd' sits at [9:7]; bits [11:10] are the
                    // opcode selector, so they must not be folded into rd.
                    let rd = bits(word, 9, 7) + 8;
                    match bits(word, 11, 10) {
                        0x0 | 0x1 => {
                            // c.srli / c.srai: shamt at [12]+[6:2].
                            let shamt = (bits(word, 12, 12) << 5) | bits(word, 6, 2);
                            if shamt == 0 {
                                return None;
                            }
                            let funct7 = if bits(word, 11, 10) == 0x1 {
                                0x20
                            } else {
                                0x00
                            };
                            Some(
                                0x13 | (funct7 << 25)
                                    | (shamt << 20)
                                    | (5 << 12)
                                    | (rd << 15)
                                    | (rd << 7),
                            )
                        }
                        0x2 => {
                            // c.andi
                            let imm = sext_n((bits(word, 12, 12) << 5) | bits(word, 6, 2), 6);
                            Some(i_type(0x7, rd, rd, imm as u32))
                        }
                        _ => {
                            // c.sub/c.xor/c.or/c.and: funct2 at [6:5], rs2'
                            // at [4:2]. On RV64 bit12=1 selects c.subw/c.addw
                            // (word-width variants, opcode 0x3b).
                            let rs2 = bits(word, 4, 2) + 8;
                            let funct2 = bits(word, 6, 5);
                            if bits(word, 12, 12) == 1 {
                                let funct7 = if funct2 == 0 { 0x20 } else { 0x00 };
                                return Some(
                                    0x3b | (funct7 << 25)
                                        | (rs2 << 20)
                                        | (rd << 15)
                                        | (rd << 7),
                                );
                            }
                            let (funct7, funct3) = match funct2 {
                                0x0 => (0x20, 0x0),
                                0x1 => (0x00, 0x4),
                                0x2 => (0x00, 0x6),
                                _ => (0x00, 0x7),
                            };
                            Some(
                                0x33 | (funct7 << 25)
                                    | (rs2 << 20)
                                    | (rd << 15)
                                    | (funct3 << 12)
                                    | (rd << 7),
                            )
                        }
                    }
                }
                0x5 => Some(jal(0, cj_offset(word))),
                0x6 | 0x7 => {
                    let rs1 = bits(word, 9, 7) + 8;
                    let offset = cb_offset(word);
                    let funct3 = if bits(word, 15, 13) == 0x6 { 0x0 } else { 0x1 };
                    Some(branch(rs1, offset, funct3))
                }
                _ => None,
            }
        }
        // Quadrant 2: sp-relative loads/stores, register ops, jumps.
        0x2 => {
            let rd = bits(word, 11, 7);
            let rs1 = bits(word, 11, 7);
            let prefix = bits(word, 15, 13);
            let bit12 = bits(word, 12, 12);
            let rs2 = bits(word, 6, 2);
            match prefix {
                0x0 => {
                    // c.slli
                    let shamt = (bit12 << 5) | bits(word, 6, 2);
                    if rd == 0 || shamt == 0 {
                        return None;
                    }
                    Some(0x13 | (1 << 12) | (shamt << 20) | (rd << 15) | (rd << 7))
                }
                0x2 => {
                    // c.lwsp
                    let off = unpack_offset(LWSP_OFFSET, word);
                    if rd == 0 || off == 0 {
                        return None;
                    }
                    Some(
                        0x03 | (2 << 12)
                            | (2 << 15)
                            | (rd << 7)
                            | (((off >> 5) & 1) << 25)
                            | (((off >> 2) & 7) << 22)
                            | (((off >> 6) & 3) << 26),
                    )
                }
                0x3 if rv64 => {
                    // c.ldsp
                    let off = unpack_offset(LDSP_OFFSET, word);
                    if rd == 0 || off == 0 {
                        return None;
                    }
                    Some(
                        0x03 | (3 << 12)
                            | (2 << 15)
                            | (rd << 7)
                            | (((off >> 5) & 1) << 25)
                            | (((off >> 2) & 7) << 22)
                            | (((off >> 8) & 3) << 27),
                    )
                }
                0x4 if bit12 == 0 => {
                    // The [11:7] field is rs1 here (shared with the mv/rd
                    // slot). c.jr needs rs1 != 0; c.mv needs rd != 0.
                    if rs2 == 0 {
                        // c.jr rs1
                        let rs1 = bits(word, 11, 7);
                        if rs1 == 0 {
                            return None;
                        }
                        Some(0x67 | (rs1 << 15))
                    } else {
                        // c.mv rd, rs2 -> add rd, x0, rs2 (register move);
                        // the rs2 field here spans all 32 registers.
                        if rd == 0 || rs2 == 0 {
                            return None;
                        }
                        Some(0x33 | (rs2 << 20) | (rd << 7))
                    }
                }
                0x4 if bit12 != 0 => {
                    // bit12 = 1: c.ebreak (all register fields zero),
                    // c.jalr (rs1 at [11:7], ra implied), or c.add. x0 pairs
                    // are hints and decode as reserved here.
                    if rs1 == 0 && rs2 == 0 && rd == 0 {
                        return Some(0x00100073); // c.ebreak
                    }
                    if rs2 == 0 {
                        // c.jalr ra, 0(rs1)
                        if rs1 == 0 {
                            return None;
                        }
                        return Some(0x67 | (rs1 << 15) | (1 << 7));
                    }
                    if rd == 0 || rs2 == 0 {
                        return None; // hint encodings
                    }
                    Some(0x33 | (rs2 << 20) | (rd << 15) | (rd << 7))
                }
                0x6 => {
                    // c.swsp
                    let off = unpack_offset(SWSP_OFFSET, word);
                    if rs2 == 0 || off == 0 {
                        return None;
                    }
                    // S-type split: imm[4:0]@[11:7], imm[11:5]@[31:25].
                    Some(
                        0x23 | (2 << 12)
                            | (2 << 15)
                            | (rs2 << 20)
                            | (((off >> 2) & 0x7) << 9)
                            | (((off >> 5) & 1) << 25)
                            | (((off >> 6) & 3) << 26),
                    )
                }
                0x7 if rv64 => {
                    // c.sdsp
                    let off = unpack_offset(SDSP_OFFSET, word);
                    if rs2 == 0 || off == 0 {
                        return None;
                    }
                    Some(
                        0x23 | (3 << 12)
                            | (2 << 15)
                            | (rs2 << 20)
                            | (((off >> 3) & 7) << 23)
                            | (((off >> 6) & 7) << 26),
                    )
                }
                _ => None,
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_golden_expansions() {
        // c.ebreak = 0x9002 expands to ebreak 0x00100073.
        assert_eq!(expand_compressed(0x9002, false), Some(0x00100073));
        // c.ret (c.jr ra) = 0x8082 -> jalr x0, 0(ra) = 0x00008067.
        assert_eq!(expand_compressed(0x8082, false), Some(0x00008067));
        // c.nop = 0x0001 -> addi x0, x0, 0 = 0x00000013.
        assert_eq!(expand_compressed(0x0001, false), Some(0x00000013));
        // c.mv a0, a1 = 0x852e -> addi a0, x0, a1.
        assert_eq!(expand_compressed(0x852e, false), Some(0x00b00533));
    }
}

#[cfg(test)]
mod debug_probe {
    use super::*;
    #[test]
    fn probe() {
        println!("jr ra = {:?}", expand_compressed(0x8082, false));
        println!("mv a0,a1 = {:?}", expand_compressed(0x852e, false));
        println!("ebreak = {:?}", expand_compressed(0x9002, false));
    }
}

#[cfg(test)]
mod probe2 {
    use super::*;
    #[test]
    fn probe_swsp() {
        let r = expand_compressed(0xc22a, false);
        println!("swsp expand: {r:?}");
        assert!(r.is_some());
    }
}
