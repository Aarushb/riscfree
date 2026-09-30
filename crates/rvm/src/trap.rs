//! Interrupt and trap delivery, RARS parity. A trap enters at a delivery
//! point between instructions: `ucause`/`uepc`/`utval` are written, `ustatus`
//! stacks UIE into UPIE, and the pc redirects through `utvec` (vectored mode
//! adds `4 * cause` for interrupts). Delivery is RARS's priority order —
//! external device sources, then software, then the timer.
//!
//! Enable model (a documented choice; RARS is loose here): an interrupt
//! needs its *device* enable bit AND the global `ustatus.UIE`. Pending state
//! is derived from the sources rather than latched where possible, so a
//! request that arrives while UIE is clear simply waits and fires when UIE
//! is set.

use crate::csr;
use crate::host::Host;
use crate::{Change, Halt, Machine, Undo};

/// Interrupt cause numbers, RARS's constants. Delivered causes also carry
/// the RISC-V interrupt bit (0x8000_0000) in `ucause`; these are the base
/// values used for vectored dispatch (`utvec base + 4 * cause`).
pub mod irq {
    /// Software interrupt (usip-style; no in-machine producer yet — tools
    /// will raise it later, so it exists in the priority ladder).
    pub const SOFTWARE: u32 = 0x01;
    /// The built-in instruction-count timer (RARS TIMER_INTERRUPT).
    pub const TIMER: u32 = 0x10;
    /// Keyboard/receiver device interrupt (RARS's MMIO keyboard).
    pub const KEYBOARD: u32 = 0x40;
    /// Display/transmitter device interrupt (RARS's MMIO display).
    pub const DISPLAY: u32 = 0x80;
    /// Generic external interrupt (reserved for tool timers and friends).
    pub const EXTERNAL: u32 = 0x100;
    /// Hex-keyboard device interrupt (Digital Lab Sim; reserved here).
    pub const HEX_KEYS: u32 = 0x200;

    /// The interrupt bit set in `ucause` for interrupt traps.
    pub const INTERRUPT_BIT: u64 = 0x8000_0000;
}

/// Synchronous exception causes, RISC-V/RARS exception numbers.
pub(crate) mod exc {
    pub const INSN_MISALIGNED: u32 = 0;
    pub const ILLEGAL_INSN: u32 = 2;
    pub const LOAD_MISALIGNED: u32 = 4;
    pub const LOAD_FAULT: u32 = 5;
    pub const STORE_MISALIGNED: u32 = 6;
    pub const STORE_FAULT: u32 = 7;
}

impl Machine {
    /// True while `ustatus.UIE` is set — the global interrupt gate.
    pub(crate) fn uie(&self) -> bool {
        self.read_csr(csr::USTATUS) & 0x1 != 0
    }

    /// Timer pending: armed and the retired-instruction clock passed the
    /// deadline. Level-shaped here so a request raised while UIE is clear
    /// survives until it becomes deliverable.
    pub(crate) fn timer_pending(&self) -> bool {
        self.timer_interval.is_some() && self.instret >= self.timer_deadline
    }

    /// The highest-priority deliverable interrupt, if any. Delivery is gated
    /// on `ustatus.UIE` plus each device's own enable bit; within the
    /// external class the lowest cause wins (RARS polls devices in
    /// registration order — this is our deterministic stand-in).
    pub(crate) fn deliverable_interrupt(&self) -> Option<u32> {
        if !self.uie() {
            return None;
        }
        use irq::*;
        // External device class first...
        if self.mmio.recv_ie && self.mmio.has_input() {
            return Some(KEYBOARD);
        }
        if self.mmio.xmit_ie && self.mmio.xmit_edge_latched() {
            return Some(DISPLAY);
        }
        // The hex-keypad request is latched at press time (the device's own
        // enable gate already applied), so only the latch is polled here.
        if self.mmio.keys_pending {
            return Some(HEX_KEYS);
        }
        // ...then software, then the timer.
        if self.software_pending {
            return Some(SOFTWARE);
        }
        if self.timer_pending() {
            return Some(TIMER);
        }
        None
    }

    /// Refresh wake sources for a hart parked in `wfi`: poll the keyboard
    /// (interrupt mode only) and, if the timer is armed, let the
    /// instruction-count clock run to its next tick — a parked hart retires
    /// nothing, so virtual time skips ahead to the next event the way an
    /// event-driven simulator would. Returns true when an interrupt is now
    /// deliverable.
    pub(crate) fn refresh_wake(&mut self) -> bool {
        if self.mmio.recv_ie {
            let host: &mut dyn Host = &mut *self.host;
            self.mmio.poll_input(host);
        }
        if self.deliverable_interrupt().is_some() {
            return true;
        }
        if self.timer_interval.is_some() {
            self.instret = self.instret.max(self.timer_deadline);
            return self.deliverable_interrupt().is_some();
        }
        false
    }

    /// Poll the keyboard into the receiver queue at a delivery point. Gated
    /// on the receiver enable so interrupt-blind polling never drains the
    /// host buffer out from under `read_line` syscalls.
    pub(crate) fn poll_input(&mut self) {
        if self.mmio.recv_ie {
            let host: &mut dyn Host = &mut *self.host;
            self.mmio.poll_input(host);
        }
    }

    /// Deliver one trap: journal it as its own backstep statement, write the
    /// trap CSRs, stack `ustatus`, and redirect the pc. `uepc` is supplied by
    /// the caller (the not-yet-executed instruction for interrupts, the
    /// faulting one for exceptions).
    pub(crate) fn take_trap(&mut self, cause: u32, tval: u64, interrupt: bool, uepc: u32, changes: &mut Vec<Change>) {
        use irq::*;
        self.journal.begin_statement();
        // Cause-specific consumption first so backstep can restore it.
        match cause {
            KEYBOARD => {
                // Derived from queue + enable: draining the queue at the
                // handler's `lw` clears the request by itself.
            }
            DISPLAY => {
                if self.mmio.take_xmit_edge() {
                    self.journal.push(Undo::XmitEdge);
                }
            }
            HEX_KEYS => {
                // Latched at press time; consume it like the software
                // request so the handler's uret does not immediately re-trap.
                self.mmio.keys_pending = false;
                self.journal.push(Undo::HexKeys);
            }
            SOFTWARE => {
                self.software_pending = false;
                self.journal.push(Undo::SoftwareIrq);
            }
            TIMER => {
                // Periodic: consume this tick and schedule the next one.
                let old = self.timer_deadline;
                self.timer_deadline = old.saturating_add(self.timer_interval.unwrap_or(1));
                self.journal.push(Undo::TimerDeadline { old });
            }
            _ => {}
        }
        let utvec = self.read_csr(csr::UTVEC);
        let base = (utvec & 0xFFFF_FFFC) as u32;
        // Vectored mode (utvec & 0x3 == 2) offsets interrupts by 4 * cause;
        // exceptions always target the base.
        let target = if interrupt && (utvec & 0x3) == 2 {
            base.wrapping_add(4 * cause)
        } else {
            base
        };
        // ustatus: UPIE <- UIE, UIE <- 0 (user-level spec stacking).
        let st = self.read_csr(csr::USTATUS);
        let stacked = (st & !0x11) | ((st & 0x1) << 4);
        self.write_csr_raw(csr::USTATUS, stacked, changes);
        self.write_csr_raw(csr::UEPC, u64::from(uepc), changes);
        let full_cause = if interrupt { u64::from(cause) | irq::INTERRUPT_BIT } else { u64::from(cause) };
        self.write_csr_raw(csr::UCAUSE, full_cause, changes);
        self.write_csr_raw(csr::UTVAL, tval, changes);
        self.journal.push(Undo::Pc { old: self.pc });
        self.pc = target;
    }

    /// `uret`: pc <- uepc, UIE <- UPIE, UPIE <- 1. Runs inside the `uret`
    /// instruction's own journal statement, so one backstep undoes it whole.
    pub(crate) fn do_uret(&mut self, changes: &mut Vec<Change>) {
        let uepc = self.read_csr(csr::UEPC) as u32;
        let st = self.read_csr(csr::USTATUS);
        let upie = (st >> 4) & 0x1;
        let restored = (st & !0x11) | upie | 0x10;
        self.write_csr_raw(csr::USTATUS, restored, changes);
        self.pc = uepc;
    }

    /// Arm the instruction-count timer: raises the timer interrupt
    /// (`irq::TIMER`) every `interval_instructions` retired instructions,
    /// one request per tick. Zero intervals clamp to 1 so arming at an
    /// exact tick boundary still yields a steady period.
    pub fn set_timer(&mut self, interval_instructions: u64) {
        let interval = interval_instructions.max(1);
        self.timer_interval = Some(interval);
        self.timer_deadline = self.instret.saturating_add(interval);
    }

    /// Disarm the timer and drop any undelivered tick.
    pub fn clear_timer(&mut self) {
        self.timer_interval = None;
        self.timer_deadline = 0;
    }

    /// True while the hart is parked in `wfi` (no deliverable interrupt).
    pub fn is_waiting(&self) -> bool {
        self.waiting
    }

    /// Clear a debugger stop and resume. For breakpoints/ebreaks a one-shot
    /// skip is armed so the stop at the current pc does not re-fire
    /// immediately — standard continue semantics: it fires again on the
    /// *next* arrival at that address. Watchpoint, memcheck, and
    /// calling-convention stops need no skip: the watched instruction
    /// already retired, so the pc is past it (and arming one would wrongly
    /// swallow a breakpoint on the next instruction). Real terminations
    /// (exit, error, dropped-off) stay put.
    pub fn continue_after_stop(&mut self) {
        match self.terminated {
            Some(Halt::Breakpoint) | Some(Halt::Ebreak) => {
                self.skip_break_once = Some(self.pc);
                self.terminated = None;
            }
            Some(Halt::Watchpoint { .. }) | Some(Halt::Memcheck { .. }) | Some(Halt::CallingConvention { .. }) => {
                self.terminated = None;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csr;
    use crate::testutil::*;
    use crate::{Event, Halt, ScriptHost};

    const URET: u32 = 0x0020_0073;
    const WFI: u32 = 0x1050_0073;

    /// Overwrite the word at `addr` with a hand-encoded instruction (the
    /// assembler has no uret/wfi mnemonics and .word is data-only).
    fn patch(m: &mut Machine, addr: u32, word: u32) {
        m.mem.write_bytes(addr, &word.to_le_bytes());
    }

    fn sym(m: &Machine, name: &str) -> u32 {
        m.program().symbols.get(name).unwrap().addr
    }

    #[test]
    fn receiver_interrupt_end_to_end() {
        // Enable the receiver interrupt, point utvec at a handler that reads
        // the key and prints it, and return with uret. The host pre-queues
        // the keystroke; the machine polls it at the delivery point.
        let src = "\
main:
    li t0, 0xffff0000
    li t1, 2
    sw t1, 0(t0)          # receiver interrupt enable
    la t2, handler
    csrrw x0, utvec, t2
    li t3, 1
    csrrw x0, ustatus, t3 # global UIE
spin:
    beq x0, x0, spin
handler:
    li t4, 0xffff0004
    lw a0, 0(t4)          # read the key (clears the request)
    li a7, 11
    ecall                 # PrintChar
uret_slot:
    nop
";
        let host = ScriptHost::with_input(vec!["K".into()]);
        let mut m = machine_with(src, Box::new(host.clone()));
        let uret_addr = sym(&m, "uret_slot");
        patch(&mut m, uret_addr, URET);
        let events = m.run(Some(400));
        assert_eq!(host.take_output(), "K"); // exactly once: no re-fire loop
        assert_eq!(events.last(), Some(&Event::Halted(Halt::Limit)));
        // uret restored UIE and set UPIE per the stacking rule.
        assert_eq!(m.csr(csr::USTATUS), 0x11);
        assert_eq!(m.csr(csr::UCAUSE), irq::INTERRUPT_BIT | u64::from(irq::KEYBOARD));
        // pc is back at the interrupted instruction.
        assert_eq!(m.pc(), m.csr(csr::UEPC) as u32);
    }

    #[test]
    fn vectored_mode_offsets_by_cause_direct_does_not() {
        for (mode_bit, offset) in [(0u64, 0u32), (2, 4 * irq::KEYBOARD)] {
            let host = ScriptHost::with_input(vec!["z".into()]);
            let mut m = machine_with("    nop\n    nop\n", Box::new(host));
            let handler = m.program().text_base + 0x400;
            m.csrs.insert(csr::UTVEC, u64::from(handler) | mode_bit);
            m.csrs.insert(csr::USTATUS, 1); // UIE
            m.mmio.recv_ie = true;
            m.poll_input(); // a key waits in the receiver
            let before = m.pc();
            m.step(); // the nop retires, then the delivery point traps
            let want = handler.wrapping_add(offset);
            assert_eq!(m.pc(), want, "mode bit {mode_bit}");
            assert_eq!(m.csr(csr::UEPC), u64::from(before + 4));
            assert_eq!(m.csr(csr::UCAUSE), irq::INTERRUPT_BIT | u64::from(irq::KEYBOARD));
            assert_eq!(m.csr(csr::UTVAL), 0);
            // UIE stacked into UPIE, UIE cleared.
            assert_eq!(m.csr(csr::USTATUS), 0x10);
        }
    }

    #[test]
    fn ustatus_stacks_through_uret() {
        let mut m = machine("    nop\n");
        m.csrs.insert(csr::UTVEC, u64::from(m.program().text_base + 0x400));
        m.csrs.insert(csr::USTATUS, 1);
        m.software_pending = true;
        m.step(); // nop retires; the software interrupt traps
        assert_eq!(m.csr(csr::USTATUS), 0x10);
        let uepc = m.csr(csr::UEPC);
        // Return with hand-encoded uret.
        let pc = m.pc();
        patch(&mut m, pc, URET);
        m.step();
        assert_eq!(m.pc(), uepc as u32);
        // UIE <- UPIE (1), UPIE <- 1.
        assert_eq!(m.csr(csr::USTATUS), 0x11);
    }

    #[test]
    fn backstep_undoes_a_trap() {
        let mut m = machine("    nop\n    nop\n");
        let handler = m.program().text_base + 0x400;
        m.csrs.insert(csr::UTVEC, u64::from(handler));
        m.csrs.insert(csr::USTATUS, 1);
        m.software_pending = true;
        m.step();
        assert_eq!(m.pc(), handler);
        assert!(m.backstep());
        // Back to the interrupted instruction with the trap CSRs undone and
        // the request re-raised, so stepping traps again.
        assert_eq!(m.pc(), m.program().text_base + 4);
        assert_eq!(m.csr(csr::UCAUSE), 0);
        assert_eq!(m.csr(csr::UEPC), 0);
        assert_eq!(m.csr(csr::USTATUS), 1);
        assert!(m.software_pending);
        m.step();
        assert_eq!(m.pc(), handler);
    }

    #[test]
    fn interrupt_priority_external_then_software_then_timer() {
        let host = ScriptHost::with_input(vec!["q".into()]);
        let mut m = machine_with("    nop\n", Box::new(host));
        m.csrs.insert(csr::USTATUS, 1);
        m.set_timer(1);
        m.instret += 5; // the tick is already due
        m.software_pending = true;
        m.mmio.recv_ie = true;
        m.poll_input();
        // External device first, then software, then the timer.
        assert_eq!(m.deliverable_interrupt(), Some(irq::KEYBOARD));
        m.mmio.clear_input();
        assert_eq!(m.deliverable_interrupt(), Some(irq::SOFTWARE));
        m.software_pending = false;
        assert_eq!(m.deliverable_interrupt(), Some(irq::TIMER));
        // Nothing is deliverable without the global UIE.
        m.csrs.insert(csr::USTATUS, 0);
        assert_eq!(m.deliverable_interrupt(), None);
        m.csrs.insert(csr::USTATUS, 1);
        assert_eq!(m.deliverable_interrupt(), Some(irq::TIMER));
    }

    #[test]
    fn timer_interrupt_fires_periodically_through_a_handler() {
        let src = "\
main:
    la t0, handler
    csrrw x0, utvec, t0
    li t1, 1
    csrrw x0, ustatus, t1
spin:
    nop               # patched to wfi: park between ticks
    beq x0, x0, spin
handler:
    addi s0, s0, 1
uret_slot:
    nop
";
        let mut m = machine(src);
        let uret_addr = sym(&m, "uret_slot");
        let spin_addr = sym(&m, "spin");
        patch(&mut m, uret_addr, URET);
        patch(&mut m, spin_addr, WFI);
        m.set_timer(25);
        let events = m.run(Some(400));
        assert_eq!(events.last(), Some(&Event::Halted(Halt::Limit)));
        // The timer woke the parked hart more than once and every tick
        // trapped exactly once.
        assert!(m.reg(8) >= 2, "s0 = {}", m.reg(8));
        assert!(!m.is_waiting());
        // time reads as the retired-instruction clock.
        assert_eq!(m.csr(csr::TIME), m.instret());
    }

    #[test]
    fn wfi_parks_then_input_wakes_it() {
        let src = "\
main:
    li t0, 0xffff0000
    la t1, handler
    csrrw x0, utvec, t1
    li t2, 1
    csrrw x0, ustatus, t2
    # receiver interrupt stays disabled here; run() parks, the test then
    # flips the enable so the queued key becomes the wake source.
spin:
    nop               # patched to wfi
    beq x0, x0, spin
handler:
    addi s0, s0, 1
    li t3, 0xffff0004
    lw t4, 0(t3)      # drain the key so the request clears
uret_slot:
    nop
";
        let host = ScriptHost::with_input(vec!["w".into()]);
        let mut m = machine_with(src, Box::new(host));
        let uret_addr = sym(&m, "uret_slot");
        let spin_addr = sym(&m, "spin");
        patch(&mut m, uret_addr, URET);
        patch(&mut m, spin_addr, WFI);
        // Nothing can wake the hart yet (no receiver enable, no timer):
        // run hands control back with the machine still waiting.
        let events = m.run(Some(100));
        assert!(events.is_empty());
        assert!(m.is_waiting());
        assert!(!m.is_terminated());
        // The enable bit turns the queued key into a pending interrupt.
        m.mmio.recv_ie = true;
        m.run(Some(100));
        assert_eq!(m.reg(8), 1); // the handler ran exactly once
        assert!(!m.is_terminated());
    }

    #[test]
    fn transmitter_edge_fires_exactly_once() {
        let src = "\
main:
    li t0, 0xffff0008
    li t1, 2
    sw t1, 0(t0)          # transmitter enable rising edge: one request
    la t2, handler
    csrrw x0, utvec, t2
    li t3, 1
    csrrw x0, ustatus, t3
spin:
    beq x0, x0, spin
handler:
    addi s0, s0, 1
uret_slot:
    nop
";
        let mut m = machine(src);
        let uret_addr = sym(&m, "uret_slot");
        patch(&mut m, uret_addr, URET);
        m.run(Some(200));
        // The always-ready transmitter is modeled as edge-triggered: the
        // enable transition requests one interrupt, and nothing re-arms it.
        assert_eq!(m.reg(8), 1);
    }

    #[test]
    fn breakpoint_fires_on_trap_handler_entry() {
        let src = "\
main:
    li t0, 0xffff0000
    li t1, 2
    sw t1, 0(t0)
    la t2, handler
    csrrw x0, utvec, t2
    li t3, 1
    csrrw x0, ustatus, t3
spin:
    beq x0, x0, spin
handler:
    li t4, 0xffff0004
    lw a0, 0(t4)
    li a7, 11
    ecall
uret_slot:
    nop
";
        let host = ScriptHost::with_input(vec!["x".into()]);
        let mut m = machine_with(src, Box::new(host.clone()));
        let handler = sym(&m, "handler");
        m.set_breakpoint(handler, true);
        let events = m.run(None);
        // The RARS gap: the breakpoint fires when the trap ENTERS the
        // handler, before any handler instruction runs.
        assert_eq!(events.last(), Some(&Event::Halted(Halt::Breakpoint)));
        assert_eq!(m.pc(), handler);
        assert_eq!(m.csr(csr::UCAUSE), irq::INTERRUPT_BIT | u64::from(irq::KEYBOARD));
        // Continue from the stop: the handler runs to completion.
        m.continue_after_stop();
        assert!(!m.is_terminated());
        m.run(Some(200));
        assert_eq!(host.take_output(), "x");
    }

    #[test]
    fn sync_exception_vectors_when_handler_configured() {
        let src = "\
main:
    la t0, handler
    csrrw x0, utvec, t0
    li t1, 8
    lw a0, 0(t1)          # access violation -> load fault trap
dead:
    beq x0, x0, dead
handler:
    csrrs s1, ucause, x0
    csrrs s2, uepc, x0
    csrrs s3, utval, x0
    li a7, 10
    ecall
";
        let mut m = machine(src);
        m.run(None);
        let lw_addr = m.csr(csr::UEPC) as u32;
        assert_eq!(m.exit_code(), Some(0)); // the handler exited cleanly
        assert_eq!(m.reg(9), 5); // s1: load access fault
        assert_eq!(m.reg(18), u64::from(lw_addr)); // s2: faulting instruction
        assert_eq!(m.reg(19), 8); // s3: the bad address
    }

    #[test]
    fn sync_exception_halts_without_utvec_and_marks_terminated() {
        let mut m = machine("    li t0, 8\n    lw a0, 0(t0)\n");
        let events = m.run(None);
        assert!(matches!(events.last(), Some(Event::Halted(Halt::Error { .. }))));
        // The stop is visible through the machine state, like other halts.
        assert!(m.is_terminated());
        assert!(matches!(m.halt_reason(), Some(Halt::Error { .. })));
    }

    #[test]
    fn misaligned_fetch_vectors_or_halts() {
        // With a handler configured the misaligned pc becomes a trap (cause
        // 0, utval = the bad pc); without one it halts as before.
        let mut m = machine("    nop\n");
        let base = m.program().text_base;
        m.csrs.insert(csr::UTVEC, u64::from(base + 0x400));
        m.pc = base + 2;
        m.step();
        assert_eq!(m.pc(), base + 0x400);
        assert_eq!(m.csr(csr::UCAUSE), 0);
        assert_eq!(m.csr(csr::UEPC), u64::from(base + 2));
        assert_eq!(m.csr(csr::UTVAL), u64::from(base + 2));
        assert!(!m.is_terminated());

        let mut m = machine("    nop\n");
        m.pc = m.program().text_base + 2;
        let out = m.step();
        assert!(matches!(out.events.last(), Some(Event::Halted(Halt::Error { .. }))));
        assert!(m.is_terminated());
    }

    #[test]
    fn wfi_parks_and_backsteps_clean() {
        let mut m = machine("    nop\n");
        let base = m.program().text_base;
        patch(&mut m, base, WFI);
        m.step();
        // No interrupt can arrive: the hart parks at the wfi, nothing retires.
        assert!(m.is_waiting());
        assert!(!m.is_terminated());
        assert_eq!(m.pc(), m.program().text_base);
        assert_eq!(m.instret(), 0);
        assert!(m.backstep());
        assert!(!m.is_waiting());
        // Waking when an interrupt is already deliverable: plain no-op.
        let mut m = machine("    nop\n");
        let handler = m.program().text_base + 0x400;
        m.csrs.insert(csr::UTVEC, u64::from(handler));
        m.csrs.insert(csr::USTATUS, 1);
        m.software_pending = true;
        m.step();
        assert!(!m.is_waiting());
        assert_eq!(m.pc(), handler); // the wfi retired, then the trap delivered
        assert_eq!(m.instret(), 1);
        assert_eq!(m.csr(csr::UEPC), u64::from(m.program().text_base + 4));
    }
}



