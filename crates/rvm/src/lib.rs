//! The machine: registers, CSRs, execution, syscalls, breakpoints, and the
//! backstep journal. Owned exclusively by one driver thread; all UI contact
//! flows through `Host` and returned events.

mod exec;
mod host;
mod mmio;
mod memory;
mod syscalls;

pub use host::{Host, ScriptHost, StdHost};

use crate::memory::{MemError, MemLayout, Memory};
use crate::mmio::Mmio;
use rvasm::Program;
use std::collections::{BTreeSet, VecDeque};

/// Machine-level settings.
#[derive(Debug, Clone, Default)]
pub struct MachineConfig {
    pub layout: MemLayout,
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
    Reg { index: usize, old: u64 },
    Csr { id: u16, old: u64 },
    Mem { addr: u32, old: u64, width: u8 },
    Pc { old: u32 },
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
    mmio: Mmio,
    layout: MemLayout,
    config: MachineConfig,
    program: Program,
    breakpoints: BTreeSet<u32>,
    journal: Journal,
    host: Box<dyn Host>,
    terminated: Option<Halt>,
    pub(crate) instret: u64,
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
    }

    pub fn csr(&self, id: u16) -> u64 {
        match id {
            // Sequential model: cycle tracks instret; time is cached by the
            // time syscall when programs read it.
            csr::CYCLE | csr::CYCLEH | csr::INSTRET | csr::INSTRETH => self.instret,
            other => self.csrs.get(&other).copied().unwrap_or(0),
        }
    }

    pub fn instret(&self) -> u64 {
        self.instret
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

    /// Execute exactly one instruction, or report a breakpoint stop.
    pub fn step(&mut self) -> StepOutcome {
        if self.terminated.is_some() {
            return StepOutcome::empty();
        }
        // Breakpoints fire before the instruction at the marked address runs.
        if self.breakpoints.contains(&self.pc) {
            self.terminated = Some(Halt::Breakpoint);
            return StepOutcome::halted(Halt::Breakpoint);
        }
        if !self.in_text(self.pc) {
            return self.dropped_off();
        }
        if (self.pc & 0x3) != 0 {
            return self.error(format!("instruction address 0x{:08x} is not word-aligned", self.pc));
        }

        self.journal.begin_statement();
        let pc_before = self.pc;
        self.journal.push(Undo::Pc { old: pc_before });
        self.pc += 4;

        let Ok(word) = self.mem.read_u32(pc_before) else {
            return self.error(format!("cannot fetch instruction at 0x{pc_before:08x}"));
        };

        let outcome = exec::execute(self, word, pc_before);
        // The instruction retired even when it halted the machine (an exit
        // ecall, for instance); RARS counts it.
        if outcome.outcome.executed {
            self.instret += 1;
        }
        if outcome.terminated_now {
            return outcome.outcome;
        }

        // Cliff: PC moved past the last statement.
        if self.pc >= self.program.text_end() {
            return self.dropped_off();
        }
        outcome.outcome
    }

    fn dropped_off(&mut self) -> StepOutcome {
        let h = Halt::DroppedOff;
        self.terminated = Some(h.clone());
        StepOutcome::halted(h)
    }

    /// Run until a halt or `max_steps` instructions have executed.
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

    /// Undo the most recently executed instruction. Returns false when the
    /// journal is exhausted or the program has exited.
    pub fn backstep(&mut self) -> bool {
        let Some(recs) = self.journal.pop_statement() else {
            return false;
        };
        for rec in recs {
            match rec {
                Undo::Reg { index, old } => {
                    if index != 0 {
                        self.regs[index] = old;
                    }
                }
                Undo::Csr { id, old } => {
                    self.csrs.insert(id, old);
                }
                Undo::Mem { addr, old, width } => {
                    let bytes = old.to_le_bytes();
                    self.mem.write_bytes(addr, &bytes[..width as usize]);
                }
                Undo::Pc { old } => self.pc = old,
                Undo::Boundary => {}
            }
        }
        self.terminated = None;
        self.instret = self.instret.saturating_sub(1);
        true
    }

    /// Reset to the post-assembly state, keeping breakpoints.
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

    pub(crate) fn write_csr_raw(&mut self, id: u16, value: u64, changes: &mut Vec<Change>) {
        if csr::is_read_only(id) {
            return;
        }
        let old = self.csrs.get(&id).copied().unwrap_or(0);
        self.journal.push(Undo::Csr { id, old });
        self.csrs.insert(id, value);
        changes.push(Change::Csr { id, old, new: value });
    }

    pub(crate) fn read_csr(&self, id: u16) -> u64 {
        match id {
            csr::CYCLE | csr::CYCLEH | csr::INSTRET | csr::INSTRETH => self.instret,
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
            _ => u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as u64,
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
}
