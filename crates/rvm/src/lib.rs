//! The machine: registers, CSRs, execution, syscalls, breakpoints, and the
//! backstep journal. Owned exclusively by one driver thread; all UI contact
//! flows through `Host` and returned events.

mod exec;
mod fp;
mod host;
mod mmio;
mod memory;
mod syscalls;
mod trap;

pub use host::{Host, ScriptHost, StdHost};
pub use memory::MemLayout;
pub use trap::irq;

use crate::memory::{MemError, Memory};
use crate::mmio::Mmio;
use rvasm::Program;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Machine-level settings.
#[derive(Debug, Clone, Default)]
pub struct MachineConfig {
    pub layout: MemLayout,
    /// Run the integer ISA at 64-bit width (RARS's RV64 setting; the default
    /// is RV32, as in RARS). Registers hold full 64-bit values, branches and
    /// compares are 64-bit, the RV64-only instructions execute (`ld`, `sd`,
    /// `lwu`, the `*w` ops, 64-bit FP conversions), and `lui`/`auipc`
    /// sign-extend their immediates. The memory map, MMIO, syscalls, and
    /// breakpoint/backstep behavior are identical in both modes.
    pub rv64: bool,
    /// Allow unaligned loads/stores (RARS errors on them by default).
    pub allow_unaligned: bool,
    /// Allow stores into the text segment (RARS's self-modifying code flag).
    pub self_modifying_code: bool,
}

/// Why the machine stopped producing instructions.
#[derive(Debug, Clone, PartialEq)]
pub enum Halt {
    /// `ecall` exit with a code.
    Exit { code: i32 },
    /// PC ran past the last instruction (RARS's "dropped off the bottom").
    DroppedOff,
    /// Hit a user breakpoint before executing the instruction there.
    Breakpoint,
    /// `ebreak` executed.
    Ebreak,
    /// Simulation error (bad address, unknown syscall, ...).
    Error { message: String },
    /// Caller-imposed instruction limit reached.
    Limit,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Program console output (also delivered to `Host::write_output`).
    Output(String),
    Halted(Halt),
}

/// One observed state change, for UI views and narration.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    Reg { index: usize, old: u64, new: u64 },
    /// Floating-point register write (raw NaN-boxed bits).
    FReg { index: usize, old: u64, new: u64 },
    Csr { id: u16, old: u64, new: u64 },
    Mem { addr: u32, old: u64, new: u64, width: u8 },
    Pc { old: u32, new: u32 },
}

#[derive(Debug, Default)]
pub struct StepOutcome {
    /// True when an instruction executed (false when halted or stopped).
    pub executed: bool,
    pub pc_before: u32,
    pub events: Vec<Event>,
    pub changes: Vec<Change>,
}

impl StepOutcome {
    fn empty() -> Self {
        StepOutcome::default()
    }

    fn halted(h: Halt) -> Self {
        StepOutcome {
            executed: false,
            pc_before: 0,
            events: vec![Event::Halted(h)],
            changes: Vec::new(),
        }
    }
}

/// One undo record. A `Boundary` marks where a statement's records start so
/// backstep can undo a whole instruction atomically.
#[derive(Debug, Clone, Copy)]
enum Undo {
    Boundary,
    /// Marker that this statement retired an instruction, carrying the
    /// opcode (encoding bits 6:0) so backstep rewinds `instret` and the
    /// per-opcode counters together. Traps and parked wfis do not retire.
    Retired { opcode: u32 },
    Reg { index: usize, old: u64 },
    FReg { index: usize, old: u64 },
    Csr { id: u16, old: u64 },
    Mem { addr: u32, old: u64, width: u8 },
    Pc { old: u32 },
    /// A trap consumed the timer's current tick; undo reschedules it.
    TimerDeadline { old: u64 },
    /// A trap consumed the transmitter's edge request; undo re-latches it.
    XmitEdge,
    /// A trap cleared the software request; undo re-raises it.
    SoftwareIrq,
    /// A trap consumed the keypad request; undo re-raises it.
    HexKeys,
    /// `wfi` parked the hart; undo un-parks.
    Waiting { old: bool },
}

struct Journal {
    records: VecDeque<Undo>,
    cap: usize,
}

impl Journal {
    fn new(cap: usize) -> Self {
        Journal { records: VecDeque::with_capacity(cap.min(4096)), cap }
    }

    fn push(&mut self, u: Undo) {
        if self.records.len() == self.cap {
            self.records.pop_front();
        }
        self.records.push_back(u);
    }

    fn begin_statement(&mut self) {
        self.push(Undo::Boundary);
    }

    /// Pop records until the next Boundary is consumed. Returns the records
    /// of one statement in reverse order, or None when the journal is empty.
    fn pop_statement(&mut self) -> Option<Vec<Undo>> {
        let mut recs = Vec::new();
        loop {
            match self.records.pop_back() {
                None => return if recs.is_empty() { None } else { Some(recs) },
                Some(Undo::Boundary) => return Some(recs),
                Some(other) => recs.push(other),
            }
        }
    }
}

/// The RISC-V machine.
pub struct Machine {
    pub(crate) regs: [u64; 32],
    pub(crate) fregs: [u64; 32],
    pub(crate) csrs: std::collections::BTreeMap<u16, u64>,
    pub(crate) pc: u32,
    pub(crate) mem: Memory,
    pub(crate) mmio: Mmio,
    layout: MemLayout,
    config: MachineConfig,
    program: Program,
    pub(crate) breakpoints: BTreeSet<u32>,
    pub(crate) journal: Journal,
    pub(crate) host: Box<dyn Host>,
    pub(crate) terminated: Option<Halt>,
    pub(crate) instret: u64,
    /// Hart parked in `wfi` (no deliverable interrupt).
    pub(crate) waiting: bool,
    /// Retired-instruction counts by opcode (encoding bits 6:0) for the
    /// Instruction Counter tool view. Kept in lockstep with `instret`, so
    /// the counts always sum to it.
    pub(crate) opcode_counts: BTreeMap<u32, u64>,
    /// Armed timer period in retired instructions (None = disarmed).
    pub(crate) timer_interval: Option<u64>,
    /// instret value at which the armed timer next fires.
    pub(crate) timer_deadline: u64,
    /// Pending software interrupt request (no producer yet; tools later).
    pub(crate) software_pending: bool,
    /// One-shot breakpoint skip armed by `continue_after_stop` so the
    /// breakpoint under the stopped pc does not re-fire immediately.
    pub(crate) skip_break_once: Option<u32>,
}

// CSR addresses (RARS's set).
pub mod csr {
    pub const USTATUS: u16 = 0x000;
    pub const FFLAGS: u16 = 0x001;
    pub const FRM: u16 = 0x002;
    pub const FCSR: u16 = 0x003;
    pub const UIE: u16 = 0x004;
    pub const UTVEC: u16 = 0x005;
    pub const USCRATCH: u16 = 0x040;
    pub const UEPC: u16 = 0x041;
    pub const UCAUSE: u16 = 0x042;
    pub const UTVAL: u16 = 0x043;
    pub const UIP: u16 = 0x044;
    pub const CYCLE: u16 = 0xC00;
    pub const TIME: u16 = 0xC01;
    pub const INSTRET: u16 = 0xC02;
    pub const CYCLEH: u16 = 0xC80;
    pub const TIMEH: u16 = 0xC81;
    pub const INSTRETH: u16 = 0xC82;

    /// Counters are read-only for programs.
    pub fn is_read_only(id: u16) -> bool {
        matches!(id, CYCLE | TIME | INSTRET | CYCLEH | TIMEH | INSTRETH)
    }

    /// fflags/frm/fcsr alias each other; writes resync all three.
    pub fn is_fp_control(id: u16) -> bool {
        matches!(id, FFLAGS | FRM | FCSR)
    }
}

/// Interpret an FP register word as a single-precision value under the
/// NaN-boxing rule (a word whose upper 32 bits are not all ones reads as the
/// canonical NaN). Register-file views can pair this with
/// `rvasm::abi_freg_name` for column headers.
pub fn f32_of_freg(bits: u64) -> f32 {
    f32::from_bits(fp::single_bits(bits))
}

/// Interpret an FP register word as a double-precision value.
pub fn f64_of_freg(bits: u64) -> f64 {
    f64::from_bits(bits)
}

impl Machine {
    /// Build a machine from an assembled program. Registers start at zero
    /// except $gp and $sp (RARS initialization).
    pub fn new(program: Program, host: Box<dyn Host>, config: MachineConfig) -> Self {
        let layout = config.layout.clone();
        let mut m = Machine {
            regs: [0; 32],
            fregs: [0; 32],
            csrs: std::collections::BTreeMap::new(),
            pc: program.text_base,
            mem: Memory::default(),
            mmio: Mmio::new(layout.mmio_base),
            layout,
            config,
            program,
            breakpoints: BTreeSet::new(),
            journal: Journal::new(8192),
            host,
            terminated: None,
            instret: 0,
            waiting: false,
            opcode_counts: BTreeMap::new(),
            timer_interval: None,
            timer_deadline: 0,
            software_pending: false,
            skip_break_once: None,
        };
        m.regs[3] = 0x1000_8000; // gp
        m.regs[2] = 0x7fff_effc; // sp
        m.load_program_image();
        m
    }

    fn load_program_image(&mut self) {
        for s in &self.program.statements {
            self.mem.write_bytes(s.addr, &s.encoding.to_le_bytes());
        }
        if !self.program.data.bytes.is_empty() {
            self.mem.write_bytes(self.program.data.base, &self.program.data.bytes);
        }
        // `.extern` reservations: zero-fill the reserved regions so the
        // addresses the symbols point at exist in the image (they read zero
        // until the program stores into them).
        for (addr, bytes) in &self.program.extern_chunks {
            self.mem.write_bytes(*addr, bytes);
        }
    }

    pub fn program(&self) -> &Program {
        &self.program
    }

    pub fn pc(&self) -> u32 {
        self.pc
    }

    pub fn reg(&self, index: usize) -> u64 {
        self.regs[index]
    }

    pub fn freg(&self, index: usize) -> u64 {
        self.fregs[index]
    }    pub fn csr(&self, id: u16) -> u64 {
        match id {
            // Sequential model: cycle/time track instret. `time` reads as the
            // retired-instruction clock; the wall-clock time stays with the
            // host-driven time syscall (a7 = 30).
            csr::CYCLE | csr::CYCLEH | csr::TIME | csr::TIMEH | csr::INSTRET | csr::INSTRETH => self.instret,
            other => self.csrs.get(&other).copied().unwrap_or(0),
        }
    }

    pub fn instret(&self) -> u64 {
        self.instret
    }

    /// Retired-instruction counts grouped by opcode (encoding bits 6:0),
    /// sorted by opcode — the Instruction Counter / Statistics tool view.
    /// Always sums to `instret()`: both rewind together on backstep.
    pub fn opcode_counts(&self) -> Vec<(u32, u64)> {
        self.opcode_counts.iter().map(|(op, n)| (*op, *n)).collect()
    }

    pub fn exit_code(&self) -> Option<i32> {
        match &self.terminated {
            Some(Halt::Exit { code }) => Some(*code),
            _ => None,
        }
    }

    pub fn is_terminated(&self) -> bool {
        self.terminated.is_some()
    }

    pub fn halt_reason(&self) -> Option<&Halt> {
        self.terminated.as_ref()
    }

    pub fn set_breakpoint(&mut self, addr: u32, on: bool) {
        if on {
            self.breakpoints.insert(addr);
        } else {
            self.breakpoints.remove(&addr);
        }
    }

    pub fn breakpoints(&self) -> &BTreeSet<u32> {
        &self.breakpoints
    }

    pub fn layout(&self) -> &MemLayout {
        &self.layout
    }

    /// True when the machine executes the 64-bit ISA (register width for
    /// arithmetic, compares, and the RV64-only instructions).
    pub fn rv64(&self) -> bool {
        self.config.rv64
    }

    /// Read memory bytes for data views (no side effects).
    pub fn peek_bytes(&self, addr: u32, buf: &mut [u8]) -> Result<(), MemError> {
        self.mem.read_bytes(addr, buf)
    }

    pub(crate) fn valid_addr(&self, addr: u32) -> bool {
        let l = &self.layout;
        (addr >= l.text_base && addr < l.text_base + l.text_len)
            || (addr >= l.data_base && addr < l.stack_top)
            || (addr >= l.kernel_base)
    }

    pub(crate) fn in_text(&self, addr: u32) -> bool {
        addr >= self.layout.text_base && addr < self.layout.text_base + self.layout.text_len
    }

    pub(crate) fn error(&mut self, message: String) -> StepOutcome {
        let h = Halt::Error { message };
        self.terminated = Some(h.clone());
        StepOutcome::halted(h)
    }

    /// Execute exactly one instruction, or report a breakpoint stop. The
    /// space between instructions is also the interrupt delivery point: after
    /// an instruction retires (and before the next one), one pending
    /// interrupt may trap into the handler.
    pub fn step(&mut self) -> StepOutcome {
        if self.terminated.is_some() {
            return StepOutcome::empty();
        }
        if self.waiting {
            // Hart parked at a wfi: see whether anything can wake it. A
            // stalled wake keeps the machine waiting and reports a no-op.
            if !self.refresh_wake() {
                return StepOutcome { executed: false, pc_before: self.pc, events: Vec::new(), changes: Vec::new() };
            }
            self.waiting = false;
        }
        // Breakpoints fire before the instruction at the marked address runs
        // (unless continue_after_stop armed a one-shot skip for this pc).
        let skip = self.skip_break_once.take() == Some(self.pc);
        if self.breakpoints.contains(&self.pc) && !skip {
            self.terminated = Some(Halt::Breakpoint);
            return StepOutcome::halted(Halt::Breakpoint);
        }
        if !self.in_text(self.pc) {
            return self.dropped_off();
        }
        if (self.pc & 0x3) != 0 {
            // A misaligned fetch is a synchronous exception like any other
            // once a handler is installed; without utvec it halts as before.
            let pc = self.pc;
            if self.read_csr(csr::UTVEC) != 0 {
                let mut out = StepOutcome { executed: false, pc_before: pc, events: Vec::new(), changes: Vec::new() };
                self.take_trap(crate::trap::exc::INSN_MISALIGNED, u64::from(pc), false, pc, &mut out.changes);
                return self.trap_entry_breakpoint(out);
            }
            return self.error(format!("instruction address 0x{pc:08x} is not word-aligned"));
        }

        self.journal.begin_statement();
        let pc_before = self.pc;
        self.journal.push(Undo::Pc { old: pc_before });
        self.pc += 4;

        let Ok(word) = self.mem.read_u32(pc_before) else {
            return self.error(format!("cannot fetch instruction at 0x{pc_before:08x}"));
        };

        let outcome = exec::execute(self, word, pc_before);
        // A wfi with nothing pending parked the hart: pc stays at the wfi,
        // nothing retires, and the run loop takes over the waiting.
        if self.waiting {
            self.journal.push(Undo::Waiting { old: false });
            return outcome.outcome;
        }
        // Count the instruction when it actually retired. Halting
        // instructions (the exit ecall, ebreak) report executed:false and
        // retire nothing, so the per-opcode counts stay a partition of
        // instret — the opcode rides the Retired record so backstep rewinds
        // both together.
        if outcome.outcome.executed {
            self.instret += 1;
            let opcode = word & 0x7f;
            *self.opcode_counts.entry(opcode).or_insert(0) += 1;
            self.journal.push(Undo::Retired { opcode });
        }
        if outcome.terminated_now {
            // Synchronous exceptions (address faults, illegal instructions)
            // vector through utvec when a handler is configured, matching
            // RARS's "exception handler loaded" behavior; without one they
            // halt with the error as before. Exits and ebreak stay halts.
            if let Some((cause, tval)) = outcome.exception {
                if self.read_csr(csr::UTVEC) != 0 {
                    let mut out = outcome.outcome;
                    out.events.clear(); // the deferred error halt never happened
                    // The faulting instruction did not retire: uepc points
                    // back at it so uret can retry (or report) it.
                    self.take_trap(cause, tval, false, pc_before, &mut out.changes);
                    return self.trap_entry_breakpoint(out);
                }
                // Keep the halt and record it on the machine (halt_reason,
                // is_terminated) the way every other stop does.
                if let Some(Event::Halted(h)) = outcome.outcome.events.last() {
                    self.terminated = Some(h.clone());
                }
                return outcome.outcome;
            }
            return outcome.outcome;
        }

        // Delivery point: one pending interrupt may trap between the
        // instruction that just retired and the next one.
        let mut outcome = outcome.outcome;
        self.poll_input();
        if let Some(cause) = self.deliverable_interrupt() {
            self.take_trap(cause, 0, true, self.pc, &mut outcome.changes);
            return self.trap_entry_breakpoint(outcome);
        }

        // Cliff: PC moved past the last statement.
        if self.pc >= self.program.text_end() {
            return self.dropped_off();
        }
        outcome
    }

    /// A trap just entered the handler: honor the RARS gap where breakpoints
    /// must also fire on trap-handler entry (RARS PR #225).
    fn trap_entry_breakpoint(&mut self, mut out: StepOutcome) -> StepOutcome {
        if self.breakpoints.contains(&self.pc) {
            self.terminated = Some(Halt::Breakpoint);
            out.events.push(Event::Halted(Halt::Breakpoint));
        }
        out
    }

    fn dropped_off(&mut self) -> StepOutcome {
        let h = Halt::DroppedOff;
        self.terminated = Some(h.clone());
        StepOutcome::halted(h)
    }

    /// Run until a halt, `max_steps` instructions have executed, or the hart
    /// parked in `wfi` with nothing left that could wake it.
    pub fn run(&mut self, max_steps: Option<u64>) -> Vec<Event> {
        let mut events = Vec::new();
        let mut steps = 0u64;
        loop {
            let outcome = self.step();
            let halted = outcome.events.iter().any(|e| matches!(e, Event::Halted(_)));
            events.extend(outcome.events);
            if halted {
                break;
            }
            if self.waiting {
                // Parked: sleep one tick, then refresh the wake sources (host
                // input, timer). A wake falls back into stepping so the next
                // pass delivers the interrupt; with no armed timer and no
                // input the hart can only be woken by a future keystroke, so
                // hand control back to the driver (call run again later).
                self.host_mut().sleep_ms(1);
                if self.refresh_wake() {
                    self.waiting = false;
                    continue;
                }
                break;
            }
            steps += 1;
            if let Some(limit) = max_steps {
                if steps >= limit {
                    self.terminated = Some(Halt::Limit);
                    events.push(Event::Halted(Halt::Limit));
                    break;
                }
            }
        }
        events
    }

    /// Undo the most recently executed statement — an instruction, or a trap
    /// entry, or a parked wfi. Returns false when the journal is exhausted or
    /// the program has exited.
    pub fn backstep(&mut self) -> bool {
        let Some(recs) = self.journal.pop_statement() else {
            return false;
        };
        // Only statements that retired an instruction rewind the counter;
        // trap entries and parked wfis never incremented it. The opcode
        // comes along so its per-opcode count rewinds with instret.
        let retired_opcode = recs.iter().find_map(|r| match r {
            Undo::Retired { opcode } => Some(*opcode),
            _ => None,
        });
        for rec in recs {
            match rec {
                Undo::Reg { index, old } => {
                    if index != 0 {
                        self.regs[index] = old;
                    }
                }
                Undo::FReg { index, old } => {
                    self.fregs[index] = old;
                }
                Undo::Csr { id, old } => {
                    self.csrs.insert(id, old);
                }
                Undo::Mem { addr, old, width } => {
                    let bytes = old.to_le_bytes();
                    self.mem.write_bytes(addr, &bytes[..width as usize]);
                }
                Undo::Pc { old } => self.pc = old,
                Undo::TimerDeadline { old } => self.timer_deadline = old,
                Undo::XmitEdge => self.mmio.restore_xmit_edge(),
                Undo::SoftwareIrq => self.software_pending = true,
                Undo::HexKeys => self.mmio.keys_pending = true,
                Undo::Waiting { old } => self.waiting = old,
                Undo::Retired { .. } | Undo::Boundary => {}
            }
        }
        self.terminated = None;
        if let Some(opcode) = retired_opcode {
            self.instret = self.instret.saturating_sub(1);
            let count = self.opcode_counts.entry(opcode).or_insert(0);
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.opcode_counts.remove(&opcode);
            }
        }
        true
    }

    /// Reset to the post-assembly state, keeping breakpoints. Simulated
    /// device/trap state (armed timer, waiting flag, pending requests) is
    /// cleared; the host re-arms the timer after a reset.
    pub fn reset(&mut self) {
        self.regs = [0; 32];
        self.fregs = [0; 32];
        self.csrs.clear();
        self.regs[3] = 0x1000_8000;
        self.regs[2] = 0x7fff_effc;
        self.pc = self.program.text_base;
        self.mem.clear();
        self.load_program_image();
        self.journal.records.clear();
        self.terminated = None;
        self.instret = 0;
        self.opcode_counts.clear();
        self.waiting = false;
        self.software_pending = false;
        self.skip_break_once = None;
        self.clear_timer();
    }

    /// Publish program arguments the RARS way: argv strings go just below
    /// $sp, $a1 points at the argv pointer array (NULL-terminated) and
    /// $a0 = argc. Call after `new` and before running; writes land directly
    /// (nothing has executed, so there is nothing to journal).
    pub fn set_program_args(&mut self, args: &[String]) {
        let mut top = self.layout.stack_top;
        // Strings first, packed downward from the stack top.
        let mut string_addrs = Vec::with_capacity(args.len());
        for arg in args {
            top = top.wrapping_sub(arg.len() as u32 + 1); // + NUL
            self.mem.write_bytes(top, arg.as_bytes());
            self.mem.write_bytes(top + arg.len() as u32, &[0]);
            string_addrs.push(top);
        }
        // Then the pointer array, word-aligned, with an argv[argc] = NULL.
        top &= !0x3;
        top -= 4 * (args.len() as u32 + 1);
        for (i, addr) in string_addrs.iter().enumerate() {
            self.mem.write_bytes(top + 4 * i as u32, &addr.to_le_bytes());
        }
        self.mem.write_bytes(top + 4 * args.len() as u32, &0u32.to_le_bytes());
        self.regs[10] = args.len() as u64; // a0 = argc
        self.regs[11] = u64::from(top); // a1 = argv
    }

    // ---- Internal helpers used by exec/syscalls ----

    pub(crate) fn write_reg(&mut self, index: usize, value: u64, changes: &mut Vec<Change>) {
        if index == 0 {
            return; // x0 is hardwired to zero
        }
        let old = self.regs[index];
        self.journal.push(Undo::Reg { index, old });
        self.regs[index] = value;
        changes.push(Change::Reg { index, old, new: value });
    }

    pub(crate) fn write_freg(&mut self, index: usize, value: u64, changes: &mut Vec<Change>) {
        let old = self.fregs[index];
        self.journal.push(Undo::FReg { index, old });
        self.fregs[index] = value;
        changes.push(Change::FReg { index, old, new: value });
    }

    /// OR exception bits into fflags, the way FP instructions accumulate
    /// them. Each write lands in the journal so backstep strips the flags.
    pub(crate) fn acc_fflags(&mut self, flags: u8, changes: &mut Vec<Change>) {
        if flags == 0 {
            return;
        }
        let old = self.read_csr(csr::FFLAGS);
        self.write_csr_raw(csr::FFLAGS, (old | flags as u64) & 0x1f, changes);
    }

    pub(crate) fn write_csr_raw(&mut self, id: u16, value: u64, changes: &mut Vec<Change>) {
        if csr::is_read_only(id) {
            return;
        }
        // The FP control CSRs alias: fcsr = frm << 5 | fflags. Writing any
        // one of them resyncs the others so reads stay consistent.
        if csr::is_fp_control(id) {
            self.write_fp_csr(id, value, changes);
            return;
        }
        let old = self.csrs.get(&id).copied().unwrap_or(0);
        self.journal.push(Undo::Csr { id, old });
        self.csrs.insert(id, value);
        changes.push(Change::Csr { id, old, new: value });
    }

    fn write_fp_csr(&mut self, id: u16, value: u64, changes: &mut Vec<Change>) {
        let mut fflags = self.read_csr(csr::FFLAGS);
        let mut frm = self.read_csr(csr::FRM);
        match id {
            csr::FFLAGS => fflags = value & 0x1f,
            csr::FRM => frm = value & 0x7,
            _ => {
                // fcsr carries both fields.
                fflags = value & 0x1f;
                frm = (value >> 5) & 0x7;
            }
        }
        let fcsr = (frm << 5) | fflags;
        for (csr_id, v) in [(csr::FFLAGS, fflags), (csr::FRM, frm), (csr::FCSR, fcsr)] {
            let old = self.csrs.get(&csr_id).copied().unwrap_or(0);
            if old == v {
                continue;
            }
            self.journal.push(Undo::Csr { id: csr_id, old });
            self.csrs.insert(csr_id, v);
            changes.push(Change::Csr { id: csr_id, old, new: v });
        }
    }

    pub(crate) fn read_csr(&self, id: u16) -> u64 {
        match id {
            csr::CYCLE | csr::CYCLEH | csr::TIME | csr::TIMEH | csr::INSTRET | csr::INSTRETH => self.instret,
            other => self.csrs.get(&other).copied().unwrap_or(0),
        }
    }

    pub(crate) fn load_bytes(&mut self, addr: u32, width: u32, changes: &mut Vec<Change>) -> Result<u64, MemError> {
        if !self.config.allow_unaligned && !addr.is_multiple_of(width) {
            return Err(MemError::Unaligned { addr, width });
        }
        // MMIO registers answer before the memory path; device reads have
        // side effects (receiver data pops a key), so no journal entry.
        if self.mmio.in_window(addr) {
            let val = {
                let host: &mut dyn Host = &mut *self.host;
                self.mmio.load(addr, width, host)
            };
            changes.push(Change::Mem { addr, old: 0, new: val, width: width as u8 });
            return Ok(val);
        }
        if !self.valid_addr(addr) {
            return Err(MemError::AccessViolation { addr });
        }
        let mut buf = [0u8; 8];
        self.mem.read_bytes(addr, &mut buf[..width as usize])?;
        let val = match width {
            1 => buf[0] as u64,
            2 => u16::from_le_bytes([buf[0], buf[1]]) as u64,
            4 => u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as u64,
            // fld moves a full doubleword into an FP register.
            _ => u64::from_le_bytes(buf),
        };
        changes.push(Change::Mem { addr, old: 0, new: val, width: width as u8 });
        Ok(val)
    }

    /// Sign-extended load helper (lb/lh/lw).
    pub(crate) fn load_signed(&mut self, addr: u32, width: u32, changes: &mut Vec<Change>) -> Result<u64, MemError> {
        let v = self.load_bytes(addr, width, changes)?;
        Ok(match width {
            1 => v as u8 as i8 as i64 as u64,
            2 => v as u16 as i16 as i64 as u64,
            _ => v as u32 as i32 as i64 as u64,
        })
    }

    pub(crate) fn store_bytes(&mut self, addr: u32, value: u64, width: u32, changes: &mut Vec<Change>) -> Result<(), MemError> {
        if !self.config.allow_unaligned && !addr.is_multiple_of(width) {
            return Err(MemError::Unaligned { addr, width });
        }
        // MMIO registers swallow the store (device side effects instead of
        // memory); the effects (output, key consumption) cannot be undone,
        // so unlike normal stores nothing enters the backstep journal.
        if self.mmio.in_window(addr) {
            {
                let host: &mut dyn Host = &mut *self.host;
                self.mmio.store(addr, value, host);
            }
            changes.push(Change::Mem { addr, old: 0, new: value, width: width as u8 });
            return Ok(());
        }
        if !self.valid_addr(addr) {
            return Err(MemError::AccessViolation { addr });
        }
        let old = match width {
            1 => self.mem.read_u8(addr).unwrap_or(0) as u64,
            2 => self.mem.read_u16(addr).unwrap_or(0) as u64,
            4 => self.mem.read_u32(addr).unwrap_or(0) as u64,
            _ => self.mem.read_u64(addr).unwrap_or(0),
        };
        let bytes = value.to_le_bytes();
        self.mem.write_bytes(addr, &bytes[..width as usize]);
        self.journal.push(Undo::Mem { addr, old, width: width as u8 });
        changes.push(Change::Mem { addr, old, new: value, width: width as u8 });
        Ok(())
    }

    pub(crate) fn host_mut(&mut self) -> &mut dyn Host {
        &mut *self.host
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;

    pub fn asm(src: &str) -> rvasm::AsmResult {
        let files = vec![rvasm::InputFile { name: "t.s".into(), source: src.into() }];
        rvasm::assemble(&files, &rvasm::AsmConfig::default())
    }

    pub fn machine_with(src: &str, host: Box<dyn Host>) -> Machine {
        let r = asm(src);
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        Machine::new(r.program.unwrap(), host, MachineConfig::default())
    }

    pub fn machine(src: &str) -> Machine {
        machine_with(src, Box::new(ScriptHost::default()))
    }

    /// Assemble and simulate in RV64 mode (RARS's 64-bit setting).
    pub fn machine64_with(src: &str, host: Box<dyn Host>) -> Machine {
        let files = vec![rvasm::InputFile { name: "t.s".into(), source: src.into() }];
        let acfg = rvasm::AsmConfig { rv64: true, ..rvasm::AsmConfig::default() };
        let r = rvasm::assemble(&files, &acfg);
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let mcfg = MachineConfig { rv64: true, ..MachineConfig::default() };
        Machine::new(r.program.unwrap(), host, mcfg)
    }

    pub fn machine64(src: &str) -> Machine {
        machine64_with(src, Box::new(ScriptHost::default()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;

    #[test]
    fn fib_program_runs() {
        let src = "\
main:
    li a0, 10
    li a1, 0
    li a2, 1
fib_loop:
    beqz a0, done
    add a3, a1, a2
    mv a1, a2
    mv a2, a3
    addi a0, a0, -1
    j fib_loop
done:
    mv a0, a1
    li a7, 1
    ecall
    li a7, 10
    ecall
";
        let mut m = machine(src);
        let events = m.run(None);
        assert_eq!(m.exit_code(), Some(0));
        assert_eq!(events.last(), Some(&Event::Halted(Halt::Exit { code: 0 })));
        assert_eq!(m.reg(10), 55);
    }

    #[test]
    fn print_string_outputs() {
        let src = "\
.data
msg: .asciz \"hello\"
.text
la a0, msg
li a7, 4
ecall
li a7, 10
ecall
";
        let host = ScriptHost::default();
        let mut m = machine_with(src, Box::new(host.clone()));
        m.run(None);
        assert_eq!(host.take_output(), "hello");
    }

    #[test]
    fn backstep_restores_state() {
        let src = "\
    li a0, 7
    addi a0, a0, 5
    sw a0, 0(sp)
";
        let mut m = machine(src);
        let pc0 = m.pc();
        m.step();
        m.step();
        m.step();
        assert_eq!(m.reg(10), 12);
        assert_eq!(m.instret(), 3);
        assert!(m.backstep());
        assert_eq!(m.instret(), 2);
        assert!(m.backstep());
        assert_eq!(m.reg(10), 7);
        assert!(m.backstep());
        assert_eq!(m.pc(), pc0);
        assert_eq!(m.instret(), 0);
        assert!(!m.backstep()); // journal exhausted
    }

    #[test]
    fn breakpoint_stops_before_instruction() {
        let src = "\
    addi a0, a0, 1
    addi a0, a0, 2
    addi a0, a0, 3
";
        let mut m = machine(src);
        m.set_breakpoint(m.program().text_base + 4, true);
        let events = m.run(None);
        assert_eq!(events.last(), Some(&Event::Halted(Halt::Breakpoint)));
        assert_eq!(m.reg(10), 1); // first instruction ran, second did not
        assert_eq!(m.pc(), m.program().text_base + 4);
    }

    #[test]
    fn breakpoint_resume_then_step_back() {
        let src = "\
    addi a0, a0, 1
    addi a0, a0, 2
";
        let mut m = machine(src);
        m.set_breakpoint(m.program().text_base + 4, true);
        m.run(None);
        // A breakpoint stop is a pause, not an exit: backstep is allowed and
        // clears the stop so stepping can resume.
        assert!(m.backstep());
        assert!(!m.is_terminated());
        assert_eq!(m.pc(), m.program().text_base);
    }

    #[test]
    fn ebreak_stops() {
        let mut m = machine("    ebreak\n");
        let events = m.run(None);
        assert_eq!(events.last(), Some(&Event::Halted(Halt::Ebreak)));
    }

    #[test]
    fn reset_restores_program() {
        let mut m = machine("    addi a0, a0, 5\n");
        m.run(None);
        assert!(m.is_terminated());
        m.reset();
        assert!(!m.is_terminated());
        assert_eq!(m.reg(10), 0);
        assert_eq!(m.pc(), m.program().text_base);
    }

    #[test]
    fn access_violation_errors() {
        let mut m = machine("    li t0, 8\n    lw a0, 0(t0)\n");
        let events = m.run(None);
        assert!(matches!(events.last(), Some(Event::Halted(Halt::Error { .. }))));
    }

    #[test]
    fn continue_after_stop_runs_past_breakpoint_and_refires_on_return() {
        let src = "\
    li t0, 3
loop:
    addi a0, a0, 1
    blt a0, t0, loop
    li a7, 10
    ecall
";
        let mut m = machine(src);
        let loop_addr = m.program().statements[1].addr;
        m.set_breakpoint(loop_addr, true);
        // First arrival: stop before a0 is touched.
        let events = m.run(None);
        assert_eq!(events.last(), Some(&Event::Halted(Halt::Breakpoint)));
        assert_eq!(m.reg(10), 0);
        // Continue: the breakpoint under the pc is skipped once...
        m.continue_after_stop();
        assert!(!m.is_terminated());
        // ...and fires again on the NEXT arrival at the same address.
        let events = m.run(None);
        assert_eq!(events.last(), Some(&Event::Halted(Halt::Breakpoint)));
        assert_eq!(m.reg(10), 1);
        // Each pass needs its own continue (a0 = 2 next) before the loop
        // condition finally releases and the program exits.
        m.continue_after_stop();
        let events = m.run(None);
        assert_eq!(events.last(), Some(&Event::Halted(Halt::Breakpoint)));
        assert_eq!(m.reg(10), 2);
        m.continue_after_stop();
        let events = m.run(None);
        assert_eq!(events.last(), Some(&Event::Halted(Halt::Exit { code: 0 })));
        assert_eq!(m.reg(10), 3);
        // Real terminations are not continuable.
        m.continue_after_stop();
        assert!(m.is_terminated());
    }

    #[test]
    fn continue_after_stop_resumes_past_ebreak() {
        let src = "\
    addi a0, a0, 5
    ebreak
    addi a0, a0, 2
    li a7, 93
    ecall
";
        let mut m = machine(src);
        let events = m.run(None);
        assert_eq!(events.last(), Some(&Event::Halted(Halt::Ebreak)));
        m.continue_after_stop();
        assert!(!m.is_terminated());
        m.run(None);
        assert_eq!(m.reg(10), 7);
        assert_eq!(m.exit_code(), Some(7)); // service 93 exits with a0
    }

    #[test]
    fn time_csr_tracks_instret() {
        let src = "\
    csrrs a0, time, x0
    csrrs a1, timeh, x0
";
        let mut m = machine(src);
        m.run(None);
        // Each read samples the clock before its own instruction retires.
        assert_eq!(m.reg(10), 0);
        assert_eq!(m.reg(11), 1);
        assert_eq!(m.csr(csr::TIME), m.instret());
        assert_eq!(m.csr(csr::TIMEH), m.instret());
        assert_eq!(m.instret(), 2);
    }

    #[test]
    fn dropped_off_and_limit_do_not_vector() {
        // A single nop past the end of text drops off even with a handler
        // installed: only synchronous exceptions vector.
        let mut m = machine("    nop\n");
        m.csrs.insert(csr::UTVEC, u64::from(m.program().text_base + 0x400));
        let events = m.run(None);
        assert_eq!(events.last(), Some(&Event::Halted(Halt::DroppedOff)));
        let mut m = machine("loop: j loop\n");
        m.csrs.insert(csr::UTVEC, u64::from(m.program().text_base + 0x400));
        let events = m.run(Some(10));
        assert_eq!(events.last(), Some(&Event::Halted(Halt::Limit)));
    }

    #[test]
    fn run_limit_stops() {
        let src = "loop: j loop\n";
        let mut m = machine(src);
        let events = m.run(Some(100));
        assert_eq!(events.last(), Some(&Event::Halted(Halt::Limit)));
        assert_eq!(m.instret(), 100);
    }

    #[test]
    fn program_args_lay_out_argv() {
        let mut m = machine("    nop\n");
        m.set_program_args(&["foo".into(), "barbaz".into()]);
        assert_eq!(m.reg(10), 2); // a0 = argc
        let argv = m.reg(11) as u32; // a1 = argv
        let mut word = [0u8; 4];
        m.peek_bytes(argv + 8, &mut word).unwrap();
        assert_eq!(word, [0; 4]); // argv[argc] = NULL
        let mut strings = Vec::new();
        for i in 0..2 {
            m.peek_bytes(argv + 4 * i, &mut word).unwrap();
            strings.push(u32::from_le_bytes(word));
        }
        let mut buf = [0u8; 7];
        m.peek_bytes(strings[0], &mut buf).unwrap();
        assert_eq!(&buf[..4], b"foo\0");
        m.peek_bytes(strings[1], &mut buf).unwrap();
        assert_eq!(&buf, b"barbaz\0");
        // Strings and the pointer array live below the initial $sp.
        assert!(strings.iter().chain(&[argv]).all(|a| *a < 0x7fff_fffc));
    }

    // ---- .extern reservations ----

    #[test]
    fn extern_reservations_map_into_memory() {
        // .extern reserves address space at the extern base without emitting
        // initializer bytes; the machine zero-maps it and the program uses it
        // like static data.
        let src = "\
.extern var 4
.text
main:
    la t0, var
    li t1, 77
    sw t1, 0(t0)
    lw a0, 0(t0)
    li a7, 10
    ecall
";
        let r = asm(src);
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let var = r.program.as_ref().unwrap().symbols.get("var").unwrap().addr;
        assert_eq!(var, 0x1000_0000); // the data-segment base (extern region)
        let mut m = Machine::new(r.program.unwrap(), Box::new(ScriptHost::default()), MachineConfig::default());
        // The reservation zero-mapped the region before any execution.
        let mut buf = [0u8; 4];
        m.peek_bytes(var, &mut buf).unwrap();
        assert_eq!(buf, [0, 0, 0, 0]);
        m.run(None);
        // The store to the extern address succeeded and the load read it back.
        assert_eq!(m.reg(10), 77);
    }

    #[test]
    fn extern_reservations_advance_and_stay_distinct() {
        // Two reservations: the second symbol's region does not overlap the
        // first, so independent stores stay independent.
        let src = "\
.extern a 4
.extern b 4
.text
main:
    la t0, a
    la t1, b
    li t2, 1
    li t3, 2
    sw t2, 0(t0)
    sw t3, 0(t1)
    lw a0, 0(t0)
    lw a1, 0(t1)
    li a7, 10
    ecall
";
        let r = asm(src);
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let p = r.program.as_ref().unwrap();
        let a = p.symbols.get("a").unwrap().addr;
        let b = p.symbols.get("b").unwrap().addr;
        assert_eq!(a, 0x1000_0000);
        assert_eq!(b, a + 4);
        let mut m = Machine::new(p.clone(), Box::new(ScriptHost::default()), MachineConfig::default());
        m.run(None);
        assert_eq!(m.reg(10), 1);
        assert_eq!(m.reg(11), 2);
    }

    // ---- memory configuration presets ----

    #[test]
    fn compact_data_preset_runs_a_data_at_zero_layout() {
        // CompactDataAtZero: the data segment base is 0 and static data sits
        // data_base + 0x10000 into it (Default's gap), so the matching
        // assembler config moves its data base to 0x10000.
        let files = vec![rvasm::InputFile {
            name: "t.s".into(),
            source: ".data\nv: .word 99\n.text\nlw a0, v\nli a7, 10\necall\n".into(),
        }];
        let acfg = rvasm::AsmConfig { data_base: 0x0001_0000, ..rvasm::AsmConfig::default() };
        let r = rvasm::assemble(&files, &acfg);
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let program = r.program.unwrap();
        assert_eq!(program.data.base, 0x0001_0000);
        let cfg = MachineConfig { layout: MemLayout::compact_data_at_zero(), ..MachineConfig::default() };
        let mut m = Machine::new(program, Box::new(ScriptHost::default()), cfg);
        assert_eq!(m.layout().name(), "CompactDataAtZero");
        m.run(None);
        assert_eq!(m.reg(10), 99);
    }

    #[test]
    fn compact_text_preset_fetches_from_address_zero() {
        // CompactTextAtZero: text at 0, data segment at the Default base.
        let files = vec![rvasm::InputFile { name: "t.s".into(), source: "    li a0, 5\n    ecall\n".into() }];
        let acfg = rvasm::AsmConfig { text_base: 0x0000_0000, ..rvasm::AsmConfig::default() };
        let r = rvasm::assemble(&files, &acfg);
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        let program = r.program.unwrap();
        assert_eq!(program.text_base, 0);
        let cfg = MachineConfig { layout: MemLayout::compact_text_at_zero(), ..MachineConfig::default() };
        let mut m = Machine::new(program, Box::new(ScriptHost::default()), cfg);
        assert_eq!(m.layout().name(), "CompactTextAtZero");
        assert_eq!(m.pc(), 0);
        m.run(None);
        assert_eq!(m.reg(10), 5);
    }

    // ---- per-opcode execution counters ----

    #[test]
    fn opcode_counts_group_retired_instructions() {
        let src = "\
main:
    li a0, 3            # addi (opcode 0x13)
loop:
    beqz a0, done       # beq (0x63)
    addi a0, a0, -1     # addi
    j loop              # jal (0x6f)
done:
    nop                 # addi zero, zero, 0; drops off the bottom
";
        let mut m = machine(src);
        m.run(None);
        // a0 counts 3->0: three loop passes, the entry li, and the nop.
        let counts = m.opcode_counts();
        assert_eq!(counts, vec![(0x13, 5), (0x63, 4), (0x6f, 3)]);
        // The counts are a full partition of the retired instructions.
        let total: u64 = counts.iter().map(|(_, n)| n).sum();
        assert_eq!(total, m.instret());
    }

    #[test]
    fn backstep_rewinds_opcode_counts() {
        let mut m = machine("    addi a0, a0, 1\n    sw a0, 0(sp)\n");
        m.step();
        m.step();
        assert_eq!(m.opcode_counts(), vec![(0x13, 1), (0x23, 1)]);
        assert!(m.backstep());
        assert_eq!(m.opcode_counts(), vec![(0x13, 1)]);
        assert_eq!(m.instret(), 1);
        // Re-executing after the backstep counts again.
        m.step();
        assert_eq!(m.opcode_counts(), vec![(0x13, 1), (0x23, 1)]);
    }

    #[test]
    fn reset_clears_opcode_counts() {
        let mut m = machine("    addi a0, a0, 1\n");
        m.run(None);
        assert_eq!(m.opcode_counts(), vec![(0x13, 1)]);
        m.reset();
        assert!(m.opcode_counts().is_empty());
        assert_eq!(m.instret(), 0);
    }

    // ---- RV64 mode ----

    #[test]
    fn rv64_print_int_uses_low_32_bits() {
        // RARS's RV64 syscall convention: integer inputs come from the low
        // 32 bits of a0 and results sign-extend into it.
        let src = "\
    li t0, 1
    slli t0, t0, 32
    li a0, 65
    or a0, a0, t0           # 0x1_0000_0041
    li a7, 1
    ecall                   # prints the low 32 bits only
    li a0, -1
    li a7, 36
    ecall                   # PrintIntUnsigned of 0xffffffff
    li a7, 10
    ecall
";
        let host = ScriptHost::default();
        let mut m = machine64_with(src, Box::new(host.clone()));
        m.run(None);
        assert_eq!(host.take_output(), "654294967295");
    }

    #[test]
    fn rv64_read_int_sign_extends_result() {
        let src = "\
    li a7, 5
    ecall
    li a7, 1
    ecall
    li a7, 10
    ecall
";
        let host = ScriptHost::with_input(vec!["42".into()]);
        let mut m = machine64_with(src, Box::new(host));
        m.run(None);
        assert_eq!(m.reg(10), 42);
        // And a negative value sign-extends to the full 64-bit width.
        let host = ScriptHost::with_input(vec!["-7".into()]);
        let mut m = machine64_with(src, Box::new(host));
        m.run(None);
        assert_eq!(m.reg(10), (-7i64) as u64);
    }

    #[test]
    fn rv64_end_to_end_64bit_program() {
        // Assembler (AsmConfig::rv64) + machine (MachineConfig::rv64): store
        // two wide halves, load them back, add carrying past bit 32, and
        // print the low word.
        let src = "\
.data
vals: .word 0x22222222, 0x11111111   # one doubleword 0x1111111122222222
      .word 1, 0                     # ...and one holding 1
.text
main:
    la t2, vals
    ld t0, 0(t2)
    ld t1, 8(t2)
    add a0, t0, t1          # 0x1111111122222223
    sd a0, 8(t2)
    ld a1, 8(t2)
    li a7, 1
    mv a0, a1
    ecall                   # prints low 32 bits: 0x22222223 = 572662307
    li a7, 10
    ecall
";
        let host = ScriptHost::default();
        let mut m = machine64_with(src, Box::new(host.clone()));
        let events = m.run(None);
        assert_eq!(m.exit_code(), Some(0));
        assert_eq!(events.last(), Some(&Event::Halted(Halt::Exit { code: 0 })));
        assert_eq!(m.reg(11), 0x1111_1111_2222_2223);
        assert_eq!(host.take_output(), "572662307");
    }

    #[test]
    fn rv64_backstep_restores_wide_values() {
        let src = "\
    li t0, 1000000000000000
    addi t0, t0, 1
    sd t0, -4(sp)
";
        let mut m = machine64(src);
        m.step();
        for _ in 1..8 {
            m.step(); // finish the wide li chain
        }
        assert_eq!(m.reg(5), 1_000_000_000_000_000);
        m.step(); // addi
        assert_eq!(m.reg(5), 1_000_000_000_000_001);
        m.step(); // sd
        assert!(m.backstep());
        assert_eq!(m.reg(5), 1_000_000_000_000_001); // store undone, reg kept
        assert!(m.backstep());
        assert_eq!(m.reg(5), 1_000_000_000_000_000);
    }
}
