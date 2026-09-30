//! Debugger watches (memory and register watchpoints) and the memcheck
//! initialization shadow.
//!
//! Watchpoints mirror the breakpoint model with one documented difference:
//! a breakpoint stops *before* the instruction at the marked address runs,
//! while a watchpoint stop lands *after* the watched instruction retires
//! with its effect applied. The watched address is only known inside the
//! load/store helpers, so the checks live where the effect completes and
//! the stop is raised at the end of the step. `continue_after_stop` knows
//! the difference: for watch stops the pc is already past the instruction,
//! so no one-shot breakpoint skip is armed (a breakpoint on the *next*
//! instruction must still fire).
//!
//! Memcheck (Venus's uninitialized-read detector) is opt-in through
//! `MachineConfig::memcheck`. Every byte of general memory carries a shadow
//! initialization bit: program-image bytes, `.extern` reservations, and the
//! argv block start initialized; stores and memory-writing syscalls
//! initialize as they run; everything else (unset stack, sbrk'd heap,
//! never-touched pages reading as zero) stays uninitialized, and a load
//! touching any such byte halts. The MMIO window is exempt — device
//! semantics, not memory.

use crate::{Halt, Machine};
use std::collections::HashMap;

use crate::memory::PAGE_SIZE;

/// ABI names for register-watch descriptions. Local copy because rvasm keeps
/// its table private to its `asm` module and this crate cannot reach in.
const ABI: [&str; 32] = [
    "zero", "ra", "sp", "gp", "tp", "t0", "t1", "t2", "s0", "s1", "a0", "a1", "a2", "a3", "a4", "a5",
    "a6", "a7", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11", "t3", "t4", "t5", "t6",
];

/// What a watchpoint watches. Public so a watchpoints panel can list the
/// live registrations via `Machine::watchpoints`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WatchSpec {
    /// Any byte in `[addr, addr + len)` (e.g. 4 bytes for one word).
    /// `on_read`/`on_write` select the access kinds that stop. MMIO
    /// accesses count for the kinds their direction implies (an MMIO store
    /// is a write), so a watch over the MMIO window sees device traffic.
    Mem { addr: u32, len: u32, on_read: bool, on_write: bool },
    /// Writes to one integer register. x0 is hardwired to zero and never
    /// written, so watching it never fires.
    Reg { index: usize },
}

/// One live watchpoint registration.
pub(crate) struct Watch {
    pub(crate) id: u32,
    pub(crate) spec: WatchSpec,
}

/// Memcheck shadow: one initialization flag per byte of general memory.
/// Page-granular like `Memory` (4 KiB keys); a missing page is fully
/// uninitialized, which is exactly the "read of a never-touched page" bug
/// memcheck exists to catch. One flag byte per tracked byte keeps the
/// arithmetic trivial; the cost is bounded by pages the program actually
/// initialized, and the whole structure exists only when memcheck is on.
#[derive(Default)]
pub(crate) struct InitShadow {
    pages: HashMap<u32, Vec<u8>>,
}

impl InitShadow {
    /// Mark `[addr, addr + len)` initialized.
    pub(crate) fn mark_init(&mut self, mut addr: u32, len: u32) {
        let mut rest = len;
        while rest > 0 {
            let off = (addr as usize) % PAGE_SIZE;
            let n = (PAGE_SIZE - off).min(rest as usize);
            let page = self.pages.entry(addr - off as u32).or_insert_with(|| vec![0; PAGE_SIZE]);
            page[off..off + n].fill(1);
            rest -= n as u32;
            addr += n as u32;
        }
    }

    /// True when the byte is initialized (a never-touched page is not).
    pub(crate) fn is_init(&self, addr: u32) -> bool {
        let off = (addr as usize) % PAGE_SIZE;
        self.pages.get(&(addr - off as u32)).is_some_and(|p| p[off] != 0)
    }

    /// How many bytes of `[addr, addr + len)` are still uninitialized.
    pub(crate) fn uninit_count(&self, mut addr: u32, len: u32) -> u32 {
        let mut count = 0u32;
        let mut rest = len;
        while rest > 0 {
            let off = (addr as usize) % PAGE_SIZE;
            let n = (PAGE_SIZE - off).min(rest as usize);
            match self.pages.get(&(addr - off as u32)) {
                Some(page) => count += page[off..off + n].iter().filter(|b| **b == 0).count() as u32,
                // A page never touched by an initializer is fully uninitialized.
                None => count += n as u32,
            }
            rest -= n as u32;
            addr += n as u32;
        }
        count
    }

    /// Clear one byte's flag (the undo arm of a journaled store).
    pub(crate) fn clear_init(&mut self, addr: u32) {
        let off = (addr as usize) % PAGE_SIZE;
        if let Some(page) = self.pages.get_mut(&(addr - off as u32)) {
            page[off] = 0;
        }
    }

    pub(crate) fn clear(&mut self) {
        self.pages.clear();
    }
}

impl Machine {
    // ---- watchpoint registration (public API) ----

    /// Watch a memory range: stop (after the access retires) when a load or
    /// store touches any watched byte in the selected direction. Returns a
    /// watchpoint id for `remove_watchpoint`.
    pub fn add_mem_watchpoint(&mut self, addr: u32, len: u32, on_read: bool, on_write: bool) -> u32 {
        let id = self.next_watch_id;
        self.next_watch_id += 1;
        self.watchpoints.push(Watch { id, spec: WatchSpec::Mem { addr, len, on_read, on_write } });
        id
    }

    /// Watch one integer register for writes, in the same id namespace as
    /// memory watchpoints.
    pub fn add_reg_watchpoint(&mut self, index: usize) -> u32 {
        let id = self.next_watch_id;
        self.next_watch_id += 1;
        self.watchpoints.push(Watch { id, spec: WatchSpec::Reg { index } });
        id
    }

    /// Remove one watchpoint by id; true when a registration was dropped.
    pub fn remove_watchpoint(&mut self, id: u32) -> bool {
        let before = self.watchpoints.len();
        self.watchpoints.retain(|w| w.id != id);
        self.watchpoints.len() != before
    }

    /// Drop every watchpoint registration (memory and register alike).
    pub fn clear_watchpoints(&mut self) {
        self.watchpoints.clear();
    }

    /// Snapshot of the live registrations (id + what it watches), for a
    /// watchpoints panel. Ids are stable until removed.
    pub fn watchpoints(&self) -> Vec<(u32, WatchSpec)> {
        self.watchpoints.iter().map(|w| (w.id, w.spec)).collect()
    }

    // ---- checks called from the load/store/register paths ----

    /// A load/store of `width` bytes just landed at `addr`. The first
    /// matching watch wins; one stop is raised per instruction.
    pub(crate) fn check_mem_watch(&mut self, addr: u32, width: u32, write: bool) {
        if self.pending_stop.is_some() || self.watchpoints.is_empty() {
            return;
        }
        let hit = self.watchpoints.iter().find_map(|w| match w.spec {
            WatchSpec::Mem { addr: wa, len, on_read, on_write } => {
                let touched = addr < wa.wrapping_add(len) && wa < addr.wrapping_add(width);
                (touched && (if write { on_write } else { on_read })).then_some((w.id, write))
            }
            WatchSpec::Reg { .. } => None,
        });
        if let Some((id, write)) = hit {
            let desc = format!(
                "watchpoint {id}: {} 0x{addr:08x}{}",
                if write { "write to" } else { "read of" },
                self.at_text(),
            );
            self.pending_stop = Some(Halt::Watchpoint { description: desc });
        }
    }

    /// A register write just landed (x0 never reaches here).
    pub(crate) fn check_reg_watch(&mut self, index: usize) {
        if self.pending_stop.is_some() || self.watchpoints.is_empty() {
            return;
        }
        let hit =
            self.watchpoints.iter().find_map(|w| match w.spec {
                WatchSpec::Reg { index: ri } => (ri == index).then_some(w.id),
                WatchSpec::Mem { .. } => None,
            });
        if let Some(id) = hit {
            let name = if index < 32 { ABI[index].to_string() } else { format!("x{index}") };
            let desc = format!("watchpoint {id}: write to register {name}{}", self.at_text());
            self.pending_stop = Some(Halt::Watchpoint { description: desc });
        }
    }

    /// Memcheck: a load of `width` bytes just completed at `addr`; halt when
    /// any covered byte is still uninitialized. MMIO never reaches here
    /// (device reads are exempt), and the MMIO-vs-memory split in
    /// `load_bytes` keeps it that way.
    pub(crate) fn check_memcheck_load(&mut self, addr: u32, width: u32) {
        if self.pending_stop.is_some() {
            return;
        }
        let uninit = self.shadow.uninit_count(addr, width);
        if uninit == 0 {
            return;
        }
        let desc = format!(
            "read of {width} {} at 0x{addr:08x} touches {uninit} uninitialized {}, loaded{}",
            if width == 1 { "byte" } else { "bytes" },
            if uninit == 1 { "byte" } else { "bytes" },
            self.at_text(),
        );
        self.pending_stop = Some(Halt::Memcheck { description: desc });
    }

    /// " by sw at line 14" for the instruction executing now, or "" when the
    /// pc is not on an assembled statement (patched/handler code).
    fn at_text(&self) -> String {
        match self.program.statement_at(self.active_pc) {
            Some(s) => {
                let word = s.basic_text.split_whitespace().next().unwrap_or("?");
                format!(" by {word} at line {}", s.source.line)
            }
            None => String::new(),
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

    fn memcheck_cfg() -> MachineConfig {
        MachineConfig { memcheck: true, ..MachineConfig::default() }
    }

    fn sym(m: &Machine, name: &str) -> u32 {
        m.program().symbols.get(name).unwrap().addr
    }

    /// The description of a Watchpoint/Memcheck halt, or None.
    fn stop_text(m: &Machine) -> Option<String> {
        match m.halt_reason() {
            Some(Halt::Watchpoint { description }) | Some(Halt::Memcheck { description }) => {
                Some(description.clone())
            }
            _ => None,
        }
    }

    // ---- memory watchpoints ----

    #[test]
    fn write_watchpoint_stops_store_and_describes_it() {
        let src = "\
.data
val: .word 0
.text
main:
    li t0, 5
    la t1, val
    sw t0, 0(t1)
    li a7, 10
    ecall
";
        let mut m = machine(src);
        let val = sym(&m, "val");
        let id = m.add_mem_watchpoint(val, 4, false, true);
        assert_eq!(id, 0);
        let events = m.run(None);
        assert!(matches!(events.last(), Some(Event::Halted(Halt::Watchpoint { .. }))));
        assert!(matches!(m.halt_reason(), Some(Halt::Watchpoint { .. })));
        // The store already landed (the stop follows the effect).
        let mut buf = [0u8; 4];
        m.peek_bytes(val, &mut buf).unwrap();
        assert_eq!(buf, 5u32.to_le_bytes());
        // "watchpoint 1: write to 0x10010000 by sw at line 7"
        let desc = stop_text(&m).unwrap();
        assert_eq!(
            desc,
            format!("watchpoint {id}: write to 0x{val:08x} by sw at line 7"),
            "desc: {desc}"
        );
    }

    #[test]
    fn read_write_watchpoint_catches_load_but_write_only_skips_it() {
        let src = "\
.data
v: .word 55
.text
main:
    la t0, v
    lw a0, 0(t0)
    li a7, 10
    ecall
";
        // on_read + on_write: the load stops the machine.
        let mut m = machine(src);
        let v = sym(&m, "v");
        m.add_mem_watchpoint(v, 4, true, true);
        let events = m.run(None);
        assert!(matches!(events.last(), Some(Event::Halted(Halt::Watchpoint { .. }))));
        assert_eq!(m.reg(10), 55); // the load landed, then the stop raised
        let desc = stop_text(&m).unwrap();
        assert!(desc.contains("read of"), "{desc}");
        assert!(desc.contains(&format!("0x{v:08x}")), "{desc}");
        assert!(desc.contains("by lw at line 6"), "{desc}");
        // Write-only: the same load sails through and the program exits.
        let mut m = machine(src);
        m.add_mem_watchpoint(sym(&m, "v"), 4, false, true);
        let events = m.run(None);
        assert_eq!(events.last(), Some(&Event::Halted(EXIT)));
    }

    #[test]
    fn register_watchpoint_catches_li() {
        let src = "\
main:
    li a1, 42
    li a7, 10
    ecall
";
        let mut m = machine(src);
        m.add_reg_watchpoint(11); // a1
        let events = m.run(None);
        assert!(matches!(events.last(), Some(Event::Halted(Halt::Watchpoint { .. }))));
        assert_eq!(m.reg(11), 42); // the write landed
        let desc = stop_text(&m).unwrap();
        assert_eq!(desc, "watchpoint 0: write to register a1 by addi at line 2", "{desc}");
        // Continue runs to completion: nothing writes a1 again.
        m.continue_after_stop();
        let events = m.run(None);
        assert_eq!(events.last(), Some(&Event::Halted(EXIT)));
    }

    #[test]
    fn remove_and_clear_watchpoints_work() {
        let src = "\
main:
    li a0, 1
    li a7, 10
    ecall
";
        let mut m = machine(src);
        let mem_id = m.add_mem_watchpoint(0x7fff_effc, 4, false, true);
        let reg_id = m.add_reg_watchpoint(10);
        // One shared id namespace.
        assert_ne!(mem_id, reg_id);
        assert_eq!(
            m.watchpoints(),
            vec![
                (mem_id, WatchSpec::Mem { addr: 0x7fff_effc, len: 4, on_read: false, on_write: true }),
                (reg_id, WatchSpec::Reg { index: 10 }),
            ]
        );
        assert!(m.remove_watchpoint(mem_id));
        assert!(!m.remove_watchpoint(mem_id)); // already gone
        m.clear_watchpoints();
        assert!(m.watchpoints().is_empty());
        let events = m.run(None);
        assert_eq!(events.last(), Some(&Event::Halted(EXIT)));
        assert_eq!(m.reg(10), 1);
    }

    #[test]
    fn breakpoint_wins_over_watchpoint_on_same_instruction() {
        let src = "\
main:
    li t0, 7
    sw t0, 0(sp)
    li a7, 10
    ecall
";
        let mut m = machine(src);
        let sw_addr = m.program().statements[1].addr; // li is one instruction here
        m.set_breakpoint(sw_addr, true);
        m.add_mem_watchpoint(m.reg(2) as u32, 4, false, true); // [sp, sp+4)
        // The breakpoint fires first: the store never ran.
        let events = m.run(None);
        assert_eq!(events.last(), Some(&Event::Halted(Halt::Breakpoint)));
        assert_eq!(m.pc(), sw_addr);
        // Continue: the store lands and the watchpoint reports it.
        m.continue_after_stop();
        let events = m.run(None);
        assert!(matches!(events.last(), Some(Event::Halted(Halt::Watchpoint { .. }))));
        // Continue past the watch: the program exits.
        m.continue_after_stop();
        let events = m.run(None);
        assert_eq!(events.last(), Some(&Event::Halted(EXIT)));
    }

    #[test]
    fn backstep_clears_watchpoint_halt_and_continue_resumes() {
        let src = "\
.data
val: .word 0
.text
main:
    li t0, 5
    la t1, val
    sw t0, 0(t1)
    li a7, 10
    ecall
";
        let mut m = machine(src);
        m.add_mem_watchpoint(sym(&m, "val"), 4, false, true);
        m.run(None);
        assert!(matches!(m.halt_reason(), Some(Halt::Watchpoint { .. })));
        // Like a breakpoint stop, backstep undoes the watched instruction
        // and clears the halt.
        let sw_pc = m.pc() - 4;
        assert!(m.backstep());
        assert!(!m.is_terminated());
        assert_eq!(m.pc(), sw_pc);
        let mut buf = [0u8; 4];
        m.peek_bytes(sym(&m, "val"), &mut buf).unwrap();
        assert_eq!(buf, [0; 4]); // the store was undone
        // Re-executing refires the watch...
        m.step();
        assert!(matches!(m.halt_reason(), Some(Halt::Watchpoint { .. })));
        // ...and continue_after_stop resumes past it (the halt is a pause,
        // like a breakpoint's).
        m.continue_after_stop();
        assert!(!m.is_terminated());
        let events = m.run(None);
        assert_eq!(events.last(), Some(&Event::Halted(EXIT)));
    }

    #[test]
    fn watch_refires_each_loop_pass_through_continue() {
        let src = "\
main:
    li t1, 3
loop:
    addi a0, a0, 1
    blt a0, t1, loop
    li a7, 10
    ecall
";
        let mut m = machine(src);
        m.add_reg_watchpoint(10); // a0
        for want in 1..=3 {
            let events = m.run(None);
            assert!(matches!(events.last(), Some(Event::Halted(Halt::Watchpoint { .. }))), "pass {want}");
            assert_eq!(m.reg(10), want);
            m.continue_after_stop();
        }
        let events = m.run(None);
        assert_eq!(events.last(), Some(&Event::Halted(EXIT)));
        assert_eq!(m.reg(10), 3);
    }

    #[test]
    fn mmio_store_triggers_write_watch() {
        let src = "\
main:
    li t0, 0xffff000c
    li t1, 88
    sw t1, 0(t0)
    li a7, 10
    ecall
";
        let host = ScriptHost::default();
        let mut m = machine_with(src, Box::new(host.clone()));
        m.add_mem_watchpoint(0xffff_0000, 16, false, true);
        let events = m.run(None);
        assert!(matches!(events.last(), Some(Event::Halted(Halt::Watchpoint { .. }))));
        // The device write landed before the stop (the 'X' was emitted).
        assert_eq!(host.take_output(), "X");
        let desc = stop_text(&m).unwrap();
        assert!(desc.contains("write to 0xffff000c"), "{desc}");
    }

    #[test]
    fn watch_survives_reset() {
        let src = "\
main:
    li a0, 2
    li a7, 10
    ecall
";
        let mut m = machine(src);
        m.add_reg_watchpoint(10);
        m.run(None);
        assert!(matches!(m.halt_reason(), Some(Halt::Watchpoint { .. })));
        // Reset clears the halt but keeps registrations, like breakpoints.
        m.reset();
        assert!(!m.is_terminated());
        assert_eq!(m.watchpoints().len(), 1);
        let events = m.run(None);
        assert!(matches!(events.last(), Some(Event::Halted(Halt::Watchpoint { .. }))));
    }

    #[test]
    fn rv64_watchpoint_tracks_word_and_doubleword() {
        let src = "\
main:
    li t0, 9
    sd t0, -4(sp)
    ld a0, -4(sp)
    li a7, 10
    ecall
";
        let mut m = machine64(src);
        // [sp-4, sp+4) covers the whole doubleword.
        m.add_mem_watchpoint(m.reg(2) as u32 - 4, 8, true, true);
        let events = m.run(None);
        assert!(matches!(events.last(), Some(Event::Halted(Halt::Watchpoint { .. }))));
        assert!(stop_text(&m).unwrap().contains("write to"), "{}", stop_text(&m).unwrap());
        m.continue_after_stop();
        let events = m.run(None);
        assert!(matches!(events.last(), Some(Event::Halted(Halt::Watchpoint { .. }))));
        assert_eq!(m.reg(10), 9);
        let desc = stop_text(&m).unwrap();
        assert!(desc.contains("read of"), "{desc}");
    }

    // ---- memcheck (uninitialized reads) ----

    #[test]
    fn read_before_write_halts_with_description() {
        let src = "\
main:
    li t0, 0x7fffeff0
    lw a0, 0(t0)
    li a7, 10
    ecall
";
        let mut m = machine_cfg(src, memcheck_cfg());
        let events = m.run(None);
        assert!(matches!(events.last(), Some(Event::Halted(Halt::Memcheck { .. }))));
        assert_eq!(
            stop_text(&m).as_deref(),
            Some("read of 4 bytes at 0x7fffeff0 touches 4 uninitialized bytes, loaded by lw at line 3"),
            "{:?}",
            stop_text(&m)
        );
        // A memcheck halt is a pause: backstep then re-executing replays
        // the identical stop (the shadow state was not touched by a load).
        let desc = stop_text(&m).unwrap();
        assert!(m.backstep());
        assert!(!m.is_terminated());
        m.step();
        assert_eq!(stop_text(&m).as_deref(), Some(desc.as_str()));
        m.continue_after_stop();
        let events = m.run(None);
        assert_eq!(events.last(), Some(&Event::Halted(EXIT)));
    }

    #[test]
    fn partial_initialization_counts_only_uninit_bytes() {
        let src = "\
main:
    li t0, 0x7fffeff0
    li t1, 1
    sb t1, 0(t0)
    sb t1, 1(t0)
    lw a0, 0(t0)
    li a7, 10
    ecall
";
        let mut m = machine_cfg(src, memcheck_cfg());
        m.run(None);
        assert_eq!(
            stop_text(&m).as_deref(),
            Some("read of 4 bytes at 0x7fffeff0 touches 2 uninitialized bytes, loaded by lw at line 6"),
            "{:?}",
            stop_text(&m)
        );
        // A byte-wide read reports byte-granular wording.
        let mut m = machine_cfg("main:\n    li t0, 0x7fffeff0\n    lb a0, 2(t0)\n", memcheck_cfg());
        m.run(None);
        assert_eq!(
            stop_text(&m).as_deref(),
            Some("read of 1 byte at 0x7fffeff2 touches 1 uninitialized byte, loaded by lb at line 3"),
            "{:?}",
            stop_text(&m)
        );
    }

    #[test]
    fn write_then_read_passes() {
        let src = "\
main:
    li t0, 0x7fffeff0
    li t1, 9
    sw t1, 0(t0)
    lw a0, 0(t0)
    li a7, 10
    ecall
";
        let mut m = machine_cfg(src, memcheck_cfg());
        let events = m.run(None);
        assert_eq!(events.last(), Some(&Event::Halted(EXIT)));
        assert_eq!(m.reg(10), 9);
    }

    #[test]
    fn data_image_reads_pass_at_start() {
        let src = "\
.data
v: .word 42
.text
main:
    lw a0, v
    li a7, 10
    ecall
";
        let mut m = machine_cfg(src, memcheck_cfg());
        let events = m.run(None);
        assert_eq!(events.last(), Some(&Event::Halted(EXIT)));
        assert_eq!(m.reg(10), 42);
    }

    #[test]
    fn argv_reads_pass() {
        // argv[0]'s pointer array word and the string bytes are both
        // initialized by set_program_args.
        let src = "\
main:
    lw t0, 0(a1)
    lb a0, 0(t0)
    li a7, 10
    ecall
";
        let mut m = machine_cfg(src, memcheck_cfg());
        m.set_program_args(&["hi".into()]);
        let events = m.run(None);
        assert_eq!(events.last(), Some(&Event::Halted(EXIT)));
        assert_eq!(m.reg(10), b'h' as u64);
    }

    #[test]
    fn untouched_heap_read_halts() {
        // The heap (and any sbrk'd space; service 9 is not wired up yet) is
        // never-touched memory: zero-filled on read but uninitialized.
        let src = "\
main:
    li t0, 0x10040000
    lw a0, 0(t0)
    li a7, 10
    ecall
";
        let mut m = machine_cfg(src, memcheck_cfg());
        m.run(None);
        let desc = stop_text(&m).unwrap_or_default();
        assert!(desc.starts_with("read of 4 bytes at 0x10040000 touches 4 uninitialized bytes"), "{desc}");
    }

    #[test]
    fn unset_stack_below_sp_halts() {
        // Venus's classic catch: reading the caller's frame that was never
        // written, even though the bytes read as zero.
        let src = "\
main:
    lw a0, -8(sp)
    li a7, 10
    ecall
";
        let mut m = machine_cfg(src, memcheck_cfg());
        m.run(None);
        let desc = stop_text(&m).unwrap_or_default();
        assert!(desc.contains("loaded by lw at line 2"), "{desc}");
    }

    #[test]
    fn disabled_config_never_halts() {
        let src = "\
main:
    li t0, 0x7fffeff0
    lw a0, 0(t0)
    li a7, 10
    ecall
";
        // Default is off; the same program that memcheck-halts exits cleanly.
        let mut m = machine(src);
        let events = m.run(None);
        assert_eq!(events.last(), Some(&Event::Halted(EXIT)));
        assert_eq!(m.reg(10), 0);
    }

    #[test]
    fn backstep_restores_shadow_bits() {
        let src = "\
main:
    li t0, 0x7fffeff0
    li t1, 9
    sw t1, 0(t0)
    lw a0, 0(t0)
    li a7, 10
    ecall
";
        let mut m = machine_cfg(src, memcheck_cfg());
        // li expands to lui+addi, li again, then sw and lw: five steps.
        for _ in 0..5 {
            m.step();
        }
        assert_eq!(m.reg(10), 9); // the load passed: the store initialized
        // Undo the load, then the store. The store journaled the shadow
        // bytes it flipped, so its undo clears them again.
        assert!(m.backstep());
        assert_eq!(m.pc(), m.program().text_base + 16);
        assert!(m.shadow.is_init(0x7fff_eff0));
        assert!(m.backstep());
        assert_eq!(m.pc(), m.program().text_base + 12);
        assert!(!m.shadow.is_init(0x7fff_eff0));
        // Re-running forward reproduces the same states exactly.
        m.step(); // sw again
        assert!(m.shadow.is_init(0x7fff_eff0));
        m.step(); // lw again
        assert_eq!(m.reg(10), 9);
    }

    #[test]
    fn reset_restores_shadow_to_program_load_state() {
        let src = "\
main:
    li t0, 0x7fffeff0
    lw a0, 0(t0)
    li a7, 10
    ecall
";
        let mut m = machine_cfg(src, memcheck_cfg());
        m.run(None);
        let first = stop_text(&m).unwrap();
        m.continue_after_stop();
        m.run(None);
        assert_eq!(m.exit_code(), Some(0));
        m.reset();
        let events = m.run(None);
        assert!(matches!(events.last(), Some(Event::Halted(Halt::Memcheck { .. }))));
        assert_eq!(stop_text(&m).as_deref(), Some(first.as_str()));
    }

    #[test]
    fn syscall_writes_initialize_the_buffer() {
        // ReadString into stack memory, then read the buffer back: legal
        // only because the syscall's write counts as initialization.
        let src = "\
main:
    addi a0, sp, -32
    li a1, 16
    li a7, 8
    ecall
    lbu a1, 0(a0)
    li a7, 10
    ecall
";
        let host = ScriptHost::with_input(vec!["Ada".into()]);
        let r = asm(src);
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let mut m = Machine::new(r.program.unwrap(), Box::new(host), memcheck_cfg());
        let events = m.run(None);
        assert_eq!(events.last(), Some(&Event::Halted(EXIT)));
        assert_eq!(m.reg(11), b'A' as u64);
    }

    #[test]
    fn mmio_load_is_exempt_from_memcheck() {
        let src = "\
main:
    li t0, 0xffff0004
    lw a0, 0(t0)
    li a7, 10
    ecall
";
        let mut m = machine_cfg(src, memcheck_cfg());
        let events = m.run(None);
        assert_eq!(events.last(), Some(&Event::Halted(EXIT)));
        assert_eq!(m.reg(10), 0); // no keys waiting; reads zero, no halt
    }

    #[test]
    fn fp_loads_and_stores_track_the_shadow() {
        // fsw initializes; a later flw of the same bytes passes...
        let src = "\
main:
    fcvt.s.w f2, zero
    fsw f2, -4(sp)
    flw f1, -4(sp)
    li a7, 10
    ecall
";
        let mut m = machine_cfg(src, memcheck_cfg());
        let events = m.run(None);
        assert_eq!(events.last(), Some(&Event::Halted(EXIT)));
        // ...and an flw of never-written stack halts like an integer load.
        let src = "\
main:
    flw f1, -4(sp)
    li a7, 10
    ecall
";
        let mut m = machine_cfg(src, memcheck_cfg());
        m.run(None);
        let desc = stop_text(&m).unwrap_or_default();
        assert!(desc.contains("loaded by flw at line 2"), "{desc}");
    }

    #[test]
    fn rv64_doubleword_memcheck() {
        let src = "\
main:
    ld a0, -4(sp)
    li a7, 10
    ecall
";
        let files = vec![rvasm::InputFile { name: "t.s".into(), source: src.into() }];
        let acfg = rvasm::AsmConfig { rv64: true, ..rvasm::AsmConfig::default() };
        let r = rvasm::assemble(&files, &acfg);
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let mcfg = MachineConfig { rv64: true, memcheck: true, ..MachineConfig::default() };
        let mut m = Machine::new(r.program.unwrap(), Box::new(ScriptHost::default()), mcfg);
        let events = m.run(None);
        assert!(matches!(events.last(), Some(Event::Halted(Halt::Memcheck { .. }))));
        assert_eq!(
            stop_text(&m).as_deref(),
            Some("read of 8 bytes at 0x7fffeff8 touches 8 uninitialized bytes, loaded by ld at line 2"),
            "{:?}",
            stop_text(&m)
        );
    }
}
