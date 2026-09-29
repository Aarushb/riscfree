//! Fetch-decode-execute for RV32I plus the M extension. The machine's write
//! helpers record undo entries and change events, so this file stays a plain
//! decode `match`.

use crate::csr;
use crate::fp;
use crate::{Change, Event, Halt, Machine, StepOutcome};

pub(crate) struct ExecOutcome {
    pub outcome: StepOutcome,
    /// True when the machine must stop stepping (halt already recorded).
    pub terminated_now: bool,
}

impl ExecOutcome {
    fn ok(changes: Vec<Change>, events: Vec<Event>, pc_before: u32) -> Self {
        ExecOutcome {
            outcome: StepOutcome { executed: true, pc_before, events, changes },
            terminated_now: false,
        }
    }

    fn halt(h: Halt, events: Vec<Event>, pc_before: u32) -> Self {
        let mut evs = events;
        evs.push(Event::Halted(h.clone()));
        ExecOutcome {
            outcome: StepOutcome { executed: false, pc_before, events: evs, changes: Vec::new() },
            terminated_now: true,
        }
    }
}

fn sx(v: u32, bits: u32) -> u64 {
    // Sign-extend the low `bits` of v to 64 bits.
    let shift = 32 - bits;
    (((v << shift) as i32) >> shift) as i64 as u64
}

fn imm_i(w: u32) -> u64 {
    sx(w >> 20, 12)
}

fn imm_s(w: u32) -> u64 {
    sx(((w >> 25) << 5) | ((w >> 7) & 0x1f), 12)
}

fn imm_b(w: u32) -> u64 {
    sx(
        (((w >> 31) & 0x1) << 12) | (((w >> 7) & 0x1) << 11) | (((w >> 25) & 0x3f) << 5) | (((w >> 8) & 0xf) << 1),
        13,
    )
}

fn imm_u(w: u32) -> u64 {
    (w & 0xffff_f000) as u64
}

fn imm_j(w: u32) -> u64 {
    sx(
        (((w >> 31) & 0x1) << 20)
            | (((w >> 12) & 0xff) << 12)
            | (((w >> 20) & 0x1) << 11)
            | (((w >> 21) & 0x3ff) << 1),
        21,
    )
}

fn shamt(w: u32) -> u64 {
    ((w >> 20) & 0x1f) as u64
}

fn rd(w: u32) -> usize {
    ((w >> 7) & 0x1f) as usize
}

fn rs1(w: u32) -> usize {
    ((w >> 15) & 0x1f) as usize
}

fn rs2(w: u32) -> usize {
    ((w >> 20) & 0x1f) as usize
}

/// Effective rounding mode: rm field 7 (DYN) reads the frm CSR. None marks
/// the reserved 5/6 encodings.
fn fp_rm(m: &Machine, f3: u32) -> Option<u8> {
    fp::resolve_rm(f3, m.read_csr(csr::FRM))
}

pub(crate) fn execute(m: &mut Machine, w: u32, pc_before: u32) -> ExecOutcome {
    let mut changes = Vec::new();
    let mut events = Vec::new();
    let opcode = w & 0x7f;

    macro_rules! bail {
        ($msg:expr) => {
            return ExecOutcome::halt(Halt::Error { message: $msg }, events, pc_before)
        };
    }

    match opcode {
        0x37 => {
            // lui
            let v = imm_u(w);
            m.write_reg(rd(w), v, &mut changes);
        }
        0x17 => {
            // auipc
            let v = (pc_before as u64).wrapping_add(imm_u(w));
            m.write_reg(rd(w), v, &mut changes);
        }
        0x6f => {
            // jal
            let link = (pc_before + 4) as u64;
            let target = (pc_before as u64).wrapping_add(imm_j(w)) as u32;
            m.write_reg(rd(w), link, &mut changes);
            m.pc = target;
        }
        0x67 => {
            // jalr
            let link = (pc_before + 4) as u64;
            let target = (m.regs[rs1(w)].wrapping_add(imm_i(w)) as u32) & !1;
            m.write_reg(rd(w), link, &mut changes);
            m.pc = target;
        }
        0x63 => {
            // branches
            let a = m.regs[rs1(w)] as u32 as i32;
            let b = m.regs[rs2(w)] as u32 as i32;
            let au = m.regs[rs1(w)] as u32;
            let bu = m.regs[rs2(w)] as u32;
            let taken = match (w >> 12) & 0x7 {
                0 => a == b,
                1 => a != b,
                4 => a < b,
                5 => a >= b,
                6 => au < bu,
                7 => au >= bu,
                f => bail!(format!("invalid branch funct3 {f}")),
            };
            if taken {
                m.pc = (pc_before as u64).wrapping_add(imm_b(w)) as u32;
            }
        }
        0x03 => {
            // loads
            let addr = (m.regs[rs1(w)] as u32).wrapping_add(imm_i(w) as u32);
            let loaded = match (w >> 12) & 0x7 {
                0 => m.load_signed(addr, 1, &mut changes),
                1 => m.load_signed(addr, 2, &mut changes),
                2 => m.load_signed(addr, 4, &mut changes),
                4 => m.load_bytes(addr, 1, &mut changes),
                5 => m.load_bytes(addr, 2, &mut changes),
                f => bail!(format!("invalid load funct3 {f}")),
            };
            match loaded {
                Ok(v) => m.write_reg(rd(w), v, &mut changes),
                Err(e) => bail!(e.to_string()),
            }
        }
        0x23 => {
            // stores
            let addr = (m.regs[rs1(w)] as u32).wrapping_add(imm_s(w) as u32);
            let val = m.regs[rs2(w)];
            let r = match (w >> 12) & 0x7 {
                0 => m.store_bytes(addr, val, 1, &mut changes),
                1 => m.store_bytes(addr, val, 2, &mut changes),
                2 => m.store_bytes(addr, val, 4, &mut changes),
                f => bail!(format!("invalid store funct3 {f}")),
            };
            if let Err(e) = r {
                bail!(e.to_string());
            }
        }
        0x13 => {
            // op-imm
            let a = m.regs[rs1(w)];
            let imm = imm_i(w);
            let v = match (w >> 12) & 0x7 {
                0 => a.wrapping_add(imm),
                2 => ((a as u32 as i32) < (imm as u32 as i32)) as u64,
                3 => ((a as u32) < (imm as u32)) as u64,
                4 => a ^ imm,
                6 => a | imm,
                7 => a & imm,
                1 => a << shamt(w), // slli
                5 => {
                    if (w >> 25) & 0x20 != 0 {
                        ((a as i64) >> shamt(w)) as u64 // srai
                    } else {
                        a >> shamt(w) // srli
                    }
                }
                _ => unreachable!(),
            };
            m.write_reg(rd(w), v, &mut changes);
        }
        0x33 => {
            // op
            let a = m.regs[rs1(w)];
            let b = m.regs[rs2(w)];
            let v = match ((w >> 12) & 0x7, (w >> 25) & 0x3f) {
                (0, 0x00) => a.wrapping_add(b),           // add
                (0, 0x20) => a.wrapping_sub(b),           // sub
                (1, 0x00) => a << (b & 0x1f),             // sll
                (2, 0x00) => ((a as u32 as i32) < (b as u32 as i32)) as u64, // slt
                (3, 0x00) => ((a as u32) < (b as u32)) as u64,               // sltu
                (4, 0x00) => a ^ b,                       // xor
                (5, 0x00) => a >> (b & 0x1f),             // srl
                (5, 0x20) => ((a as i64) >> (b & 0x1f)) as u64, // sra
                (6, 0x00) => a | b,                       // or
                (7, 0x00) => a & b,                       // and
                // RV32M: results are 32-bit and stored sign-extended.
                (0, 0x01) => (a as u32 as i32).wrapping_mul(b as u32 as i32) as i64 as u64, // mul
                // The signed products fit i64 exactly, so >> 32 is the
                // exact upper half (arithmetic for mulh/mulhsu).
                (1, 0x01) => (((a as u32 as i32 as i64) * (b as u32 as i32 as i64)) >> 32) as u64, // mulh
                (2, 0x01) => (((a as u32 as i32 as i64) * (b as u32 as i64)) >> 32) as u64, // mulhsu
                (3, 0x01) => ((a as u32 as u64) * (b as u32 as u64)) >> 32, // mulhu
                // div/rem follow RISC-V's no-trap rules: divide by zero
                // yields -1 / the dividend, and MIN/-1 overflow yields
                // MIN / 0 instead of trapping.
                (4, 0x01) => {
                    let (x, y) = (a as u32 as i32, b as u32 as i32);
                    let v: i32 = if y == 0 {
                        -1
                    } else if x == i32::MIN && y == -1 {
                        i32::MIN
                    } else {
                        x / y
                    };
                    v as i64 as u64
                }
                (5, 0x01) => {
                    let (x, y) = (a as u32, b as u32);
                    // Divide by zero yields all ones instead of trapping.
                    let v: u32 = x.checked_div(y).unwrap_or(u32::MAX);
                    v as i32 as i64 as u64
                }
                (6, 0x01) => {
                    let (x, y) = (a as u32 as i32, b as u32 as i32);
                    let v: i32 = if y == 0 {
                        x
                    } else if x == i32::MIN && y == -1 {
                        0
                    } else {
                        x % y
                    };
                    v as i64 as u64
                }
                (7, 0x01) => {
                    let (x, y) = (a as u32, b as u32);
                    let v: u32 = if y == 0 { x } else { x % y };
                    v as i32 as i64 as u64
                }
                (f, f7) => bail!(format!("invalid op funct3 {f} funct7 {f7}")),
            };
            m.write_reg(rd(w), v, &mut changes);
        }
        0x0f => {
            // fence: no-op in this sequential model, matching RARS.
        }
        0x07 => {
            // FP loads: flw moves the word and NaN-boxes it into the
            // register; fld moves the full doubleword.
            let addr = (m.regs[rs1(w)] as u32).wrapping_add(imm_i(w) as u32);
            let width = match (w >> 12) & 0x7 {
                2 => 4u32,
                3 => 8,
                f => bail!(format!("invalid FP load funct3 {f}")),
            };
            match m.load_bytes(addr, width, &mut changes) {
                Ok(v) => {
                    let boxed = if width == 4 { fp::box_single(v as u32) } else { v };
                    m.write_freg(rd(w), boxed, &mut changes);
                }
                Err(e) => bail!(e.to_string()),
            }
        }
        0x27 => {
            // FP stores: fsw writes the low word, fsd the doubleword.
            let addr = (m.regs[rs1(w)] as u32).wrapping_add(imm_s(w) as u32);
            let width = match (w >> 12) & 0x7 {
                2 => 4u32,
                3 => 8,
                f => bail!(format!("invalid FP store funct3 {f}")),
            };
            if let Err(e) = m.store_bytes(addr, m.fregs[rs2(w)], width, &mut changes) {
                bail!(e.to_string());
            }
        }
        0x53 => {
            if let Err(msg) = fp_op(m, w, &mut changes) {
                bail!(msg);
            }
        }
        0x43 | 0x47 | 0x4b | 0x4f => {
            if let Err(msg) = fp_fma(m, w, &mut changes) {
                bail!(msg);
            }
        }
        0x73 => {
            let funct3 = (w >> 12) & 0x7;
            if funct3 == 0 {
                let imm12 = (w >> 20) & 0xfff;
                if imm12 == 0 {
                    // ecall
                    let step = crate::syscalls::dispatch(m, &mut events, pc_before);
                    let terminated_now = step.events.iter().any(|e| matches!(e, Event::Halted(_)));
                    return ExecOutcome { outcome: step, terminated_now };
                } else if imm12 == 1 {
                    m.terminated = Some(Halt::Ebreak);
                    return ExecOutcome::halt(Halt::Ebreak, events, pc_before);
                }
                bail!("invalid system instruction".to_string());
            }
            // Zicsr
            let csr_id = ((w >> 20) & 0xfff) as u16;
            let old = m.read_csr(csr_id);
            let write_val = match funct3 {
                1 => Some(m.regs[rs1(w)]),                       // csrrw
                2 => (m.regs[rs1(w)] != 0).then(|| m.regs[rs1(w)]), // csrrs
                3 => (m.regs[rs1(w)] != 0).then(|| !m.regs[rs1(w)]), // csrrc
                5 => Some(imm_i(w) as u32 as u64),               // csrrwi
                6 => (imm_i(w) != 0).then_some(imm_i(w) as u32 as u64), // csrrsi
                7 => (imm_i(w) != 0).then_some(!(imm_i(w) as u32 as u64)), // csrrci
                _ => bail!("invalid csr instruction".to_string()),
            };
            m.write_reg(rd(w), old, &mut changes);
            if let Some(val) = write_val {
                m.write_csr_raw(csr_id, val, &mut changes);
            }
        }
        other => bail!(format!(
            "unknown instruction 0x{w:08x} (opcode 0x{other:02x}) at 0x{pc_before:08x}"
        )),
    }

    ExecOutcome::ok(changes, events, pc_before)
}

/// Execute one FP instruction (opcode 0x53). The funct7 field is
/// `operation << 1 | format` with format 1 meaning double precision; funct3
/// is either a rounding mode or an operation selector depending on the
/// group. Returns Err with the halt message on illegal encodings.
fn fp_op(m: &mut Machine, w: u32, changes: &mut Vec<Change>) -> Result<(), String> {
    let f7 = (w >> 25) & 0x7f;
    let f3 = (w >> 12) & 0x7;
    let dbl = f7 & 1 == 1;
    let rd = rd(w);
    let group = f7 >> 1;
    match group {
        // fadd/fsub/fmul/fdiv: funct3 is the rounding mode.
        0 | 2 | 4 | 6 => {
            let Some(rm) = fp_rm(m, f3) else {
                return Err(format!("invalid rounding mode {f3}"));
            };
            let op = match group {
                0 => fp::BinOp::Add,
                2 => fp::BinOp::Sub,
                4 => fp::BinOp::Mul,
                _ => fp::BinOp::Div,
            };
            let a = m.fregs[rs1(w)];
            let b = m.fregs[rs2(w)];
            let (val, flags) = if dbl {
                fp::double_bin(op, a, b, rm)
            } else {
                let (v, fl) = fp::single_bin(op, fp::single_bits(a), fp::single_bits(b), rm);
                (fp::box_single(v), fl)
            };
            m.write_freg(rd, val, changes);
            m.acc_fflags(flags, changes);
        }
        // fsgnj/fsgnjn/fsgnjx: funct3 selects, no flags.
        8 => {
            if f3 > 2 {
                return Err(format!("invalid fsgnj funct3 {f3}"));
            }
            let a = m.fregs[rs1(w)];
            let b = m.fregs[rs2(w)];
            let val = if dbl {
                fp::sign_inject64(a, b, f3)
            } else {
                fp::box_single(fp::sign_inject32(fp::single_bits(a), fp::single_bits(b), f3))
            };
            m.write_freg(rd, val, changes);
        }
        // fmin/fmax: funct3 selects, no flags.
        10 => {
            if f3 > 1 {
                return Err(format!("invalid fmin/fmax funct3 {f3}"));
            }
            let a = m.fregs[rs1(w)];
            let b = m.fregs[rs2(w)];
            let val = if dbl {
                fp::min_max64(a, b, f3 == 1)
            } else {
                fp::box_single(fp::min_max32(fp::single_bits(a), fp::single_bits(b), f3 == 1))
            };
            m.write_freg(rd, val, changes);
        }
        // fsqrt: single operand, rounding mode in funct3.
        22 => {
            let Some(rm) = fp_rm(m, f3) else {
                return Err(format!("invalid rounding mode {f3}"));
            };
            let a = m.fregs[rs1(w)];
            let (val, flags) = if dbl {
                fp::double_sqrt(a, rm)
            } else {
                let (v, fl) = fp::single_sqrt(fp::single_bits(a), rm);
                (fp::box_single(v), fl)
            };
            m.write_freg(rd, val, changes);
            m.acc_fflags(flags, changes);
        }
        // Precision conversions fcvt.s.d / fcvt.d.s, rounding mode in funct3.
        16 => {
            let Some(rm) = fp_rm(m, f3) else {
                return Err(format!("invalid rounding mode {f3}"));
            };
            let a = m.fregs[rs1(w)];
            let (val, flags) = if dbl {
                // fcvt.d.s: single in, double out.
                let (bits, fl) = fp::cvt_f32_to_f64(fp::single_bits(a));
                (bits, fl)
            } else {
                // fcvt.s.d: double in, single out (NaN-boxed).
                let (bits, fl) = fp::cvt_f64_to_f32(a, rm);
                (fp::box_single(bits), fl)
            };
            m.write_freg(rd, val, changes);
            m.acc_fflags(flags, changes);
        }
        // Comparisons fle/flt/feq: integer destination.
        40 => {
            if f3 > 2 {
                return Err(format!("invalid comparison funct3 {f3}"));
            }
            let a = m.fregs[rs1(w)];
            let b = m.fregs[rs2(w)];
            let (res, flags) = if dbl {
                fp::compare64(a, b, f3)
            } else {
                fp::compare32(fp::single_bits(a), fp::single_bits(b), f3)
            };
            m.write_reg(rd, res, changes);
            m.acc_fflags(flags, changes);
        }
        // fcvt.w.s / fcvt.wu.s / fcvt.w.d / fcvt.wu.d: float to int, rs2
        // picks signed (0) or unsigned (1), funct3 the rounding mode.
        48 => {
            let Some(rm) = fp_rm(m, f3) else {
                return Err(format!("invalid rounding mode {f3}"));
            };
            if rs2(w) > 1 {
                return Err(format!("invalid fcvt rs2 {}", rs2(w)));
            }
            let unsigned = rs2(w) == 1;
            let a = m.fregs[rs1(w)];
            let (res, flags) = fp::cvt_to_int(if dbl { a } else { fp::single_bits(a) as u64 }, dbl, rm, unsigned);
            m.write_reg(rd, res, changes);
            m.acc_fflags(flags, changes);
        }
        // fcvt.s.w / fcvt.s.wu / fcvt.d.w / fcvt.d.wu: int to float.
        52 => {
            let Some(rm) = fp_rm(m, f3) else {
                return Err(format!("invalid rounding mode {f3}"));
            };
            if rs2(w) > 1 {
                return Err(format!("invalid fcvt rs2 {}", rs2(w)));
            }
            let unsigned = rs2(w) == 1;
            let v = m.regs[rs1(w)];
            if dbl {
                m.write_freg(rd, fp::cvt_int_to_f64(v, unsigned), changes);
            } else {
                let (bits, flags) = fp::cvt_int_to_f32(v, unsigned, rm);
                m.write_freg(rd, fp::box_single(bits), changes);
                m.acc_fflags(flags, changes);
            }
        }
        // fmv.x.s (funct3 0) and fclass (funct3 1): integer destinations.
        56 => {
            let a = m.fregs[rs1(w)];
            match (f3, dbl) {
                (0, false) => {
                    // Bit move: sign-extended per the 32-bit convention.
                    m.write_reg(rd, fp::single_bits(a) as i32 as i64 as u64, changes);
                }
                (1, _) => {
                    let mask = if dbl { fp::classify64(a) } else { fp::classify32(fp::single_bits(a)) };
                    m.write_reg(rd, mask, changes);
                }
                _ => return Err("fmv.x.d requires RV64".to_string()),
            }
        }
        // fmv.s.x: integer register bits move into an FP register (boxed).
        60 => {
            if f3 != 0 || dbl {
                return Err("invalid fmv.s.x encoding".to_string());
            }
            m.write_freg(rd, fp::box_single(m.regs[rs1(w)] as u32), changes);
        }
        other => return Err(format!("invalid FP funct7 {f7} (group {other})")),
    }
    Ok(())
}

/// Fused multiply-add (opcodes 0x43/0x47/0x4b/0x4f). rs3 sits above the
/// format pair in the funct7 slot.
fn fp_fma(m: &mut Machine, w: u32, changes: &mut Vec<Change>) -> Result<(), String> {
    let fmt = (w >> 25) & 0x3;
    if fmt > 1 {
        return Err(format!("invalid FMA format {fmt}"));
    }
    let Some(rm) = fp_rm(m, (w >> 12) & 0x7) else {
        return Err(format!("invalid rounding mode {}", (w >> 12) & 0x7));
    };
    let kind = match w & 0x7f {
        0x43 => fp::FmaKind::Fmadd,
        0x47 => fp::FmaKind::Fmsub,
        0x4b => fp::FmaKind::Fnmsub,
        _ => fp::FmaKind::Fnmadd,
    };
    let (a, b, c) = (m.fregs[rs1(w)], m.fregs[rs2(w)], m.fregs[((w >> 27) & 0x1f) as usize]);
    let (val, flags) = if fmt == 1 {
        fp::double_fma(kind, a, b, c, rm)
    } else {
        let (v, fl) =
            fp::single_fma(kind, fp::single_bits(a), fp::single_bits(b), fp::single_bits(c), rm);
        (fp::box_single(v), fl)
    };
    m.write_freg(rd(w), val, changes);
    m.acc_fflags(flags, changes);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csr;
    use crate::testutil::*;

    /// Hand-encode an FP opcode-0x53 instruction for rounding-mode tests
    /// the assembler cannot express (it bakes RNE/RTZ).
    fn fp_r(f7: u32, rs2: u32, rs1: u32, f3: u32, rd: u32) -> u32 {
        f7 << 25 | rs2 << 20 | rs1 << 15 | f3 << 12 | rd << 7 | 0x53
    }

    fn fp_fma(op: u32, fmt: u32, rs3: u32, rs2: u32, rs1: u32, rm: u32, rd: u32) -> u32 {
        rs3 << 27 | fmt << 25 | rs2 << 20 | rs1 << 15 | rm << 12 | rd << 7 | op
    }

    /// Execute one hand-encoded word on an existing machine (ends the
    /// program borrow before the mutable one starts).
    fn exec1(m: &mut Machine, w: u32) {
        let pc = m.program().text_base;
        execute(m, w, pc);
    }

    #[test]
    fn arithmetic_and_memory_roundtrip() {
        let src = "\
    li a0, 0x12345678
    sw a0, 0(sp)
    lw a1, 0(sp)
    lb a2, 2(sp)
";
        let mut m = machine(src);
        m.step(); // li expands to lui
        m.step(); // ... plus addi
        assert_eq!(m.reg(10), 0x1234_5678);
        m.step(); // sw
        m.step(); // lw
        assert_eq!(m.reg(11), 0x1234_5678);
        m.step(); // lb: byte 2 of 0x12345678 little-endian is 0x34
        assert_eq!(m.reg(12), 0x34);
    }

    #[test]
    fn sign_extension_on_lb() {
        let src = "\
    li a0, 0x89
    sb a0, 0(sp)
    lb a1, 0(sp)
";
        let mut m = machine(src);
        for _ in 0..4 {
            m.step();
        }
        assert_eq!(m.reg(11) as u32 as i32, -119); // 0x89 sign-extended
    }

    #[test]
    fn csr_read_write() {
        let src = "\
    li t0, 42
    csrrw x0, uscratch, t0
    csrrs a0, uscratch, x0
";
        let mut m = machine(src);
        m.run(None);
        assert_eq!(m.reg(10), 42);
    }

    #[test]
    fn auipc_and_jal_link() {
        let src = "\
    auipc a0, 0
    jal a1, target
    nop
target:
    nop
";
        let mut m = machine(src);
        m.step();
        assert_eq!(m.reg(10), u64::from(m.program().text_base));
        m.step();
        // a1 links to the instruction after the jal
        assert_eq!(m.reg(11), u64::from(m.program().text_base + 8));
        assert_eq!(m.pc(), m.program().text_base + 12);
    }

    #[test]
    fn slt_variants() {
        let src = "\
    li a0, -1
    li a1, 1
    slt a2, a0, a1
    sltu a3, a0, a1
    sltiu a4, a1, -1
";
        let mut m = machine(src);
        m.run(None);
        assert_eq!(m.reg(12), 1); // -1 < 1 signed
        assert_eq!(m.reg(13), 0); // 0xffffffff > 1 unsigned
        // sltiu sign-extends the immediate then compares unsigned:
        // 1 < 0xffffffff → 1
        assert_eq!(m.reg(14), 1);
    }

    #[test]
    fn mul_family() {
        let src = "\
    li a0, 7
    li a1, -3
    mul a2, a0, a1
    li t0, 0x10000
    mulhu a4, t0, t0
    mulh a5, t0, t0
    mulhsu a6, a1, t0
    li t1, -2147483648
    li t2, -1
    mulhsu s0, t1, t2
";
        let mut m = machine(src);
        m.run(None);
        assert_eq!(m.reg(12) as u32 as i32, -21); // mul keeps the low 32 bits
        assert_eq!(m.reg(14), 1); // mulhu: 0x10000^2 >> 32
        assert_eq!(m.reg(15), 1); // mulh agrees with mulhu on positive operands
        // mulhsu(-3, 0x10000): (-3 * 65536) >> 32 floors to -1.
        assert_eq!(m.reg(16) as u32 as i32, -1);
        // Exact worst case: i32::MIN * u32::MAX = 0x8000_0000_8000_0000,
        // so the upper half is i32::MIN itself.
        assert_eq!(m.reg(8) as u32, 0x8000_0000);
    }

    #[test]
    fn mulh_sign_combinations() {
        let src = "\
    li a0, 7
    li a1, -7
    li a2, 3
    li a3, -3
    mulh t0, a0, a2
    mulh t1, a0, a3
    mulh t2, a1, a3
    mulh t3, a1, a2
";
        let mut m = machine(src);
        m.run(None);
        assert_eq!(m.reg(5), 0); // +7 * +3: high word 0
        assert_eq!(m.reg(6) as u32 as i32, -1); // +7 * -3: high word all ones
        assert_eq!(m.reg(7), 0); // -7 * -3: high word 0
        assert_eq!(m.reg(28) as u32 as i32, -1); // -7 * +3: high word all ones
    }

    #[test]
    fn div_rem_edges() {
        let src = "\
    li a0, 7
    li a1, -2
    div a2, a0, a1
    rem a3, a0, a1
    li t0, -2147483648
    li t1, -1
    div t3, t0, t1
    rem t4, t0, t1
    div a4, a0, zero
    rem a5, a0, zero
    divu a6, a0, zero
    remu a7, a0, zero
    divu s2, a0, a1
    remu s3, a0, a1
";
        let mut m = machine(src);
        m.run(None);
        // 7 / -2 truncates toward zero: q = -3, r = 1, and a == b*q + r.
        let (q, r) = ((m.reg(12) as u32 as i32), (m.reg(13) as u32 as i32));
        assert_eq!((q, r), (-3, 1));
        assert_eq!(7, (-2i32).wrapping_mul(q).wrapping_add(r));
        // i32::MIN / -1 overflows back to i32::MIN; the matching remainder is 0.
        assert_eq!(m.reg(28) as u32, 0x8000_0000);
        assert_eq!(m.reg(29), 0);
        // Divide by zero never traps: quotient -1, remainder is the dividend.
        assert_eq!(m.reg(14), u64::MAX); // div: -1, stored sign-extended
        assert_eq!(m.reg(15), 7); // rem: rs1 passes through
        assert_eq!(m.reg(16), u64::MAX); // divu: 0xffffffff, sign-extended
        assert_eq!(m.reg(17), 7); // remu: rs1's low 32 bits
        // Unsigned view of -2 is 0xfffffffe, which swallows the small dividend.
        assert_eq!(m.reg(18), 0); // divu: 7 / 0xfffffffe = 0
        assert_eq!(m.reg(19), 7); // remu: 7 % 0xfffffffe = 7
    }

    // ---- Floating point (F/D extensions) ----

    #[test]
    fn fp_add_and_fmv_roundtrip() {
        let src = "\
.data
va: .float 1.5
vb: .float 2.25
.text
    flw f1, va
    flw f2, vb
    fadd.s f3, f1, f2
    fmv.x.s a0, f3
    li a7, 10
    ecall
";
        let mut m = machine(src);
        m.run(None);
        assert_eq!(m.reg(10), 3.75f32.to_bits() as i32 as i64 as u64);
        // NaN-boxed register contents.
        assert_eq!(m.freg(3), 0xffff_ffff_0000_0000u64 | 3.75f32.to_bits() as u64);
    }

    #[test]
    fn fp_nan_box_rules() {
        // An un-boxed register (upper bits not all ones) reads as canonical
        // NaN for 32-bit ops.
        let mut m = machine("    nop\n");
        m.fregs[1] = 1.5f32.to_bits() as u64; // not boxed
        m.fregs[2] = fp::box_single(2.0f32.to_bits());
        let w = fp_r(0x00, 2, 1, 0, 3); // fadd.s f3, f1, f2 (RNE)
        exec1(&mut m, w);
        assert_eq!(m.freg(3), fp::box_single(fp::F32_NAN));
        // fmv.s.x boxes the integer bits.
        let mut m = machine("    li t0, 0x3fc00000\n    fmv.s.x f1, t0\n");
        m.run(None);
        assert_eq!(m.freg(1), fp::box_single(1.5f32.to_bits()));
    }

    #[test]
    fn fp_directed_arithmetic_and_flags() {
        // Directed cases through assembled code (rm=RNE).
        let src = "\
.data
one: .float 1.0
minf: .float -3.0
.text
    flw f1, one
    flw f2, minf
    fadd.s f3, f1, f2      # -2.0
    fmul.s f4, f3, f3      # 4.0
    fdiv.s f5, f1, f3      # -0.5
    fsub.s f6, f3, f3      # +0 (x - x)
    fmul.s f7, f3, f1      # -2
    fsgnjx.s f8, f7, f7    # fabs -> 2
    fsqrt.s f9, f8         # sqrt(2), inexact
    feq.s t0, f1, f1       # 1
    flt.s t1, f3, f1       # 1 (-2 < 1)
";
        let mut m = machine(src);
        m.run(None);
        assert_eq!(m.freg(3) as u32, (-2.0f32).to_bits());
        assert_eq!(m.freg(4) as u32, 4.0f32.to_bits());
        assert_eq!(m.freg(5) as u32, (-0.5f32).to_bits());
        assert_eq!(m.freg(6) as u32, 0); // +0
        assert_eq!(m.freg(8) as u32, 2.0f32.to_bits());
        assert_eq!(m.freg(9) as u32, (2.0f32).sqrt().to_bits());
        assert_eq!(m.reg(5), 1);
        assert_eq!(m.reg(6), 1);
        // sqrt(2) was inexact: NX accumulated (and nothing else).
        assert_eq!(m.csr(csr::FFLAGS), fp::NX as u64);
    }

    #[test]
    fn fp_special_values_and_flags() {
        // sqrt(-1) -> canonical NaN + NV; 0/0 -> NaN + NV; 1/0 -> inf + DZ.
        let src = "\
.data
one: .float 1.0
neg: .float -1.0
zerof: .float 0.0
.text
    flw f1, one
    flw f2, neg
    flw f3, zerof
    fsqrt.s f4, f2
    fdiv.s f5, f3, f3
    fdiv.s f6, f1, f3
    fmv.x.s a0, f4
    fmv.x.s a1, f5
    fmv.x.s a2, f6
";
        let mut m = machine(src);
        m.run(None);
        assert_eq!(m.reg(10) as u32, fp::F32_NAN);
        assert_eq!(m.reg(11) as u32, fp::F32_NAN);
        assert_eq!(m.reg(12) as u32, fp::inf32(false));
        // NV (sqrt) + NV (0/0) + DZ (1/0).
        assert_eq!(m.csr(csr::FFLAGS), (fp::NV | fp::DZ) as u64);
    }

    #[test]
    fn fp_overflow_sets_of() {
        let src = "\
.data
big: .float 3.4e38
.text
    flw f1, big
    fmul.s f2, f1, f1
";
        let mut m = machine(src);
        m.run(None);
        assert_eq!(m.freg(2) as u32, fp::inf32(false));
        assert_eq!(m.csr(csr::FFLAGS), (fp::OF | fp::NX) as u64);
    }

    #[test]
    fn fp_rounding_modes_on_add() {
        // 1 + 3*2^-25 = 1.75 f32-ulps: every mode lands differently across
        // the set. The assembler cannot emit rm != RNE, so these go through
        // hand-encoded fadd.s f3, f1, f2 words.
        let one = 1.0f32.to_bits();
        let frac = (3.0 * f64::powi(2.0, -25)) as f32;
        let up = (1.0f64 + 3.0 * f64::powi(2.0, -25)) as f32;
        let cases = [
            (fp::RNE, up.to_bits()),
            (fp::RTZ, 1.0f32.to_bits()),
            (fp::RDN, 1.0f32.to_bits()),
            (fp::RUP, up.to_bits()),
            (fp::RMM, up.to_bits()),
        ];
        for (rm, want) in cases {
            let mut m = machine("    nop\n");
            m.fregs[1] = fp::box_single(one);
            m.fregs[2] = fp::box_single(frac.to_bits());
            let w = fp_r(0x00, 2, 1, rm as u32, 3);
            exec1(&mut m, w);
            assert_eq!(m.freg(3) as u32, want, "rm={rm}");
            assert_eq!(m.csr(csr::FFLAGS), fp::NX as u64, "rm={rm}");
        }
        // The tie 1 + 2^-24: RNE stays even, RMM rounds away (bits +1).
        let tie = (f64::powi(2.0, -24)) as f32;
        let tie_up_bits = 1.0f32.to_bits() + 1;
        for (rm, want) in [(fp::RNE, 1.0f32.to_bits()), (fp::RMM, tie_up_bits)] {
            let mut m = machine("    nop\n");
            m.fregs[1] = fp::box_single(one);
            m.fregs[2] = fp::box_single(tie.to_bits());
            let w = fp_r(0x00, 2, 1, rm as u32, 3);
            exec1(&mut m, w);
            assert_eq!(m.freg(3) as u32, want, "rm={rm}");
        }
    }

    #[test]
    fn fp_dyn_reads_frm_csr() {
        // rm=111 (DYN) resolves through the frm CSR; the assembler cannot
        // emit it, so the word is hand-built. With frm=RTZ the tie add
        // truncates instead of rounding to even.
        let mut m = machine("    li t0, 1\n    csrrw x0, frm, t0\n");
        m.step(); // li is small enough to be a single addi
        m.step(); // csrrw
        assert_eq!(m.csr(csr::FRM), 1);
        m.fregs[1] = fp::box_single(1.0f32.to_bits());
        m.fregs[2] = fp::box_single(f32::powi(2.0, -24).to_bits());
        let w = fp_r(0x00, 2, 1, 7, 3); // rm = DYN
        exec1(&mut m, w);
        assert_eq!(m.freg(3) as u32, 1.0f32.to_bits());
    }

    #[test]
    fn fp_double_addition_and_modes() {
        let src = "\
.data
da: .double 1.5
db: .double 2.25
sa: .float 1.5
.align 3
res: .double 0.0
.text
    fld f1, da
    fld f2, db
    fadd.d f3, f1, f2
    fsd f3, res
    flw f6, sa
    fcvt.d.s f4, f6      # boxed single widens exactly
    fadd.d f5, f4, f2
";
        let mut m = machine(src);
        m.run(None);
        let mut word = [0u8; 8];
        let res = m.program().symbols.get("res").unwrap().addr;
        m.peek_bytes(res, &mut word).unwrap();
        assert_eq!(word, 3.75f64.to_bits().to_le_bytes());
        assert_eq!(m.freg(3), 3.75f64.to_bits());
        assert_eq!(m.freg(4), 1.5f64.to_bits());
        assert_eq!(m.freg(5), 3.75f64.to_bits());
        assert_eq!(m.csr(csr::FFLAGS), 0);
    }

    #[test]
    fn fp_double_rounding_modes() {
        // 1 - 2^-54: RTZ takes the lower neighbor, RNE ties to 1.0.
        let half = (1.0f64 - 1.0f64.next_down()) / 2.0;
        for (rm, want) in [
            (fp::RTZ, 1.0f64.next_down().to_bits()),
            (fp::RNE, 1.0f64.to_bits()),
            (fp::RDN, 1.0f64.next_down().to_bits()),
            (fp::RUP, 1.0f64.to_bits()),
        ] {
            let mut m = machine("    nop\n");
            m.fregs[1] = 1.0f64.to_bits();
            m.fregs[2] = (-half).to_bits();
            let w = fp_r(0x01, 2, 1, rm as u32, 3); // fadd.d
            exec1(&mut m, w);
            assert_eq!(m.freg(3), want, "rm={rm}");
            assert_eq!(m.csr(csr::FFLAGS), fp::NX as u64, "rm={rm}");
        }
    }

    #[test]
    fn fp_conversions() {
        let src = "\
.data
big: .float 3.0e9
small: .float -2.9
nan: .float nan
.text
    flw f1, big
    flw f2, small
    flw f3, nan
    fcvt.w.s t0, f1     # out of range -> 0x7fffffff + NV
    fcvt.w.s t1, f2     # -2 (truncation)
    fcvt.w.s t2, f3     # NaN -> 0x7fffffff + NV
    fcvt.wu.s t3, f2    # invalid (negative) -> 0xffffffff + NV
    li t4, 0x7fffffff
    fcvt.s.w f5, t4     # rounds up to 2^31, NX
    fmv.x.s a0, f5
";
        let mut m = machine(src);
        m.run(None);
        assert_eq!(m.reg(5) as u32, 0x7fff_ffff);
        assert_eq!(m.reg(6) as u32 as i32, -2);
        assert_eq!(m.reg(7) as u32, 0x7fff_ffff);
        assert_eq!(m.reg(28) as u32, 0xffff_ffff); // t3
        assert_eq!(m.reg(10) as u32, (2.0f32.powi(31)).to_bits());
        assert_eq!(m.csr(csr::FFLAGS), (fp::NV | fp::NX) as u64);
    }

    #[test]
    fn fp_classify_minmax_fabs_fneg() {
        let src = "\
.data
pnorm: .float 2.5
nnorm: .float -2.5
pinf: .float inf
pz: .float 0.0
nz: .float -0.0
.text
    flw f1, pnorm
    flw f2, nnorm
    flw f3, pinf
    flw f4, pz
    flw f5, nz
    fclass.s t0, f1
    fclass.s t1, f2
    fclass.s t2, f3
    fclass.s t3, f4
    fclass.s t4, f5
    fmin.s f6, f1, f2     # -2.5
    fmax.s f7, f4, f5     # +0 (sign rule)
    fabs.s f8, f2         # 2.5
    fneg.s f9, f1         # -2.5
";
        let mut m = machine(src);
        m.run(None);
        assert_eq!(m.reg(5), 1 << 6); // +normal
        assert_eq!(m.reg(6), 1 << 1); // -normal
        assert_eq!(m.reg(7), 1 << 7); // +inf
        assert_eq!(m.reg(28), 1 << 4); // +0 (t3)
        assert_eq!(m.reg(29), 1 << 3); // -0 (t4)
        assert_eq!(m.freg(6) as u32, (-2.5f32).to_bits());
        assert_eq!(m.freg(7) as u32, 0); // +0
        assert_eq!(m.freg(8) as u32, 2.5f32.to_bits());
        assert_eq!(m.freg(9) as u32, (-2.5f32).to_bits());
        // min/max/sign ops raise no flags.
        assert_eq!(m.csr(csr::FFLAGS), 0);
    }

    #[test]
    fn fp_fmin_fmax_nan_rules() {
        let mut m = machine("    nop\n");
        m.fregs[1] = fp::box_single(fp::F32_NAN);
        m.fregs[2] = fp::box_single(1.5f32.to_bits());
        exec1(&mut m, fp_r(0x14, 2, 1, 0, 3)); // fmin.s
        assert_eq!(m.freg(3) as u32, fp::F32_NAN);
        exec1(&mut m, fp_r(0x14, 2, 1, 1, 3)); // fmax.s
        assert_eq!(m.freg(3) as u32, fp::F32_NAN);
        // No NV from NaN operands on min/max.
        assert_eq!(m.csr(csr::FFLAGS), 0);
        // Both zero: fmax(-0,+0) = +0, fmin = -0.
        let mut m = machine("    nop\n");
        m.fregs[1] = fp::box_single(0x8000_0000);
        m.fregs[2] = fp::box_single(0);
        exec1(&mut m, fp_r(0x14, 2, 1, 1, 3));
        assert_eq!(m.freg(3) as u32, 0);
        exec1(&mut m, fp_r(0x14, 2, 1, 0, 3));
        assert_eq!(m.freg(3) as u32, 0x8000_0000);
    }

    #[test]
    fn fp_fma_single_rounding_end_to_end() {
        // Fused keeps the low bit that a separate mul+add loses.
        let src = "\
.data
va: .float 1.0000001
vb: .float 1.0000004
vc: .float -1.0
.text
    flw f1, va
    flw f2, vb
    flw f3, vc
    fmadd.s f4, f1, f2, f3
    fmul.s f5, f1, f2
    fadd.s f6, f5, f3
";
        let mut m = machine(src);
        m.run(None);
        assert_eq!(m.freg(4) as u32, 0x3500_0001);
        assert_eq!(m.freg(6) as u32, 0x3500_0000);
        assert_ne!(m.freg(4), m.freg(6));
    }

    #[test]
    fn fp_fma_variants_and_double() {
        let mut m = machine("    nop\n");
        let a = (1.0f32 + f32::powi(2.0, -23)).to_bits();
        let b = (1.0f32 + 3.0 * f32::powi(2.0, -23)).to_bits();
        m.fregs[1] = fp::box_single(a);
        m.fregs[2] = fp::box_single(b);
        m.fregs[3] = fp::box_single(2.0f32.to_bits());
        // fmsub: ab - 2; fnmadd: -(ab) - 2; fnmsub: -(ab) + 2.
        exec1(&mut m, fp_fma(0x47, 0, 3, 2, 1, 0, 4));
        let fmsub = m.freg(4) as u32;
        exec1(&mut m, fp_fma(0x4f, 0, 3, 2, 1, 0, 5));
        let fnmadd = m.freg(5) as u32;
        exec1(&mut m, fp_fma(0x4b, 0, 3, 2, 1, 0, 6));
        let fnmsub = m.freg(6) as u32;
        // fnmsub = -(ab - c) is the exact sign flip of fmsub; fnmadd is the
        // exact sign flip of ab + c computed separately.
        assert_eq!(fnmsub, fmsub ^ 0x8000_0000);
        let mul_bits = fp::single_bin(fp::BinOp::Mul, a, b, fp::RNE).0;
        let add_bits = fp::single_bin(fp::BinOp::Add, mul_bits, 2.0f32.to_bits(), fp::RNE).0;
        assert_eq!(fnmadd, add_bits ^ 0x8000_0000);
        // .d fused: (1+2^-52)^2 - 1 = 2^-51 exactly after one rounding.
        let mut m = machine("    nop\n");
        let a = (1.0f64 + f64::powi(2.0, -52)).to_bits();
        m.fregs[1] = a;
        m.fregs[3] = (-1.0f64).to_bits();
        exec1(&mut m, fp_fma(0x43, 1, 3, 1, 1, 0, 4));
        assert_eq!(m.freg(4), f64::powi(2.0, -51).to_bits());
    }

    #[test]
    fn fp_comparisons_with_nan_flags() {
        let mut m = machine("    nop\n");
        m.fregs[1] = fp::box_single(1.0f32.to_bits());
        m.fregs[2] = fp::box_single(fp::F32_NAN);
        exec1(&mut m, fp_r(0x50, 2, 1, 2, 5)); // feq.s
        assert_eq!(m.reg(5), 0);
        assert_eq!(m.csr(csr::FFLAGS), 0); // quiet NaN, feq stays silent
        exec1(&mut m, fp_r(0x50, 2, 1, 1, 6)); // flt.s
        assert_eq!(m.reg(6), 0);
        assert_eq!(m.csr(csr::FFLAGS), fp::NV as u64); // ordered compare on NaN
    }

    #[test]
    fn fp_csr_aliasing() {
        // Writing fcsr splits into fflags/frm; writing fflags resyncs fcsr.
        let src = "\
    li t0, 0x4b
    csrrw x0, fcsr, t0
    csrrs a0, fflags, x0
    csrrs a1, frm, x0
    li t1, 0x1
    csrrw x0, fflags, t1
    csrrs a2, fcsr, x0
";
        let mut m = machine(src);
        m.run(None);
        assert_eq!(m.reg(10), 0x0b); // fflags = 0x4b & 0x1f
        assert_eq!(m.reg(11), 0x2); // frm = 0x4b >> 5
        assert_eq!(m.reg(12), 0x41); // fcsr after fflags=1: frm 2 << 5 | 1
    }

    #[test]
    fn fp_backstep_restores_fpregs_and_flags() {
        let src = "\
.data
one: .float 1.0
three: .float 3.0
.text
    flw f1, one
    flw f2, three
    fdiv.s f3, f1, f2   # 1/3 is inexact -> NX
";
        let mut m = machine(src);
        // flw expands to lui+flw, so the fdiv is the fifth instruction.
        for _ in 0..5 {
            m.step();
        }
        assert_eq!(m.csr(csr::FFLAGS), fp::NX as u64);
        let boxed = m.freg(3);
        assert!(m.backstep());
        assert_eq!(m.csr(csr::FFLAGS), 0);
        assert_eq!(m.freg(3), 0);
        // Re-execute to confirm nothing was corrupted by the undo.
        m.step();
        assert_eq!(m.freg(3), boxed);
    }

    #[test]
    fn fp_reserved_modes_halt() {
        // rm=5 is reserved; the decode reports it like other illegal
        // encodings via a Halted(Error) event.
        let mut m = machine("    nop\n");
        m.fregs[1] = fp::box_single(1.0f32.to_bits());
        m.fregs[2] = fp::box_single(1.0f32.to_bits());
        let w = fp_r(0x00, 2, 1, 5, 3);
        let pc = m.program().text_base;
        let out = execute(&mut m, w, pc);
        assert!(matches!(out.outcome.events.last(), Some(Event::Halted(Halt::Error { .. }))));
    }
}
