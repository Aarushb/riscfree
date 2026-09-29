//! Fetch-decode-execute for RV32I. The machine's write helpers record undo
//! entries and change events, so this file stays a plain decode `match`.

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
                (f, f7) => bail!(format!("invalid op funct3 {f} funct7 {f7}")),
            };
            m.write_reg(rd(w), v, &mut changes);
        }
        0x0f => {
            // fence: no-op in this sequential model, matching RARS.
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

#[cfg(test)]
mod tests {
    use crate::testutil::*;

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
}
