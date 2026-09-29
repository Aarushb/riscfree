//! The UI/simulation bridge: command and event types plus the simulation
//! thread. The thread owns the Machine exclusively; the UI never touches it.
//! Run-mode chunks keep the thread checking for Pause between batches so the
//! interface stays responsive during unlimited-speed runs. Console input
//! arrives on its own channel that the machine's host reads from directly.

use rvasm::Program;
use rvm::{Change, Halt, Machine, MachineConfig};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Instructions between pause checks in run mode. Small enough that the UI
/// never waits perceptibly, large enough that channel traffic is negligible.
const RUN_CHUNK: u64 = 100_000;

pub enum Cmd {
    /// Install a freshly assembled program (also resets the machine).
    Load(Box<Program>),
    Run,
    Pause,
    Step,
    Backstep,
    Reset,
    /// Toggle a breakpoint on a text-segment address.
    SetBreakpoint { addr: u32, on: bool },
    /// Read len bytes of memory for the data view.
    ReadMemory { addr: u32, len: u32 },
}

pub enum Evt {
    /// Program console output (print syscalls and the MMIO transmitter).
    Output(String),
    /// Response to ReadMemory: the bytes at the requested base.
    Memory { base: u32, bytes: Vec<u8> },
    /// Full register snapshot; sent after loads, run chunks, and halts.
    /// Boxed because 32 registers dwarf the other variants.
    State(Box<StateSnapshot>),
    /// One executed instruction in step mode, with its changes for narration.
    Stepped { text: String, line: u32, changes: Vec<Change>, pc: u32, instret: u64 },
    Halted { halt: Halt, pc: u32, instret: u64 },
    /// The machine loaded a program and is back at its starting state.
    Loaded { pc: u32, instret: u64 },
}

pub struct StateSnapshot {
    pub regs: [u64; 32],
    pub fregs: [u64; 32],
    pub pc: u32,
    pub instret: u64,
}

/// Console input shared between the UI (producer) and the machine's host
/// (consumer). A mutex around the receiver because the host is recreated per
/// program load while the channel lives for the whole app.
pub type InputChannel = Arc<Mutex<Receiver<String>>>;

struct ChannelHost {
    events: Sender<Evt>,
    input: InputChannel,
    pending: Option<String>,
    pending_pos: usize,
}

impl rvm::Host for ChannelHost {
    fn write_output(&mut self, text: &str) {
        self.events.send(Evt::Output(text.to_string())).ok();
    }

    fn read_line(&mut self) -> Option<String> {
        self.input.lock().ok()?.recv().ok()
    }

    fn read_char(&mut self) -> Option<u8> {
        if self.pending.as_ref().is_none_or(|s| self.pending_pos >= s.len()) {
            let line = self.read_line()?;
            self.pending = Some(line);
            self.pending_pos = 0;
        }
        let text = self.pending.as_ref()?;
        let byte = text.as_bytes().get(self.pending_pos).copied().unwrap_or(b'\n');
        self.pending_pos += 1;
        Some(byte)
    }

    fn sleep_ms(&mut self, ms: u64) {
        std::thread::sleep(Duration::from_millis(ms));
    }

    fn time_ms(&mut self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    fn get_cwd(&mut self) -> String {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

pub fn start_sim_thread(cmds: Receiver<Cmd>, events: Sender<Evt>, input: InputChannel) {
    let mut machine: Option<Machine> = None;
    let mut running = false;

    loop {
        // Idle: block for a command. Running: poll between chunks so Pause
        // arrives promptly.
        let cmd = if running {
            match cmds.try_recv() {
                Ok(c) => Some(c),
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(8));
                    None
                }
            }
        } else {
            match cmds.recv() {
                Ok(c) => Some(c),
                Err(_) => return,
            }
        };

        if let Some(cmd) = cmd {
            match cmd {
                Cmd::Load(program) => {
                    let host = ChannelHost {
                        events: events.clone(),
                        input: input.clone(),
                        pending: None,
                        pending_pos: 0,
                    };
                    let m = Machine::new(*program, Box::new(host), MachineConfig::default());
                    let (pc, instret) = (m.pc(), m.instret());
                    send_state(&events, &m);
                    events.send(Evt::Loaded { pc, instret }).ok();
                    machine = Some(m);
                    running = false;
                }
                Cmd::Run => {
                    if machine.as_ref().is_some_and(|m| !m.is_terminated()) {
                        running = true;
                    }
                }
                Cmd::Pause => running = false,
                Cmd::Step => {
                    if let Some(m) = machine.as_mut() {
                        if !m.is_terminated() {
                            step_once(m, &events);
                        }
                    }
                }
                Cmd::Backstep => {
                    if let Some(m) = machine.as_mut() {
                        if m.backstep() {
                            send_state(&events, m);
                        }
                    }
                }
                Cmd::Reset => {
                    if let Some(m) = machine.as_mut() {
                        m.reset();
                        running = false;
                        send_state(&events, m);
                    }
                }
                Cmd::SetBreakpoint { addr, on } => {
                    if let Some(m) = machine.as_mut() {
                        m.set_breakpoint(addr, on);
                    }
                }
                Cmd::ReadMemory { addr, len } => {
                    if let Some(m) = machine.as_ref() {
                        let mut bytes = vec![0u8; len as usize];
                        // Unmapped ranges read as zeros, matching memory
                        // semantics; no error for view refreshes.
                        m.peek_bytes(addr, &mut bytes).ok();
                        events.send(Evt::Memory { base: addr, bytes }).ok();
                    }
                }
            }
            continue;
        }

        if running {
            let Some(m) = machine.as_mut() else { return };
            let mut halted_evt = None;
            for _ in 0..RUN_CHUNK {
                if m.is_terminated() {
                    break;
                }
                let outcome = m.step();
                for evt in outcome.events {
                    if let rvm::Event::Halted(h) = evt {
                        halted_evt = Some(h);
                    }
                }
            }
            send_state(&events, m);
            if let Some(halt) = halted_evt {
                events.send(Evt::Halted { halt, pc: m.pc(), instret: m.instret() }).ok();
                running = false;
            }
        }
    }
}

fn step_once(m: &mut Machine, events: &Sender<Evt>) {
    let pc_before = m.pc();
    let statement = m.program().statement_at(pc_before).cloned();
    let outcome = m.step();
    if outcome.executed {
        let (text, line) = statement
            .map(|s| (s.basic_text.to_string(), s.source.line))
            .unwrap_or_else(|| ("<unknown>".into(), 0));
        events
            .send(Evt::Stepped {
                text,
                line,
                changes: outcome.changes,
                pc: m.pc(),
                instret: m.instret(),
            })
            .ok();
    }
    for evt in outcome.events {
        if let rvm::Event::Halted(halt) = evt {
            events.send(Evt::Halted { halt, pc: m.pc(), instret: m.instret() }).ok();
        }
    }
}

fn send_state(events: &Sender<Evt>, m: &Machine) {
    let mut regs = [0u64; 32];
    let mut fregs = [0u64; 32];
    for (i, r) in regs.iter_mut().enumerate() {
        *r = m.reg(i);
    }
    for (i, r) in fregs.iter_mut().enumerate() {
        *r = m.freg(i);
    }
    events
        .send(Evt::State(Box::new(StateSnapshot { regs, fregs, pc: m.pc(), instret: m.instret() })))
        .ok();
}
