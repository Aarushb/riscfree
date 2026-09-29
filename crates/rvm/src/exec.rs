//! Fetch-decode-execute for RV32I plus the M extension. The machine's write
//! helpers record undo entries and change events, so this file stays a plain
//! decode `match`.

use crate::csr;
use crate::fp;
use crate::memory::MemError;
use crate::trap::exc;
use crate::{Change, Event, Halt, Machine, StepOutcome};

pub(crate) struct ExecOutcome {
    pub outcome: StepOutcome,
    /// True when the machine must stop stepping (halt already recorded).
    pub terminated_now: bool,
    /// Synchronous exception attached to a `Halt::Error`: (cause, utval). The
    /// run loop vectors it through utvec when a handler is configured.
    pub exception: Option<(u32, u64)>,
}

impl ExecOutcome {
    fn ok(changes: Vec<Change>, events: Vec<Event>, pc_before: u32) -> Self {
        ExecOutcome {
            outcome: StepOutcome { executed: true, pc_before, events, changes },
            terminated_now: false,
            exception: None,
        }
    }

    fn halt(h: Halt, events: Vec<Event>, pc_before: u32) -> Self {
        let mut evs = events;
        evs.push(Event::Halted(h.clone()));
        ExecOutcome {
            outcome: StepOutcome { executed: false, pc_before, events: evs, changes: Vec::new() },
            terminated_now: true,
            exception: None,
        }
    }

    /// A halt that is really a synchronous exception; `exc` carries the cause
    /// and the utval so the caller can vector it when utvec is configured.
    fn halt_exc(h: Halt, events: Vec<Event>, pc_before: u32, exc: Option<(u32, u64)>) -> Self {
        let mut out = Self::halt(h, events, pc_before);
        out.exception = exc;
        out
    }
}

/// Map a memory fault to its exception cause and utval.
fn mem_exception(e: &MemError, store: bool) -> (u32, u64) {
    match e {
        MemError::Unaligned { addr, .. } => {
            (if store { exc::STORE_MISALIGNED } else { exc::LOAD_MISALIGNED }, u64::from(*addr))
        }
        MemError::AccessViolation { addr } => {
            (if store { exc::STORE_FAULT } else { exc::LOAD_FAULT }, u64::from(*addr))
        }
    }
}

fn sx(v: u32, bits: u32) -> u64 {
    // Sign-extend the low `bits` of v to 64 bits.
    let shift = 32 - bits;
    (((v << shift) as i32) >> shift) as i64 as u64
}

/// Sign-extend a 32-bit value to the full register width: the result rule
/// for every RV64 `*w` operation.
fn sx32(v: u32) -> u64 {
    v as i32 as i64 as u64
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

/// Shift amount, six bits wide in RV64 (imm[5] becomes shamt[5]).
fn shamt_of(m: &Machine, w: u32) -> u64 {
    ((w >> 20) & if m.rv64() { 0x3f } else { 0x1f }) as u64
}

/// Register shift amount mask: RV32 uses the low 5 bits, RV64 the low 6.
fn shift_mask(m: &Machine) -> u64 {
    if m.rv64() { 0x3f } else { 0x1f }
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
            // Every bail here is an illegal-instruction class fault; utval
            // carries the offending word for the handler.
            return ExecOutcome::halt_exc(
                Halt::Error { message: $msg },
                events,
                pc_before,
                Some((exc::ILLEGAL_INSN, w as u64)),
            )
        };
    }

    match opcode {
        0x37 => {
            // lui: the 32-bit U immediate is sign-extended to XLEN in RV64.
            let raw = imm_u(w) as u32;
            let v = if m.rv64() { sx32(raw) } else { u64::from(raw) };
            m.write_reg(rd(w), v, &mut changes);
        }
        0x17 => {
            // auipc: same sign-extension rule as lui in RV64.
            let raw = imm_u(w) as u32;
            let imm = if m.rv64() { sx32(raw) } else { u64::from(raw) };
            let v = (pc_before as u64).wrapping_add(imm);
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
            // branches: compares run at register width (32- or 64-bit).
            let (a, b, au, bu) = if m.rv64() {
                (
                    m.regs[rs1(w)] as i64,
                    m.regs[rs2(w)] as i64,
                    m.regs[rs1(w)],
                    m.regs[rs2(w)],
                )
            } else {
                (
                    m.regs[rs1(w)] as u32 as i32 as i64,
                    m.regs[rs2(w)] as u32 as i32 as i64,
                    u64::from(m.regs[rs1(w)] as u32),
                    u64::from(m.regs[rs2(w)] as u32),
                )
            };
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
            // loads. Addresses stay 32-bit by design: RARS's memory map (and
            // therefore every address a teaching program can form) lives in
            // the low 4 GB, so the effective address truncates to u32 in both
            // modes.
            let addr = (m.regs[rs1(w)] as u32).wrapping_add(imm_i(w) as u32);
            let loaded = match (w >> 12) & 0x7 {
                0 => m.load_signed(addr, 1, &mut changes),
                1 => m.load_signed(addr, 2, &mut changes),
                2 => m.load_signed(addr, 4, &mut changes),
                // RV64: ld moves a full doubleword; lwu zero-extends a word.
                3 => m.load_bytes(addr, 8, &mut changes),
                4 => m.load_bytes(addr, 1, &mut changes),
                5 => m.load_bytes(addr, 2, &mut changes),
                6 => m.load_bytes(addr, 4, &mut changes),
                f => bail!(format!("invalid load funct3 {f}")),
            };
            match loaded {
                Ok(v) => m.write_reg(rd(w), v, &mut changes),
                Err(e) => {
                    return ExecOutcome::halt_exc(
                        Halt::Error { message: e.to_string() },
                        events,
                        pc_before,
                        Some(mem_exception(&e, false)),
                    )
                }
            }
        }
        0x23 => {
            // stores (sd adds the RV64 doubleword form); addresses truncate
            // to 32 bits exactly as for loads.
            let addr = (m.regs[rs1(w)] as u32).wrapping_add(imm_s(w) as u32);
            let val = m.regs[rs2(w)];
            let r = match (w >> 12) & 0x7 {
                0 => m.store_bytes(addr, val, 1, &mut changes),
                1 => m.store_bytes(addr, val, 2, &mut changes),
                2 => m.store_bytes(addr, val, 4, &mut changes),
                3 => m.store_bytes(addr, val, 8, &mut changes),
                f => bail!(format!("invalid store funct3 {f}")),
            };
            if let Err(e) = r {
                return ExecOutcome::halt_exc(
                    Halt::Error { message: e.to_string() },
                    events,
                    pc_before,
                    Some(mem_exception(&e, true)),
                );
            }
        }
        0x13 => {
            // op-imm: compares and shifts run at register width.
            let a = m.regs[rs1(w)];
            let imm = imm_i(w);
            let v = match (w >> 12) & 0x7 {
                0 => a.wrapping_add(imm),
                2 => {
                    if m.rv64() {
                        ((a as i64) < (imm as i64)) as u64 // slti
                    } else {
                        ((a as u32 as i32) < (imm as u32 as i32)) as u64
                    }
                }
                3 => {
                    if m.rv64() {
                        (a < imm) as u64 // sltiu, xlen-unsigned
                    } else {
                        ((a as u32) < (imm as u32)) as u64
                    }
                }
                4 => a ^ imm,
                6 => a | imm,
                7 => a & imm,
                1 => a << shamt_of(m, w), // slli
                5 => {
                    if (w >> 25) & 0x20 != 0 {
                        ((a as i64) >> shamt_of(m, w)) as u64 // srai
                    } else {
                        a >> shamt_of(m, w) // srli
                    }
                }
                _ => unreachable!(),
            };
            m.write_reg(rd(w), v, &mut changes);
        }
        0x33 => {
            // op: register-width arithmetic; the M extension follows the
            // register width too (64-bit operands in RV64).
            let a = m.regs[rs1(w)];
            let b = m.regs[rs2(w)];
            let mask = shift_mask(m);
            let v = match ((w >> 12) & 0x7, (w >> 25) & 0x3f) {
                (0, 0x00) => a.wrapping_add(b),           // add
                (0, 0x20) => a.wrapping_sub(b),           // sub
                (1, 0x00) => a << (b & mask),             // sll
                (2, 0x00) => {
                    if m.rv64() {
                        ((a as i64) < (b as i64)) as u64 // slt
                    } else {
                        ((a as u32 as i32) < (b as u32 as i32)) as u64
                    }
                }
                (3, 0x00) => {
                    if m.rv64() {
                        (a < b) as u64 // sltu
                    } else {
                        ((a as u32) < (b as u32)) as u64
                    }
                }
                (4, 0x00) => a ^ b,                       // xor
                (5, 0x00) => a >> (b & mask),             // srl
                (5, 0x20) => ((a as i64) >> (b & mask)) as u64, // sra
                (6, 0x00) => a | b,                       // or
                (7, 0x00) => a & b,                       // and
                // RV32M: results are 32-bit and stored sign-extended. RV64M
                // runs the same rules on full 64-bit operands.
                (0, 0x01) if m.rv64() => (a as i64).wrapping_mul(b as i64) as u64, // mul
                // The signed products fit i128 exactly, so >> 64 is the
                // exact upper half (arithmetic for mulh/mulhsu).
                (1, 0x01) if m.rv64() => {
                    (((a as i64 as i128) * (b as i64 as i128)) >> 64) as u64 // mulh
                }
                (2, 0x01) if m.rv64() => {
                    (((a as i64 as i128) * (b as i128)) >> 64) as u64 // mulhsu
                }
                (3, 0x01) if m.rv64() => (((a as u128) * (b as u128)) >> 64) as u64, // mulhu
                (4, 0x01) if m.rv64() => {
                    // div/rem follow RISC-V's no-trap rules at 64-bit width.
                    let (x, y) = (a as i64, b as i64);
                    let v: i64 = if y == 0 {
                        -1
                    } else if x == i64::MIN && y == -1 {
                        i64::MIN
                    } else {
                        x / y
                    };
                    v as u64
                }
                (5, 0x01) if m.rv64() => {
                    // Divide by zero yields all ones instead of trapping.
                    a.checked_div(b).unwrap_or(u64::MAX)
                }
                (6, 0x01) if m.rv64() => {
                    let (x, y) = (a as i64, b as i64);
                    let v: i64 = if y == 0 {
                        x
                    } else if x == i64::MIN && y == -1 {
                        0
                    } else {
                        x % y
                    };
                    v as u64
                }
                (7, 0x01) if m.rv64() => {
                    let (x, y) = (a, b);
                    if y == 0 { x } else { x % y }
                }
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
        0x1b => {
            // RV64 op-imm-32: add the immediate / shift within 32 bits,
            // then sign-extend the word result to 64.
            let a = m.regs[rs1(w)];
            let v = match (w >> 12) & 0x7 {
                0 => sx32(a.wrapping_add(imm_i(w)) as u32), // addiw
                1 => sx32((a as u32) << shamt(w)), // slliw
                5 => {
                    if (w >> 25) & 0x20 != 0 {
                        sx32((((a as u32) as i32) >> shamt(w)) as u32) // sraiw
                    } else {
                        sx32((a as u32) >> shamt(w)) // srliw
                    }
                }
                f => bail!(format!("invalid op-imm-32 funct3 {f}")),
            };
            m.write_reg(rd(w), v, &mut changes);
        }
        0x3b => {
            // RV64 op-32 (w-suffixes) and the M w-suffixes: 32-bit compute,
            // then sign-extend.
            let a = m.regs[rs1(w)];
            let b = m.regs[rs2(w)];
            let v = match ((w >> 12) & 0x7, (w >> 25) & 0x3f) {
                (0, 0x00) => sx32(a.wrapping_add(b) as u32),           // addw
                (0, 0x20) => sx32(a.wrapping_sub(b) as u32),           // subw
                (1, 0x00) => sx32((a << (b & 0x1f)) as u32),           // sllw
                (5, 0x00) => sx32((a >> (b & 0x1f)) as u32),           // srlw
                (5, 0x20) => sx32((((a as u32) as i32) >> (b & 0x1f)) as u32), // sraw
                (0, 0x01) => sx32((a as u32).wrapping_mul(b as u32)),  // mulw
                (4, 0x01) => {
                    let (x, y) = (a as u32 as i32, b as u32 as i32);
                    let r: i32 = if y == 0 {
                        -1
                    } else if x == i32::MIN && y == -1 {
                        i32::MIN
                    } else {
                        x / y
                    };
                    sx32(r as u32) // divw
                }
                (5, 0x01) => {
                    let r = (a as u32).checked_div(b as u32).unwrap_or(u32::MAX);
                    sx32(r) // divuw
                }
                (6, 0x01) => {
                    let (x, y) = (a as u32 as i32, b as u32 as i32);
                    let r: i32 = if y == 0 {
                        x
                    } else if x == i32::MIN && y == -1 {
                        0
                    } else {
                        x % y
                    };
                    sx32(r as u32) // remw
                }
                (7, 0x01) => {
                    let (x, y) = (a as u32, b as u32);
                    sx32(if y == 0 { x } else { x % y }) // remuw
                }
                (f, f7) => bail!(format!("invalid op-32 funct3 {f} funct7 {f7}")),
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
                    return ExecOutcome { outcome: step, terminated_now, exception: None };
                } else if imm12 == 1 {
                    m.terminated = Some(Halt::Ebreak);
                    return ExecOutcome::halt(Halt::Ebreak, events, pc_before);
                } else if imm12 == 0x002 {
                    // uret: the user-mode return RARS supports.
                    m.do_uret(&mut changes);
                } else if imm12 == 0x105 {
                    // wfi parks the hart until an interrupt is deliverable;
                    // with one already pending it is a plain no-op that
                    // retires. Parking rewinds pc (the instruction has not
                    // completed) and sets the waiting flag; the step returns
                    // without retiring, and the run loop handles the wait.
                    if m.deliverable_interrupt().is_none() {
                        m.pc = pc_before;
                        m.waiting = true;
                        return ExecOutcome {
                            outcome: StepOutcome { executed: false, pc_before, events, changes },
                            terminated_now: false,
                            exception: None,
                        };
                    }
                } else {
                    bail!("invalid system instruction".to_string());
                }
            } else {
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
        // fcvt.w.s / fcvt.wu.s / fcvt.w.d / fcvt.wu.d (rs2 0/1) produce
        // sign-extended 32-bit results; fcvt.l.s / fcvt.lu.s / fcvt.l.d /
        // fcvt.lu.d (rs2 2/3, RV64 only) produce full 64-bit results.
        // Float-to-int conversions bake RTZ, funct3 the rounding mode.
        48 => {
            let Some(rm) = fp_rm(m, f3) else {
                return Err(format!("invalid rounding mode {f3}"));
            };
            if rs2(w) > 3 {
                return Err(format!("invalid fcvt rs2 {}", rs2(w)));
            }
            if rs2(w) >= 2 && !m.rv64() {
                return Err("64-bit fcvt forms require RV64".to_string());
            }
            let unsigned = rs2(w) & 1 == 1;
            let a = m.fregs[rs1(w)];
            let (res, flags) = match rs2(w) {
                2 | 3 => fp::cvt_to_int64(a, dbl, rm, unsigned),
                _ => fp::cvt_to_int(if dbl { a } else { fp::single_bits(a) as u64 }, dbl, rm, unsigned),
            };
            m.write_reg(rd, res, changes);
            m.acc_fflags(flags, changes);
        }
        // fcvt.s.w / fcvt.s.wu / fcvt.d.w / fcvt.d.wu take 32-bit sources;
        // fcvt.s.l / fcvt.s.lu / fcvt.d.l / fcvt.d.lu (rs2 2/3, RV64 only)
        // convert from full 64-bit integers.
        52 => {
            let Some(rm) = fp_rm(m, f3) else {
                return Err(format!("invalid rounding mode {f3}"));
            };
            if rs2(w) > 3 {
                return Err(format!("invalid fcvt rs2 {}", rs2(w)));
            }
            if rs2(w) >= 2 && !m.rv64() {
                return Err("64-bit fcvt forms require RV64".to_string());
            }
            let unsigned = rs2(w) & 1 == 1;
            let v = m.regs[rs1(w)];
            match (rs2(w), dbl) {
                (2 | 3, false) => {
                    let (bits, flags) = fp::cvt_int64_to_f32(v, unsigned, rm);
                    m.write_freg(rd, fp::box_single(bits), changes);
                    m.acc_fflags(flags, changes);
                }
                (2 | 3, true) => {
                    let (bits, flags) = fp::cvt_int64_to_f64(v, unsigned, rm);
                    m.write_freg(rd, bits, changes);
                    m.acc_fflags(flags, changes);
                }
                (_, true) => {
                    m.write_freg(rd, fp::cvt_int_to_f64(v, unsigned), changes);
                }
                (_, false) => {
                    let (bits, flags) = fp::cvt_int_to_f32(v, unsigned, rm);
                    m.write_freg(rd, fp::box_single(bits), changes);
                    m.acc_fflags(flags, changes);
                }
            }
        }
        // fmv.x.s (funct3 0) and fclass (funct3 1): integer destinations.
        // fmv.x.d (funct3 0, double) moves all 64 bits across.
        56 => {
            let a = m.fregs[rs1(w)];
            match (f3, dbl) {
                (0, false) => {
                    // Bit move: sign-extended per the 32-bit convention.
                    m.write_reg(rd, fp::single_bits(a) as i32 as i64 as u64, changes);
                }
                (0, true) => {
                    // fmv.x.d: the full doubleword moves unchanged.
                    m.write_reg(rd, a, changes);
                }
                (1, _) => {
                    let mask = if dbl { fp::classify64(a) } else { fp::classify32(fp::single_bits(a)) };
                    m.write_reg(rd, mask, changes);
                }
                _ => return Err(format!("invalid fmv.x/fclass encoding (funct3 {f3})")),
            }
        }
        // fmv.s.x boxes the low word of the integer register into the FP
        // register; fmv.d.x (double) moves all 64 bits unboxed.
        60 => {
            if f3 != 0 {
                return Err("invalid fmv.x encoding".to_string());
            }
            if dbl {
                m.write_freg(rd, m.regs[rs1(w)], changes);
            } else {
                m.write_freg(rd, fp::box_single(m.regs[rs1(w)] as u32), changes);
            }
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

    // ---- RV64 mode (MachineConfig::rv64) ----

    #[test]
    fn rv64_wide_li_materializes_exact_values() {
        let src = "\
    li t0, 1000000000000000
    li a0, 9223372036854775807
    addi a0, a0, 1          # wraps to i64::MIN
    li a1, -1
    add a2, a1, a1          # 64-bit -2
";
        let mut m = machine64(src);
        m.run(None);
        assert_eq!(m.reg(5), 1_000_000_000_000_000); // t0: the 8-instruction RARS chain
        assert_eq!(m.reg(10), 0x8000_0000_0000_0000); // add carry past bit 63
        assert_eq!(m.reg(12), 0xffff_ffff_ffff_fffe);
    }

    #[test]
    fn rv64_add_carry_past_bit_32() {
        let src = "\
    li t0, 0xffffffff       # 2^32-1 via the wide chain (exact, positive)
    li t1, 1
    add a0, t0, t1          # carry out of bit 31...
    add a1, a0, a0          # ...and arithmetic running past bit 32
";
        let mut m = machine64(src);
        m.run(None);
        assert_eq!(m.reg(10), 0x1_0000_0000);
        assert_eq!(m.reg(11), 0x2_0000_0000);
    }

    #[test]
    fn rv64_ld_sd_roundtrip() {
        // sp is 4-aligned by the fixed memory map, so the doubleword goes at
        // sp-4 (8-aligned) like a real program would after `addi sp, sp, -8`.
        let src = "\
    li t0, 0x123456789abcdef
    sd t0, -4(sp)
    ld a0, -4(sp)
    ld a1, 4(sp)            # untouched memory reads zero
";
        let mut m = machine64(src);
        m.run(None);
        assert_eq!(m.reg(10), 0x0123_4567_89ab_cdef);
        assert_eq!(m.reg(11), 0);
        // The doubleword really sits in memory, little-endian.
        let mut buf = [0u8; 8];
        let sp = m.reg(2) as u32;
        m.peek_bytes(sp - 4, &mut buf).unwrap();
        assert_eq!(buf, 0x0123_4567_89ab_cdefu64.to_le_bytes());
    }

    #[test]
    fn rv64_addiw_and_w_ops_sign_extend() {
        let src = "\
    li t0, 0x80000000       # exact 64-bit 2^31 (positive)
    addiw a0, t0, 0         # word result sign-extends
    li t1, 1
    addw a1, t0, t1         # 0x80000001 -> 0xffffffff80000001
    subw a2, t1, t0         # 1 - 2^31 in 32 bits -> same pattern
    slliw a3, t1, 31        # 1 << 31 sign-extended
    sraiw a4, t0, 31        # arithmetic shift fills ones
    srliw a5, t0, 31        # logical shift fills zeros
    mulw a6, t0, t0         # low 32 bits are zero
    divw a7, t0, t1         # 2^31 / 1, sign-extended
    li t2, -1
    divw s0, t0, t2         # MIN / -1 overflow returns the dividend
    remw s1, t0, t2         # ...with remainder 0
    remuw s2, t1, t0        # 1 % 2^31 = 1
    li t3, 31
    sllw s3, t1, t3         # R-type register forms too
    sraw s4, t0, t3
";
        let mut m = machine64(src);
        m.run(None);
        let want_sx = 0xffff_ffff_8000_0001u64;
        assert_eq!(m.reg(10), 0xffff_ffff_8000_0000); // addiw
        assert_eq!(m.reg(11), want_sx); // addw
        assert_eq!(m.reg(12), want_sx); // subw
        assert_eq!(m.reg(13), 0xffff_ffff_8000_0000); // slliw
        assert_eq!(m.reg(14), u64::MAX); // sraiw: -1
        assert_eq!(m.reg(15), 1); // srliw
        assert_eq!(m.reg(16), 0); // mulw
        assert_eq!(m.reg(17), 0xffff_ffff_8000_0000); // divw
        assert_eq!(m.reg(8), 0xffff_ffff_8000_0000); // divw overflow
        assert_eq!(m.reg(9), 0); // remw after overflow
        assert_eq!(m.reg(18), 1); // remuw
        assert_eq!(m.reg(19), 0xffff_ffff_8000_0000); // sllw (register amount)
        assert_eq!(m.reg(20), u64::MAX); // sraw (register amount)
    }

    #[test]
    fn rv64_lwu_zero_extends_while_lw_sign_extends() {
        let src = "\
    li t0, -1
    sw t0, 0(sp)
    lw a0, 0(sp)
    lwu a1, 0(sp)
";
        let mut m = machine64(src);
        m.run(None);
        assert_eq!(m.reg(10), u64::MAX);
        assert_eq!(m.reg(11), 0x0000_0000_ffff_ffff);
    }

    #[test]
    fn rv64_shifts_beyond_31() {
        let src = "\
    li a1, 1
    slli a0, a1, 40
    srli a2, a0, 36
    srai a3, a1, 1
    li t0, -1024
    srai a4, t0, 3
    li t1, 40
    sll a5, a1, t1          # register shift uses six bits in RV64
    sra a6, t0, t1
";
        let mut m = machine64(src);
        m.run(None);
        assert_eq!(m.reg(10), 1u64 << 40);
        assert_eq!(m.reg(12), 0x10);
        assert_eq!(m.reg(13), 0);
        assert_eq!(m.reg(14), (-128i64) as u64);
        assert_eq!(m.reg(15), 1u64 << 40);
        assert_eq!(m.reg(16), (-1i64) as u64); // -1024 >> 40 arithmetic
    }

    #[test]
    fn rv64_branches_compare_full_width() {
        let src = "\
    li a0, -1               # 0xffffffffffffffff
    li a1, 1
    li t0, 0
    blt a0, a1, L1          # signed: -1 < 1 -> taken
    addi t0, t0, 1
L1: bgeu a0, a1, L2         # unsigned: huge >= 1 -> taken
    addi t0, t0, 10
L2: bltu a0, a1, L3         # unsigned: huge < 1 -> not taken
    addi t0, t0, 100
L3: beq a0, a1, L4          # full-width equality -> not taken
    addi t0, t0, 1000
L4: sw t0, 0(sp)
";
        let mut m = machine64(src);
        m.run(None);
        assert_eq!(m.reg(5), 1100);
    }

    #[test]
    fn rv64_div_rem_mul_64bit() {
        let src = "\
    li a0, -8
    li a1, 3
    div a2, a0, a1
    rem a3, a0, a1
    li t0, -1
    mul a4, t0, t0
    mulh a5, t0, t0
    li t1, 0x4000000000000000
    mulh a6, t1, t1         # 2^62 * 2^62 >> 64 = 2^60
    mulhu a7, t1, t1
    li t2, 9223372036854775807
    addi t2, t2, 1          # i64::MIN
    li t3, -1
    div t4, t2, t3          # overflow rule: quotient = i64::MIN
    rem t5, t2, t3          # ...remainder 0
    divu t6, t0, t0
";
        let mut m = machine64(src);
        m.run(None);
        assert_eq!(m.reg(12), (-2i64) as u64); // a2: div
        assert_eq!(m.reg(13), (-2i64) as u64); // a3: rem
        assert_eq!(m.reg(14), 1); // a4: mul(-1, -1)
        assert_eq!(m.reg(15), 0); // a5: mulh(-1, -1)
        assert_eq!(m.reg(16), 1u64 << 60); // a6: mulh(2^62, 2^62)
        assert_eq!(m.reg(17), 1u64 << 60); // a7: mulhu agrees
        assert_eq!(m.reg(7), i64::MIN as u64); // t2: addi wrap to i64::MIN
        assert_eq!(m.reg(29), i64::MIN as u64); // t4: div overflow rule
        assert_eq!(m.reg(30), 0); // t5: matching remainder
        assert_eq!(m.reg(31), 1); // t6: divu all-ones / all-ones
    }

    #[test]
    fn rv64_slt_compares_full_width() {
        let src = "\
    li a0, -1
    li a1, 1
    slt a2, a0, a1
    sltu a3, a0, a1
    sltiu a4, a1, -1
    slti a5, a0, 0
";
        let mut m = machine64(src);
        m.run(None);
        assert_eq!(m.reg(12), 1); // -1 < 1 signed
        assert_eq!(m.reg(13), 0); // 2^64-1 < 1 unsigned is false
        assert_eq!(m.reg(14), 1); // 1 < sign-extended -1 unsigned
        assert_eq!(m.reg(15), 1); // -1 < 0
    }

    #[test]
    fn rv64_lui_and_auipc_sign_extend() {
        let src = "\
    lui a0, 0xfffff
    auipc a1, 0x80000
";
        let mut m = machine64(src);
        m.step();
        assert_eq!(m.reg(10), 0xffff_ffff_ffff_f000);
        m.step();
        // auipc: pc + sign-extended immediate, wrapping at 64 bits.
        let pc1 = u64::from(m.program().text_base + 4);
        assert_eq!(m.reg(11), pc1.wrapping_add(0xffff_ffff_8000_0000));
    }

    #[test]
    fn rv64_floating_point_bit_moves_and_truncations() {
        let src = "\
.data
d:   .double 3.75
nd:  .double -2.5
s:   .float -2.9
.text
    fld f1, d
    fcvt.l.d a0, f1         # 3 (RTZ baked, 64-bit result)
    fcvt.lu.d a1, f1        # 3
    fld f2, nd
    fcvt.l.d a2, f2         # -2
    flw f3, s
    fcvt.l.s a3, f3         # -2 from a single
    fmv.x.d a4, f1          # raw double bits move unchanged
    li a5, 100
    fmv.d.x f4, a5
    fmv.x.d a6, f4          # 100 back out through the double bit-move
";
        let mut m = machine64(src);
        m.run(None);
        assert_eq!(m.reg(10), 3);
        assert_eq!(m.reg(11), 3);
        assert_eq!(m.reg(12), (-2i64) as u64);
        assert_eq!(m.reg(13), (-2i64) as u64);
        assert_eq!(m.reg(14), 3.75f64.to_bits());
        assert_eq!(m.reg(16), 100);
        assert_eq!(m.freg(4), 100); // fmv.d.x stores raw, unboxed bits
    }

    #[test]
    fn rv64_fp_conversions_round_trip_wide_integers() {
        let wide = 123456789123456789i64;
        let src = format!(
            "\
.data
big: .double {wide}
.text
    fld f1, big
    fcvt.l.d a0, f1         # 64-bit integer result
    fmv.x.d a1, f1          # raw bits move unchanged
    li a2, {wide}
    fcvt.d.l f2, a2         # int -> double (rounds to the f64 grid)
    fcvt.l.d a3, f2
    fsd f2, res
.data
res: .double 0
"
        );
        let mut m = machine64(&src);
        m.run(None);
        let rounded = wide as f64;
        assert_eq!(m.reg(10), rounded as i64 as u64);
        assert_eq!(m.reg(11), rounded.to_bits());
        assert_eq!(m.reg(13), rounded as i64 as u64);
        // The stored double equals the f64 bits of the source integer.
        let mut buf = [0u8; 8];
        let addr = m.program().symbols.get("res").unwrap().addr;
        m.peek_bytes(addr, &mut buf).unwrap();
        assert_eq!(buf, rounded.to_bits().to_le_bytes());
    }

    #[test]
    fn rv64_fcvt_overflow_saturates_64bit() {
        let src = "\
.data
huge: .double 1e30
nan:  .double nan
.text
    fld f1, huge
    fcvt.l.d a0, f1         # -> i64::MAX + NV
    fcvt.lu.d a1, f1        # -> u64::MAX + NV
    fld f2, nan
    fcvt.l.d a2, f2         # NaN -> i64::MAX + NV
";
        let mut m = machine64(src);
        m.run(None);
        assert_eq!(m.reg(10), i64::MAX as u64);
        assert_eq!(m.reg(11), u64::MAX);
        assert_eq!(m.reg(12), i64::MAX as u64);
        assert_eq!(m.csr(csr::FFLAGS), fp::NV as u64);
    }

    #[test]
    fn rv64_fcvt_int_to_float_flags_and_values() {
        // 2^62 + 1 is inexact on the f64 grid (span 63 > 53).
        let v = (1u64 << 62) + 1;
        let src = format!(
            "\
    li a0, {v}
    fcvt.d.l f0, a0
    fcvt.l.d a1, f0
    li a3, 1000000
    fcvt.d.l f1, a3         # exact, no flags
    fcvt.s.l f2, a3
    fmv.x.s a4, f2
"
        );
        let mut m = machine64(&src);
        m.run(None);
        assert_eq!(m.reg(11), v as f64 as i64 as u64); // a1: round-tripped
        assert_eq!(m.csr(csr::FFLAGS), fp::NX as u64);
        // 10^6 fits f32's 24-bit mantissa exactly.
        assert_eq!(m.reg(14), 1_000_000f32.to_bits() as i32 as i64 as u64); // a4
    }
}
