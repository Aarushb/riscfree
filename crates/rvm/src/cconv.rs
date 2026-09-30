//! Calling-convention checker, opt-in via the `check_calling_convention`
//! config flag (inspired by Venus's CS61C-style checker): the skill courses
//! drill hardest is writing functions that leave registers as they found
//! them, so the machine keeps a call stack of activation records and
//! verifies, at every return, that the callee restored the two things the
//! RISC-V convention makes its responsibility — `$sp` and the s registers.
//!
//! Detection is decode-shaped, not flow-shaped: a CALL is any `jal`/`jalr`
//! that writes x1 (ra) — writing a fresh return address is what "calling"
//! means here — and a RETURN is any `jalr x0` (`ret` assembles to
//! `jalr x0, 0(ra)`; `jr` and `tail` share the shape). On CALL the record
//! snapshots `$sp` and the s registers; on RETURN the innermost record is
//! popped and compared, and any mismatch halts with a read-aloud
//! description after the return retires (a debugger pause like a
//! watchpoint, not a termination). Recursion is free: each call pushes its
//! own record, each return pops one.
//!
//! Documented simplifications, accepted because the check is advisory for
//! teaching:
//! - Tail calls through `tail`/`jr` are return-shaped, so a *conformant*
//!   tail call (nothing clobbered before the jump) actually checks cleanly;
//!   but an abandoned call that never returns simply leaves its record
//!   sitting on the stack — harmless, because a pop-from-empty passes.
//! - A function that overwrites ra with a non-return target and jumps
//!   through it (rd = x1) reads as another CALL, and a jump-table
//!   `jr tN` (rd = x0) reads as a RETURN; dispatch-heavy code can confuse
//!   the record matching.

use crate::{Halt, Machine, Undo};

/// The registers a conformant callee must preserve, in ABI order s0..s11
/// (s0/s1 are x8/x9, s2-s11 are x18-x27).
const S_REGS: [usize; 12] = [8, 9, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27];

/// ABI names for the saved registers, for read-aloud descriptions.
const S_NAME: [&str; 12] = [
    "s0", "s1", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11",
];

/// One activation record: what the callee promised to hand back, snapshotted
/// when a call wrote ra.
#[derive(Debug, Clone)]
pub(crate) struct Frame {
    /// `$sp` at the call; a conformant callee returns with it unchanged.
    sp: u64,
    /// The s-register values at the call, ordered as `S_REGS`.
    s: [u64; 12],
    /// pc of the call instruction itself; the instruction after it is where
    /// the return lands, which is the "return to line" in the description.
    call_pc: u32,
}

impl Machine {
    /// A `jal`/`jalr` just jumped: `opcode` is the encoding's bits 6:0 and
    /// `rd` the link register it wrote. With the checker on, rd = x1 is a
    /// CALL (snapshot) and a *jalr* x0 is a RETURN (verify) — `j` is also
    /// jal x0, so the opcode decides return-ness, not just rd. Runs after
    /// the register write so the journal records the checker's flip inside
    /// the instruction's own statement — one backstep undoes both, the way
    /// memcheck's shadow flips ride the store. Zero cost when disabled.
    pub(crate) fn check_call_site(&mut self, pc_before: u32, opcode: u32, rd: usize) {
        if !self.config.check_calling_convention {
            return;
        }
        if rd == 1 {
            // CALL: remember the world as the callee receives it.
            let s = std::array::from_fn(|i| self.regs[S_REGS[i]]);
            self.callstack.push(Frame {
                sp: self.regs[2],
                s,
                call_pc: pc_before,
            });
            self.journal.push(Undo::CcPush);
        } else if opcode == 0x67 && rd == 0 {
            // RETURN. An empty stack passes: main returning to the loader
            // was never a simulated call, so pop-from-empty is a no-op.
            let Some(frame) = self.callstack.pop() else {
                return;
            };
            let mut broken: Vec<String> = Vec::new();
            for (i, &reg) in S_REGS.iter().enumerate() {
                let now = self.regs[reg];
                if now != frame.s[i] {
                    broken.push(format!(
                        "{} was {} at call, now {}",
                        S_NAME[i], frame.s[i], now
                    ));
                }
            }
            let sp_now = self.regs[2];
            if sp_now != frame.sp {
                broken.push(format!("sp was {:#x}, now {:#x}", frame.sp, sp_now));
            }
            // Where the return lands: the instruction after the call. Taken
            // before the record moves into its undo entry.
            let resume = frame.call_pc.wrapping_add(4);
            // Journal the pop whatever the verdict, so a backstep pushes the
            // record back and a rerun of this return replays identically.
            self.journal.push(Undo::CcPop(Box::new(frame)));
            if broken.is_empty() || self.pending_stop.is_some() {
                // Conformant, or another checker already owns this
                // instruction's single stop slot; the record bookkeeping
                // above is done either way.
                return;
            }
            let at = match self.program.statement_at(resume) {
                Some(st) => format!(" on return to line {}", st.source.line),
                None => " on return".to_string(),
            };
            let description = format!("calling convention violation{at}: {}", broken.join("; "));
            self.pending_stop = Some(Halt::CallingConvention { description });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use crate::{Event, MachineConfig, ScriptHost};

    const EXIT: Halt = Halt::Exit { code: 0 };

    fn machine_cfg(src: &str, cfg: MachineConfig) -> Machine {
        let r = asm(src);
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        Machine::new(r.program.unwrap(), Box::new(ScriptHost::default()), cfg)
    }

    fn cconv_cfg() -> MachineConfig {
        MachineConfig {
            check_calling_convention: true,
            ..MachineConfig::default()
        }
    }

    fn stop_text(m: &Machine) -> Option<String> {
        match m.halt_reason() {
            Some(Halt::CallingConvention { description }) => Some(description.clone()),
            _ => None,
        }
    }

    /// main keeps a live value in s0 across a call; `clobber` says whether
    /// work() trashes s0, `sp_drift` says whether its epilogue restores sp.
    fn caller_src(clobber: bool, sp_drift: bool) -> String {
        let restore = if sp_drift {
            "addi sp, sp, 4"
        } else {
            "addi sp, sp, 8"
        };
        let clobber_line = if clobber { "    li s0, 7\n" } else { "" };
        format!(
            "\
main:
    li s0, 42
    jal ra, work
    li a7, 10
    ecall
work:
    addi sp, sp, -8
    sw ra, 0(sp)
{clobber_line}    lw ra, 0(sp)
{restore}
    ret
"
        )
    }

    #[test]
    fn conformant_function_passes() {
        // Proper prologue/epilogue: sp restored and the clobbered s0 handed
        // back exactly as the caller left it.
        let src = "\
main:
    li s0, 42
    li a0, 5
    jal ra, work
    li a7, 10
    ecall
work:
    addi sp, sp, -8
    sw ra, 4(sp)
    sw s0, 0(sp)
    li s0, 7               # clobber, then restore
    lw s0, 0(sp)
    lw ra, 4(sp)
    addi sp, sp, 8
    ret
";
        let mut m = machine_cfg(src, cconv_cfg());
        let events = m.run(None);
        assert_eq!(events.last(), Some(&Event::Halted(EXIT)));
        // Every call was matched by a verifying return: the stack drained.
        assert!(m.callstack.is_empty());
    }

    #[test]
    fn call_pseudo_is_detected_as_a_call() {
        // `call work` expands to auipc ra + jalr ra: the jalr's ra write is
        // the call signal, and the conformant callee passes the check.
        let src = "\
main:
    li s0, 9
    call work
    li a7, 10
    ecall
work:
    addi sp, sp, -4
    sw s0, 0(sp)
    li s0, 1
    lw s0, 0(sp)
    addi sp, sp, 4
    ret
";
        let mut m = machine_cfg(src, cconv_cfg());
        let events = m.run(None);
        assert_eq!(events.last(), Some(&Event::Halted(EXIT)));
        assert!(m.callstack.is_empty());
    }

    #[test]
    fn clobbered_s0_halts_naming_old_and_new_values() {
        let mut m = machine_cfg(&caller_src(true, false), cconv_cfg());
        let events = m.run(None);
        assert!(matches!(
            events.last(),
            Some(Event::Halted(Halt::CallingConvention { .. }))
        ));
        // jal sits on line 3, so the return lands on line 4 (li a7, 10).
        assert_eq!(
            stop_text(&m).as_deref(),
            Some("calling convention violation on return to line 4: s0 was 42 at call, now 7"),
            "{:?}",
            stop_text(&m)
        );
    }

    #[test]
    fn sp_corruption_on_return_halts_naming_both_values() {
        let mut m = machine_cfg(&caller_src(false, true), cconv_cfg());
        let events = m.run(None);
        assert!(matches!(
            events.last(),
            Some(Event::Halted(Halt::CallingConvention { .. }))
        ));
        // jal sits on line 3, so the return lands on line 4; work restored
        // ra faithfully but came back with sp four bytes short.
        assert_eq!(
            stop_text(&m).as_deref(),
            Some("calling convention violation on return to line 4: sp was 0x7fffeff8, now 0x7fffeff4"),
            "{:?}",
            stop_text(&m)
        );
    }

    #[test]
    fn every_s_register_is_checked() {
        // Clobber s0, s1, and s11 in one go; the report lists each with its
        // own old/new pair before the sp finding.
        let src = "\
main:
    jal ra, work
    li a7, 10
    ecall
work:
    li s0, 1
    li s1, 2
    li s11, 3
    ret
";
        let mut m = machine_cfg(src, cconv_cfg());
        let events = m.run(None);
        assert!(matches!(
            events.last(),
            Some(Event::Halted(Halt::CallingConvention { .. }))
        ));
        // jal sits on line 2, so the return lands on line 3 (li a7, 10).
        assert_eq!(
            stop_text(&m).as_deref(),
            Some(
                "calling convention violation on return to line 3: \
                 s0 was 0 at call, now 1; s1 was 0 at call, now 2; s11 was 0 at call, now 3"
            ),
            "{:?}",
            stop_text(&m)
        );
    }

    #[test]
    fn recursion_passes_when_conformant() {
        // 5! = 120 through five nested frames; each level snapshots and
        // restores s0, and each return verifies its own record.
        let src = "\
main:
    li a0, 5
    jal ra, fact
    li a7, 1
    ecall
    li a7, 10
    ecall
fact:
    li t0, 1
    beq a0, t0, base
    addi sp, sp, -8
    sw ra, 4(sp)
    sw s0, 0(sp)
    mv s0, a0
    addi a0, a0, -1
    jal ra, fact
    mul a0, a0, s0
    lw ra, 4(sp)
    lw s0, 0(sp)
    addi sp, sp, 8
    ret
base:
    li a0, 1
    ret
";
        let host = ScriptHost::default();
        let r = asm(src);
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let mut m = Machine::new(r.program.unwrap(), Box::new(host.clone()), cconv_cfg());
        let events = m.run(None);
        assert_eq!(events.last(), Some(&Event::Halted(EXIT)));
        assert_eq!(host.take_output(), "120");
        assert!(m.callstack.is_empty());
    }

    #[test]
    fn recursion_violation_checks_against_the_innermost_frame() {
        // fact(2) nests two calls. The inner record (pushed by line 15) holds
        // s0 = 2 and sp = 0x7fffeff4; the outer holds s0 = 5 and the initial
        // sp. The base case clobbers s0 and leaves its own -8 in place, so
        // its ret reports BOTH findings measured against the *inner* record
        // — proving the pop verified the innermost level, not the outer.
        let src = "\
main:
    li s0, 5
    li a0, 2
    jal ra, fact
    li a7, 10
    ecall
fact:
    addi sp, sp, -8
    sw ra, 4(sp)
    sw s0, 0(sp)
    li t0, 1
    beq a0, t0, base
    mv s0, a0
    addi a0, a0, -1
    jal ra, fact
    mul a0, a0, s0
    lw ra, 4(sp)
    lw s0, 0(sp)
    addi sp, sp, 8
    ret
base:
    li s0, 99
    li a0, 1
    ret
";
        let mut m = machine_cfg(src, cconv_cfg());
        let events = m.run(None);
        assert!(matches!(
            events.last(),
            Some(Event::Halted(Halt::CallingConvention { .. }))
        ));
        // The base ret popped the inner record: the outer record stays.
        assert_eq!(m.callstack.len(), 1);
        assert_eq!(
            stop_text(&m).as_deref(),
            Some(
                "calling convention violation on return to line 16: \
                 s0 was 2 at call, now 99; sp was 0x7fffeff0, now 0x7fffefe8"
            ),
            "{:?}",
            stop_text(&m)
        );
    }

    #[test]
    fn ret_before_any_call_passes() {
        // main returning to the loader pops from an empty stack: no record,
        // no check, no halt — the run ends dropped-off, not in violation.
        let mut m = machine_cfg("main:\n    li a0, 3\n    ret\n", cconv_cfg());
        let events = m.run(None);
        assert_eq!(events.last(), Some(&Event::Halted(Halt::DroppedOff)));
    }

    #[test]
    fn disabled_config_never_halts() {
        // Default is off; the same program that violates above exits cleanly.
        let mut m = machine_cfg(&caller_src(true, false), MachineConfig::default());
        let events = m.run(None);
        assert_eq!(events.last(), Some(&Event::Halted(EXIT)));
    }

    #[test]
    fn abandoned_call_never_flags_on_its_own() {
        // The simplification: a callee that never returns leaves its record
        // stacked, and nothing halts until some future return pops it.
        let mut m = machine_cfg("main:\n    jal ra, work\nwork:\n    j work\n", cconv_cfg());
        let events = m.run(Some(50));
        assert_eq!(events.last(), Some(&Event::Halted(Halt::Limit)));
        assert_eq!(m.callstack.len(), 1);
    }

    #[test]
    fn backstep_after_violation_replays_identically() {
        let mut m = machine_cfg(&caller_src(true, false), cconv_cfg());
        m.run(None);
        let desc = stop_text(&m).unwrap();
        // Like a watchpoint stop, backstep undoes the ret (effect and all)
        // and clears the halt; the CcPop undo pushes the record back.
        let ret_pc = m.program().statements.last().unwrap().addr;
        assert!(m.backstep());
        assert!(!m.is_terminated());
        assert_eq!(m.pc(), ret_pc);
        assert_eq!(m.callstack.len(), 1);
        // Re-running the same ret reproduces the identical stop.
        m.step();
        assert_eq!(stop_text(&m).as_deref(), Some(desc.as_str()));
    }

    #[test]
    fn backstep_through_the_call_empties_the_stack() {
        // Backstepping all the way over the call itself: the CcPush undo
        // pops the record, so state matches a machine that never called.
        let mut m = machine_cfg(&caller_src(true, false), cconv_cfg());
        m.run(None);
        assert!(m.backstep()); // undo the ret
        assert_eq!(m.callstack.len(), 1);
        // Undo work's five body instructions, then the jal itself.
        for _ in 0..6 {
            assert!(m.backstep());
        }
        assert!(m.callstack.is_empty());
        // Re-running forward re-enters and re-violates the same way.
        m.step(); // jal again
        assert_eq!(m.callstack.len(), 1);
        m.run(None);
        assert!(matches!(
            m.halt_reason(),
            Some(Halt::CallingConvention { .. })
        ));
    }

    #[test]
    fn continue_after_stop_resumes_past_the_violation() {
        // A violation stop is a debugger pause like a watchpoint: continue
        // runs on from the retired return and the program can finish.
        let mut m = machine_cfg(&caller_src(true, false), cconv_cfg());
        m.run(None);
        m.continue_after_stop();
        assert!(!m.is_terminated());
        let events = m.run(None);
        assert_eq!(events.last(), Some(&Event::Halted(EXIT)));
    }

    #[test]
    fn reset_clears_the_checker_call_stack() {
        let src = "\
main:
    li s0, 42
    jal ra, work
    li a7, 10
    ecall
work:
    addi sp, sp, -4
    sw ra, 0(sp)
    lw ra, 0(sp)
    addi sp, sp, 4
    ret
";
        let mut m = machine_cfg(src, cconv_cfg());
        // li s0, li a0, then the jal: one record is live.
        for _ in 0..3 {
            m.step();
        }
        assert_eq!(m.callstack.len(), 1);
        // Reset restores the post-assembly state, checker included.
        m.reset();
        assert!(m.callstack.is_empty());
        let events = m.run(None);
        assert_eq!(events.last(), Some(&Event::Halted(EXIT)));
    }

    #[test]
    fn jalr_indirect_call_is_detected() {
        // `jalr ra, 0(t0)` is the basic indirect-call form: rd = x1 through
        // the jalr arm of the detector, and the conformant callee passes.
        let src = "\
main:
    la t0, work
    jalr ra, 0(t0)
    li a7, 10
    ecall
work:
    addi sp, sp, -4
    sw ra, 0(sp)
    lw ra, 0(sp)
    addi sp, sp, 4
    ret
";
        let mut m = machine_cfg(src, cconv_cfg());
        let events = m.run(None);
        assert_eq!(events.last(), Some(&Event::Halted(EXIT)));
        assert!(m.callstack.is_empty());
    }

    #[test]
    fn plain_jump_is_neither_call_nor_return() {
        // `j` expands to jal x0: no ra write, no record pushed, so the
        // enclosing function can loop freely without confusing the checker.
        let src = "\
main:
    li t0, 3
loop:
    addi t0, t0, -1
    bgtz t0, loop
    li a7, 10
    ecall
";
        let mut m = machine_cfg(src, cconv_cfg());
        let events = m.run(None);
        assert_eq!(events.last(), Some(&Event::Halted(EXIT)));
        assert!(m.callstack.is_empty());
    }

    #[test]
    fn rv64_checks_the_same_rules_at_full_width() {
        // Same convention in 64-bit mode. The doubleword save sits at
        // -8(sp): the initial sp is 8-aligned, so a doubleword save needs
        // clobbered s0 goes unreported-back and halts the return.
        let src = "\
main:
    li s0, 42
    jal ra, work
    li a7, 10
    ecall
work:
    addi sp, sp, -8
    sd ra, -8(sp)
    li s0, 7             # clobber, never restored
    ld ra, -8(sp)
    addi sp, sp, 8
    ret
";
        let files = vec![rvasm::InputFile {
            name: "t.s".into(),
            source: src.into(),
        }];
        let acfg = rvasm::AsmConfig {
            rv64: true,
            ..rvasm::AsmConfig::default()
        };
        let r = rvasm::assemble(&files, &acfg);
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let mcfg = MachineConfig {
            rv64: true,
            check_calling_convention: true,
            ..MachineConfig::default()
        };
        let mut m = Machine::new(r.program.unwrap(), Box::new(ScriptHost::default()), mcfg);
        let events = m.run(None);
        assert!(matches!(
            events.last(),
            Some(Event::Halted(Halt::CallingConvention { .. }))
        ));
        assert_eq!(
            stop_text(&m).as_deref(),
            Some("calling convention violation on return to line 4: s0 was 42 at call, now 7"),
            "{:?}",
            stop_text(&m)
        );
    }
}
