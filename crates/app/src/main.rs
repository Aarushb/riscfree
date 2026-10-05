//! AsAccess GUI: the wxDragon front end wired to the rvasm/rvm core.
//!
//! Threading model (docs/TECH-SPEC.md section 8): the UI thread owns all
//! widgets; a simulation thread owns the Machine exclusively. They talk over
//! command/event channels, and a wx timer drains the event queue at ~30 Hz so
//! long runs never block the interface. Dynamic announcements go through the
//! narration and speech crates; structural accessibility is the native
//! widgets themselves.

mod bridge;
mod tools;
mod tools_decode;
mod tools_float;
mod tools_lab;
mod tools_timer;

#[cfg(target_os = "windows")]
use wxdragon::accessible::AccRole;
use wxdragon::prelude::*;

use narration::Verbosity;
use rvm::Halt;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::Sender;

use crate::bridge::{start_sim_thread, Cmd, Evt, InputChannel};

// Menu and control ids.
const ID_NEW: Id = 1001;
const ID_OPEN: Id = 1002;
const ID_SAVE: Id = 1003;
const ID_SETTINGS: Id = 1004;
const ID_EXIT: Id = 1005;
const ID_RECONNECT_SPEECH: Id = 1006;
const ID_RUN_ASSEMBLE: Id = 2001;
const ID_RUN_RUN: Id = 2002;
const ID_RUN_STEP: Id = 2003;
const ID_RUN_BACKSTEP: Id = 2004;
const ID_RUN_PAUSE: Id = 2005;
const ID_RUN_STOP: Id = 2006;
const ID_RUN_RESET: Id = 2007;
const ID_RUN_TOGGLE_BREAK: Id = 2009;
const ID_RUN_TOGGLE_WATCH: Id = 2010;
const ID_SHORTCUTS: Id = 3001;
const ID_TOOL_BITMAP: Id = 4001;
const ID_TOOL_FLOAT: Id = 4002;
const ID_TOOL_LAB: Id = 4003;
const ID_TOOL_COUNTER: Id = 4004;
const ID_TOOL_TIMER: Id = 4005;
const ID_TOOL_DECODE: Id = 4006;
const ID_ABOUT: Id = 3002;

const SAMPLE_RISCV: &str = "\
# add.s - add two numbers and print the sum
    .text
    .globl main
main:
    li   a0, 5        # load immediate 5
    li   a1, 7        # load immediate 7
    add  a2, a0, a1   # a2 = a0 + a1
    mv   a0, a2
    li   a7, 1        # PrintInt
    ecall
    li   a7, 10       # Exit
    ecall
";

/// Everything the run controls and dialogs share on the UI thread.
struct Shared {
    verbosity: RefCell<Verbosity>,
    program_path: RefCell<Option<String>>,
    assembled: RefCell<Option<rvasm::Program>>,
    /// Source position of each row of the assembler messages list, for
    /// Enter-to-jump.
    diagnostic_spans: RefCell<Vec<rvasm::SourcePos>>,
    /// Base address the Memory tab is viewing.
    memory_base: RefCell<u32>,
    /// RV64 mode for assemble and execution (default RV32, like RARS).
    rv64: RefCell<bool>,
    /// Memcheck: halt on reads of uninitialized memory (opt-in).
    memcheck: RefCell<bool>,
    /// Calling-convention checker: pause when a return violates the
    /// preserved-register and stack-pointer rules (opt-in).
    check_cconv: RefCell<bool>,
    /// Assemble c.* compressed instructions (opt-in; RARS has no equivalent).
    allow_compressed: RefCell<bool>,
    /// When the last output-arrival announcement spoke. One print syscall
    /// per chunk means a greeting like "Hello, name" lands as three Output
    /// events; the announcement fires once per burst, not per chunk.
    last_output_speak: RefCell<Option<std::time::Instant>>,
}

/// Every widget the behavior code needs, kept by handle. wxDragon handles are
/// Copy, so these clone freely into event closures.
#[derive(Clone)]
struct Widgets {
    frame: Frame,
    status_bar: StatusBar,
    editor: StyledTextCtrl,
    register_list: ListCtrl,
    fp_list: ListCtrl,
    program_list: ListCtrl,
    memory_list: ListCtrl,
    memory_addr: TextCtrl,
    io_output: TextCtrl,
    io_input: TextCtrl,
    messages: ListCtrl,
}

/// Speech dispatch. Created once; the Settings menu reconnects on demand so a
/// screen reader started after the app is picked up.
struct Narrator {
    /// Initialized on first use so a slow screen reader bridge never delays
    /// application startup.
    speaker: RefCell<Option<Box<dyn speech::Speaker>>>,
}

impl Narrator {
    fn new() -> Self {
        Narrator {
            speaker: RefCell::new(None),
        }
    }

    fn with_speaker(&self, f: impl FnOnce(&mut dyn speech::Speaker)) {
        let needs_init = {
            let current = self.speaker.borrow();
            !current.as_ref().is_some_and(|s| s.is_connected())
        };
        if needs_init {
            self.speaker.replace(Some(speech::best_speaker()));
        }
        if let Some(speaker) = self.speaker.borrow_mut().as_mut() {
            f(speaker.as_mut());
        }
    }

    fn reconnect(&self) {
        self.speaker.replace(Some(speech::best_speaker()));
    }

    fn speak(&self, text: Option<String>) {
        if let Some(text) = text {
            self.with_speaker(|speaker| speaker.speak(&text, false));
        }
    }

    fn backend_summary(&self) -> String {
        self.with_speaker(|_| {});
        let current = self.speaker.borrow();
        match current.as_ref() {
            Some(speaker) if speaker.is_connected() => {
                format!("Speech connected via {}", speaker.backend_name())
            }
            _ => "No speech backend found".to_string(),
        }
    }
}

fn main() {
    SystemOptions::set_option_by_int("msw.no-manifest-check", 1);

    let _ = wxdragon::main(|_| {
        let shared = Rc::new(Shared {
            verbosity: RefCell::new(Verbosity::Brief),
            program_path: RefCell::new(None),
            assembled: RefCell::new(None),
            diagnostic_spans: RefCell::new(Vec::new()),
            memory_base: RefCell::new(0x1001_0000),
            rv64: RefCell::new(false),
            memcheck: RefCell::new(false),
            check_cconv: RefCell::new(false),
            allow_compressed: RefCell::new(false),
            last_output_speak: RefCell::new(None),
        });
        let narrator = Rc::new(Narrator::new());

        let frame = Frame::builder()
            .with_title("AsAccess - RISC-V Assembly IDE")
            .with_size(Size::new(1200, 800))
            .build();
        frame.set_accessibility_label("AsAccess main window");
        #[cfg(target_os = "windows")]
        frame.set_accessibility_role(AccRole::Application);

        let status_bar = StatusBar::builder(&frame)
            .with_fields_count(2)
            .with_status_widths(vec![-1, 320])
            .add_initial_text(0, "Ready")
            .add_initial_text(1, "No program loaded")
            .build();
        status_bar.set_accessibility_label("Status bar");
        frame.set_existing_status_bar(Some(&status_bar));

        frame.set_menu_bar(build_menu_bar());

        // Simulation thread and channels.
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Cmd>();
        let (evt_tx, evt_rx) = std::sync::mpsc::channel::<Evt>();
        // Every sim event wakes the idle loop, so the idle handler below
        // only runs while there is something to drain.
        let evt_tx = bridge::EventSender::new(evt_tx);
        let (input_tx, input_rx) = std::sync::mpsc::channel::<String>();
        let input: InputChannel = std::sync::Arc::new(std::sync::Mutex::new(input_rx));
        let memory_listeners: tools::MemoryListeners = Rc::new(std::cell::RefCell::new(Vec::new()));
        let counts_listeners: tools_lab::CountsListeners =
            Rc::new(std::cell::RefCell::new(Vec::new()));
        let waiting_flag: Rc<std::cell::RefCell<bool>> = Rc::new(std::cell::RefCell::new(false));
        let listener_tags = Rc::new(std::cell::RefCell::new(1u32)); // tag 0 = Memory tab
        std::thread::Builder::new()
            .name("sim".into())
            .spawn(move || start_sim_thread(cmd_rx, evt_tx, input))
            .expect("spawn simulation thread");

        let panel = Panel::builder(&frame).build();
        panel.set_accessibility_label("Main area");
        let main_sizer = BoxSizer::builder(Orientation::Vertical).build();

        // --- Run controls ---------------------------------------------------
        let button_row = BoxSizer::builder(Orientation::Horizontal).build();
        let mut run_buttons: Vec<(Button, &'static str)> = Vec::new();
        for (label, help) in [
            ("Assemble", "Assemble the current source file"),
            ("Run", "Run the assembled program"),
            ("Step", "Execute one instruction"),
            ("Backstep", "Undo one instruction"),
            ("Pause", "Pause the running program"),
            ("Stop", "Stop the running program"),
            ("Reset", "Reset the program to its initial state"),
        ] {
            let btn = Button::builder(&panel).with_label(label).build();
            btn.set_accessibility_label(label);
            btn.set_accessibility_description(help);
            button_row.add(&btn, 0, SizerFlag::All, 4);
            run_buttons.push((btn, label));
        }
        main_sizer.add_sizer(&button_row, 0, SizerFlag::Expand, 0);

        // --- Splitters: editor | registers over output -----------------------
        let h_splitter = SplitterWindow::builder(&panel)
            .with_style(SplitterWindowStyle::LiveUpdate | SplitterWindowStyle::Default)
            .build();
        h_splitter.set_accessibility_label("Editor and state panes");

        let editor_panel = Panel::builder(&h_splitter).build();
        editor_panel.set_accessibility_label("Editor pane");
        let editor_sizer = BoxSizer::builder(Orientation::Vertical).build();
        let editor = build_editor(&editor_panel);
        editor_sizer.add(&editor, 1, SizerFlag::Expand | SizerFlag::All, 2);
        editor_panel.set_sizer(editor_sizer, true);

        let right_splitter = SplitterWindow::builder(&h_splitter)
            .with_style(SplitterWindowStyle::LiveUpdate | SplitterWindowStyle::Default)
            .build();
        right_splitter.set_accessibility_label("Right side panes");

        let (
            register_notebook,
            register_list,
            fp_list,
            program_list,
            memory_list,
            memory_addr,
            memory_go,
        ) = build_state_views(&right_splitter);
        let (bottom_notebook, io_output, io_input, send_input, messages) =
            build_bottom_views(&right_splitter);

        let _ = right_splitter.split_horizontally(&register_notebook, &bottom_notebook, 380);
        let _ = h_splitter.split_vertically(&editor_panel, &right_splitter, 620);

        // Deterministic editor tab exits: forward to the state notebook,
        // backward to the Reset button (the last control before the editor).
        if let Some((reset_btn, _)) = run_buttons.last() {
            bind_editor_tab_keys(&editor, &register_notebook, reset_btn);
        }

        main_sizer.add(&h_splitter, 1, SizerFlag::Expand, 0);
        panel.set_sizer(main_sizer, true);

        let widgets = Widgets {
            frame,
            status_bar,
            editor,
            register_list,
            fp_list,
            program_list,
            memory_list,
            memory_addr,
            io_output,
            io_input,
            messages,
        };

        // --- Run controls -> actions -----------------------------------------
        for (btn, label) in run_buttons {
            match label {
                "Assemble" => {
                    let w = widgets.clone();
                    let sh = shared.clone();
                    let nar = narrator.clone();
                    let tx = cmd_tx.clone();
                    btn.on_click(move |_| do_assemble(&w, &sh, &nar, &tx));
                }
                "Run" => {
                    let sh = shared.clone();
                    let nar = narrator.clone();
                    let tx = cmd_tx.clone();
                    let sb = widgets.status_bar;
                    btn.on_click(move |_| {
                        if tx.send(Cmd::Run).is_err() {
                            sb.set_status_text("Simulation thread is not responding", 0);
                            return;
                        }
                        nar.speak(narration::run_started(*sh.verbosity.borrow()));
                    });
                }
                "Step" => {
                    let tx = cmd_tx.clone();
                    btn.on_click(move |_| {
                        tx.send(Cmd::Step).ok();
                    });
                }
                "Backstep" => {
                    let tx = cmd_tx.clone();
                    btn.on_click(move |_| {
                        tx.send(Cmd::Backstep).ok();
                    });
                }
                "Pause" | "Stop" => {
                    let tx = cmd_tx.clone();
                    btn.on_click(move |_| {
                        tx.send(Cmd::Pause).ok();
                    });
                }
                "Reset" => {
                    let tx = cmd_tx.clone();
                    btn.on_click(move |_| {
                        tx.send(Cmd::Reset).ok();
                    });
                }
                _ => {}
            }
        }

        // Memory tab: Go requests the window from the simulation thread.
        {
            let w = widgets.clone();
            let sh = shared.clone();
            let tx = cmd_tx.clone();
            memory_go.on_click(move |_| {
                let text = w
                    .memory_addr
                    .get_value()
                    .trim()
                    .trim_start_matches("0x")
                    .to_string();
                if let Ok(addr) = u32::from_str_radix(&text, 16) {
                    *sh.memory_base.borrow_mut() = addr;
                    tx.send(Cmd::ReadMemory {
                        addr,
                        len: 512,
                        tag: 0,
                    })
                    .ok();
                } else {
                    w.status_bar
                        .set_status_text("Memory address must be hexadecimal", 0);
                }
            });
        }

        // Program list: Enter toggles a breakpoint on the selected row.
        {
            let w = widgets.clone();
            let sh = shared.clone();
            let nar = narrator.clone();
            let tx = cmd_tx.clone();
            widgets.program_list.on_item_activated(move |event| {
                let _ = event;
                toggle_selected_breakpoint(&w, &sh, &nar, &tx);
            });
        }

        // Assembler messages: Enter jumps the editor to the source line.
        {
            let w = widgets.clone();
            let sh = shared.clone();
            widgets.messages.on_item_activated(move |event| {
                let row = event.get_item_index();
                if let Some(pos) = sh.diagnostic_spans.borrow().get(row as usize) {
                    w.editor.goto_line(pos.line as i32 - 1);
                    w.editor.set_focus();
                }
            });
        }

        // Program input: Send forwards one line to the machine.
        {
            let input_ctrl = widgets.io_input;
            send_input.on_click(move |_| {
                let line = input_ctrl.get_value();
                input_ctrl.set_value("");
                if !line.is_empty() {
                    input_tx.send(line).ok();
                }
            });
        }

        bind_menu_events(
            &widgets,
            &shared,
            &narrator,
            &cmd_tx,
            &memory_listeners,
            &counts_listeners,
            &listener_tags,
            &waiting_flag,
        );

        // --- Event pump: simulation events -> views and narrator -------------
        // Driven by idle events: wxDragon's frame-owned wxTimer binding does
        // not deliver ticks (verified in the Phase 1 GUI build), while idle
        // events fire whenever the loop is empty. The small sleep inside the
        // handler throttles the request-more cycle; the drain itself never
        // blocks.
        {
            let w = widgets.clone();
            let sh = shared.clone();
            let nar = narrator.clone();
            let tx = cmd_tx.clone();
            let listeners = memory_listeners.clone();
            let counts_listeners = counts_listeners.clone();
            frame.on_idle(move |idle| {
                if let WindowEventData::Idle(idle) = idle {
                    // Stay hot only while events keep arriving; each post
                    // also calls wake_up_idle from the sim thread, so this
                    // handler runs exactly when there is something to do.
                    let mut got_any = false;
                    while let Ok(evt) = evt_rx.try_recv() {
                        got_any = true;
                        match evt {
                            Evt::Counts { counts, tag } => {
                                for (listener_tag, listener) in counts_listeners.borrow().iter() {
                                    if *listener_tag == tag {
                                        listener(&counts);
                                    }
                                }
                            }
                            other => {
                                if !tools::route_memory_event(&listeners, &other) {
                                    handle_sim_event(&w, &sh, &nar, &tx, other);
                                }
                            }
                        }
                    }
                    idle.request_more(got_any);
                }
            });
        }

        widgets.frame.show(true);
        widgets.frame.centre();
    });
}

/// Refresh the Floating Point tab rows from a snapshot.
fn refresh_fp_registers(w: &Widgets, fregs: &[u64; 32]) {
    FP_ROWS.with(|rows| {
        let mut rows = rows.borrow_mut();
        for (i, row) in rows.iter_mut().enumerate() {
            let bits = fregs[i];
            row[2] = match rvm::f32_of_freg(bits) {
                f if f.is_nan() => "NaN".to_string(),
                f => format!("{f}"),
            };
            row[3] = match rvm::f64_of_freg(bits) {
                f if f.is_nan() => "NaN".to_string(),
                f => format!("{f}"),
            };
            row[4] = format!("0x{bits:016x}");
        }
    });
    w.fp_list.refresh_items(0, 31);
}

/// Rebuild the Memory tab rows for one 16-byte-per-row window.
fn rebuild_memory_rows(base: u32, bytes: &[u8]) {
    MEMORY_ROWS.with(|rows| {
        let mut rows = rows.borrow_mut();
        rows.clear();
        for (chunk, offset) in bytes.chunks(16).zip((0u32..).step_by(16)) {
            let hex = chunk
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ");
            let ascii: String = chunk
                .iter()
                .map(|b| {
                    if (0x20..0x7f).contains(b) {
                        *b as char
                    } else {
                        '.'
                    }
                })
                .collect();
            rows.push([format!("0x{:08x}", base + offset), hex, ascii]);
        }
    });
}

/// Rebuild the Program tab rows from an assembled program.
fn load_program_rows(program: &rvasm::Program) {
    PROGRAM_ROWS.with(|rows| {
        let mut rows = rows.borrow_mut();
        rows.clear();
        for s in &program.statements {
            rows.push([
                format!("0x{:08x}", s.addr),
                format!("0x{:08x}", s.encoding),
                s.basic_text.to_string(),
                String::new(),
                String::new(),
            ]);
        }
    });
    PROGRAM_ADDRS.with(|addrs| {
        *addrs.borrow_mut() = program.statements.iter().map(|s| s.addr).collect();
    });
}

/// Move the PC marker on the Program tab to the row at `pc`.
fn mark_program_pc(w: &Widgets, pc: u32) {
    let current = PROGRAM_ADDRS.with(|addrs| {
        addrs
            .borrow()
            .iter()
            .position(|a| *a == pc)
            .map(|i| i as i64)
    });
    PROGRAM_ROWS.with(|rows| {
        let mut rows = rows.borrow_mut();
        for row in rows.iter_mut() {
            row[4] = String::new();
        }
        if let Some(i) = current {
            if let Some(row) = rows.get_mut(i as usize) {
                row[4] = "PC".to_string();
            }
        }
    });
    let count = PROGRAM_ROWS.with(|rows| rows.borrow().len() as i64);
    if count > 0 {
        w.program_list.refresh_items(0, count - 1);
        if let Some(i) = current {
            w.program_list.ensure_visible(i);
        }
    }
}

/// Toggle the breakpoint on the selected Program row.
/// Start a run and say so: programs that block on console input produce
/// neither output nor a halt for a while, and a silent run key sounds
/// identical to a dead one.
fn run_program(shared: &Shared, narrator: &Narrator, tx: &Sender<bridge::Cmd>) {
    tx.send(bridge::Cmd::Run).ok();
    narrator.speak(narration::run_started(*shared.verbosity.borrow()));
}

fn toggle_selected_breakpoint(
    w: &Widgets,
    shared: &Shared,
    narrator: &Narrator,
    cmd_tx: &Sender<Cmd>,
) {
    let mut index = w.program_list.get_first_selected_item();
    if index < 0 {
        // No selection: fall back to the row at the current PC, else the
        // first instruction, which is what a keyboard user means by
        // "current line" right after assembling.
        index = PROGRAM_ROWS.with(|rows| {
            rows.borrow()
                .iter()
                .position(|row| row[4] == "PC")
                .map(|i| i as i32)
                .unwrap_or(if rows.borrow().is_empty() { -1 } else { 0 })
        });
    }
    if index < 0 {
        return;
    }
    let addr_text = w.program_list.get_item_text(index as i64, 0);
    let Ok(addr) = u32::from_str_radix(addr_text.trim_start_matches("0x"), 16) else {
        return;
    };
    let on = w.program_list.get_item_text(index as i64, 3) != "on";
    PROGRAM_ROWS.with(|rows| {
        if let Some(row) = rows.borrow_mut().get_mut(index as usize) {
            row[3] = if on { "on".to_string() } else { String::new() };
        }
    });
    w.program_list.refresh_items(index as i64, index as i64);
    w.status_bar.set_status_text(
        &if on {
            format!("Breakpoint set at 0x{addr:08x}")
        } else {
            format!("Breakpoint cleared at 0x{addr:08x}")
        },
        0,
    );
    // The row shows the new state, but say it: walking back to the row and
    // reading the Breakpoint column is exactly the trip speech avoids.
    let line = shared
        .assembled
        .borrow()
        .as_ref()
        .and_then(|p| p.statement_at(addr))
        .map(|s| s.source.line)
        .unwrap_or(0);
    narrator.speak(narration::breakpoint_changed(
        *shared.verbosity.borrow(),
        on,
        line,
    ));
    cmd_tx.send(Cmd::SetBreakpoint { addr, on }).ok();
}

/// Toggle a write watchpoint on the selected Program row's address.
fn toggle_selected_watchpoint(w: &Widgets, cmd_tx: &Sender<bridge::Cmd>) {
    let mut index = w.program_list.get_first_selected_item();
    if index < 0 {
        index = PROGRAM_ROWS.with(|rows| {
            rows.borrow()
                .iter()
                .position(|row| row[4] == "PC")
                .map(|i| i as i32)
                .unwrap_or(if rows.borrow().is_empty() { -1 } else { 0 })
        });
    }
    if index < 0 {
        return;
    }
    let addr_text = w.program_list.get_item_text(index as i64, 0);
    let Ok(addr) = u32::from_str_radix(addr_text.trim_start_matches("0x"), 16) else {
        return;
    };
    PROGRAM_ROWS.with(|rows| {
        if let Some(row) = rows.borrow_mut().get_mut(index as usize) {
            let on = row[3].is_empty();
            // The Breakpoint column doubles as the marker column; a row can
            // hold both a breakpoint and a watchpoint marker, so keep them
            // distinguishable.
            row[3] = if on {
                "watch".to_string()
            } else {
                String::new()
            };
        }
    });
    w.program_list.refresh_items(index as i64, index as i64);
    cmd_tx.send(bridge::Cmd::ToggleMemWatchpoint { addr }).ok();
}

fn handle_sim_event(
    w: &Widgets,
    shared: &Shared,
    narrator: &Narrator,
    tx: &Sender<bridge::Cmd>,
    evt: Evt,
) {
    match evt {
        Evt::Output(text) => {
            // RARS clear-display (ASCII 12): truncate the transcript at the
            // last form feed, then cap so a chatty program cannot grow it
            // forever.
            let text = match text.rfind('') {
                Some(pos) => text[pos + 1..].to_string(),
                None => text,
            };
            if w.io_output.get_value().len() + text.len() > 1_000_000 {
                w.io_output.set_value("");
            }
            w.io_output.append_text(&text);
            const BURST: std::time::Duration = std::time::Duration::from_millis(500);
            let due = {
                let last = shared.last_output_speak.borrow();
                last.is_none_or(|t| t.elapsed() >= BURST)
            };
            if due {
                *shared.last_output_speak.borrow_mut() = Some(std::time::Instant::now());
                narrator.speak(narration::output_arrived(
                    *shared.verbosity.borrow(),
                    text.lines().count(),
                ));
            }
        }
        Evt::State(snapshot) => {
            let bridge::StateSnapshot {
                regs,
                fregs,
                displays,
                pc,
                instret,
            } = *snapshot;
            DISPLAYS.with(|d| *d.borrow_mut() = displays);
            refresh_registers(w, &regs);
            refresh_fp_registers(w, &fregs);
            mark_program_pc(w, pc);
            w.status_bar
                .set_status_text(&format!("pc 0x{pc:08x}, {instret} executed"), 1);
            let base = *shared.memory_base.borrow();
            tx.send(Cmd::ReadMemory {
                addr: base,
                len: 512,
                tag: 0,
            })
            .ok();
        }
        Evt::Memory { base, bytes, tag } => {
            // Tag 0 is the Memory tab; other tags route to their tools in
            // the pump before this handler runs.
            if tag == 0 {
                rebuild_memory_rows(base, &bytes);
                let count = MEMORY_ROWS.with(|rows| rows.borrow().len() as i64);
                w.memory_list.set_item_count(count);
                if count > 0 {
                    w.memory_list.refresh_items(0, count - 1);
                }
            }
        }
        Evt::Counts { .. } => {
            // Routed to the Instruction Counter tool by the pump.
        }
        Evt::Stepped {
            text,
            line,
            changes,
            pc,
            instret,
        } => {
            w.status_bar.set_status_text(
                &format!("line {line}, pc 0x{pc:08x}, {instret} executed"),
                1,
            );
            narrator.speak(narration::step_done(
                *shared.verbosity.borrow(),
                &text,
                &changes,
            ));
        }
        Evt::Backstepped { text, pc, instret } => {
            w.status_bar.set_status_text(
                &format!("line {}, pc 0x{pc:08x}, {instret} executed", {
                    let line = shared
                        .assembled
                        .borrow()
                        .as_ref()
                        .and_then(|p| p.statement_at(pc))
                        .map(|s| s.source.line);
                    line.unwrap_or(0)
                }),
                1,
            );
            narrator.speak(narration::backstep_done(
                *shared.verbosity.borrow(),
                &text,
                &[],
            ));
        }
        Evt::Halted { halt, pc, instret } => {
            let location = shared
                .assembled
                .borrow()
                .as_ref()
                .and_then(|p| p.statement_at(pc))
                .map(|s| format!("line {}", s.source.line));
            match &halt {
                Halt::Breakpoint | Halt::Ebreak => {
                    w.status_bar.set_status_text(
                        &format!(
                            "Stopped, {}",
                            location.as_deref().unwrap_or("pc outside program")
                        ),
                        0,
                    );
                }
                Halt::Exit { code } => {
                    w.status_bar
                        .set_status_text(&format!("Program finished with code {code}"), 0);
                }
                _ => {}
            }
            narrator.speak(narration::halted(
                *shared.verbosity.borrow(),
                &halt,
                instret,
                location.as_deref(),
            ));
        }
        Evt::Loaded { pc, instret } => {
            refresh_registers_zeroed(w);
            w.status_bar.set_status_text(
                &format!("Program loaded, pc 0x{pc:08x}, {instret} executed"),
                0,
            );
        }
    }
}

fn refresh_registers(w: &Widgets, regs: &[u64; 32]) {
    // The register list is virtual; rewriting the backing store through the
    // shared callback closure is done by regenerating rows here. Because the
    // callback closure captured its own Rc, updates go through the same data
    // by re-querying: simplest correct approach is rewriting via the list's
    // virtual callback inputs stored in a thread-local registry (see
    // build_register_views).
    REGISTERS.with(|r| {
        let mut rows = r.borrow_mut();
        for (i, row) in rows.iter_mut().enumerate() {
            row[2] = format_reg(regs[i]);
        }
    });
    w.register_list.refresh_items(0, 31);
}

fn refresh_registers_zeroed(w: &Widgets) {
    REGISTERS.with(|r| {
        let mut rows = r.borrow_mut();
        for (i, row) in rows.iter_mut().enumerate() {
            row[2] = "0".to_string();
            let _ = i;
        }
    });
    w.register_list.refresh_items(0, 31);
}

thread_local! {
    /// Backing rows for the virtual register list. A thread-local keeps the
    /// virtual text callback and the event pump sharing one allocation
    /// without fighting the widget handle lifetime.
    static REGISTERS: RefCell<Vec<[String; 4]>> = RefCell::new(
        (0..32)
            .map(|i| [format!("x{i}"), reg_name(i).to_string(), "0".to_string(), String::new()])
            .collect(),
    );
    /// Backing rows for the Program tab: address, machine code, source text,
    /// breakpoint marker, PC marker.
    static PROGRAM_ROWS: RefCell<Vec<[String; 5]>> = const { RefCell::new(Vec::new()) };
    /// Addresses parallel to PROGRAM_ROWS.
    static PROGRAM_ADDRS: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
    /// Backing rows for the Memory tab: address, hex bytes, ASCII.
    static MEMORY_ROWS: RefCell<Vec<[String; 3]>> = const { RefCell::new(Vec::new()) };
    /// Backing rows for the Floating Point tab.
    static FP_ROWS: RefCell<Vec<[String; 5]>> = const { RefCell::new(Vec::new()) };
    /// Latest seven-segment display register bytes, for the Digital Lab Sim
    /// tool's display refresh.
    static DISPLAYS: RefCell<[u8; 2]> = const { RefCell::new([0; 2]) };
}

const ABI: &[&str] = &[
    "zero", "ra", "sp", "gp", "tp", "t0", "t1", "t2", "s0", "s1", "a0", "a1", "a2", "a3", "a4",
    "a5", "a6", "a7", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11", "t3", "t4",
    "t5", "t6",
];

fn reg_name(index: usize) -> &'static str {
    ABI.get(index).copied().unwrap_or("??")
}

fn format_reg(v: u64) -> String {
    let as_i32 = v as u32 as i32;
    if (-9_999..=9_999).contains(&as_i32) {
        format!("{as_i32}")
    } else {
        format!("{as_i32} (0x{:08x})", v as u32)
    }
}

// --- Widget builders -------------------------------------------------------

fn build_menu_bar() -> MenuBar {
    let file_menu = Menu::builder()
        .append_item(ID_NEW, "&New\tCtrl+N", "New source file")
        .append_item(ID_OPEN, "&Open...\tCtrl+O", "Open a source file")
        .append_item(ID_SAVE, "&Save\tCtrl+S", "Save the current source file")
        .append_separator()
        .append_item(ID_SETTINGS, "S&ettings...", "Open the settings dialog")
        .append_item(
            ID_RECONNECT_SPEECH,
            "&Reconnect screen reader speech",
            "Reconnect the speech bridge after starting a screen reader",
        )
        .append_separator()
        .append_item(ID_EXIT, "E&xit\tAlt+F4", "Exit AsAccess")
        .build();

    // F-keys follow RARS so course muscle memory keeps working:
    // F3 assemble, F5 run, F7 step, F8 backstep, F9 pause, F11 stop, F12 reset.
    let run_menu = Menu::builder()
        .append_item(
            ID_RUN_ASSEMBLE,
            "&Assemble\tF3",
            "Assemble the current source file",
        )
        .append_item(ID_RUN_RUN, "&Run\tF5", "Run the assembled program")
        .append_item(ID_RUN_PAUSE, "Paus&e\tF9", "Pause the running program")
        .append_separator()
        .append_item(ID_RUN_STEP, "St&ep\tF7", "Execute one instruction")
        .append_item(ID_RUN_BACKSTEP, "&Backstep\tF8", "Undo one instruction")
        .append_separator()
        .append_item(ID_RUN_STOP, "Sto&p\tF11", "Stop the running program")
        .append_item(
            ID_RUN_RESET,
            "R&eset\tF12",
            "Reset the program to its initial state",
        )
        .build();

    let tools_menu = Menu::builder()
        .append_item(
            ID_TOOL_BITMAP,
            "&Bitmap Display",
            "Watch memory as a pixel grid, with a textual view of every row",
        )
        .append_item(
            ID_TOOL_FLOAT,
            "Float &Representation",
            "Convert between raw bits and float values",
        )
        .append_item(
            ID_TOOL_LAB,
            "&Digital Lab Sim",
            "Seven segment displays and a hex keypad your program controls",
        )
        .append_item(
            ID_TOOL_COUNTER,
            "Instruction &Counter",
            "Executed instruction counts grouped by opcode",
        )
        .append_item(
            ID_TOOL_TIMER,
            "&Timer Tool",
            "Arm periodic timer interrupts for handler coursework",
        )
        .append_item(
            ID_TOOL_DECODE,
            "Instruction &Decode",
            "Break an instruction word into its fields",
        )
        .build();

    let help_menu = Menu::builder()
        .append_item(
            ID_SHORTCUTS,
            "&Keyboard Shortcuts\tF1",
            "Show keyboard shortcuts",
        )
        .append_separator()
        .append_item(ID_ABOUT, "&About AsAccess", "About this application")
        .build();

    MenuBar::builder()
        .append(file_menu, "&File")
        .append(run_menu, "&Run")
        .append(tools_menu, "&Tools")
        .append(help_menu, "&Help")
        .build()
}

/// Key handling bound on the editor's own event handler. StyledTextCtrl does
/// not implement wxDragon's WindowEvents, so this newtype bridges it.
struct EditorKeys(StyledTextCtrl);

impl WxEvtHandler for EditorKeys {
    unsafe fn get_event_handler_ptr(&self) -> *mut wxdragon::ffi::wxd_EvtHandler_t {
        self.0.get_event_handler_ptr()
    }
}

impl WindowEvents for EditorKeys {}

/// Tab and Shift+Tab move focus out of the editor; Ctrl+Tab inserts the tab
/// character. The landing spots are explicit — the state notebook forward,
/// the Reset button backward — because generic wx navigation started from a
/// Scintilla inside the splitter hierarchy resolves its search point
/// inconsistently and lands on seemingly random controls.
fn bind_editor_tab_keys(editor: &StyledTextCtrl, next: &Notebook, prev: &Button) {
    let handle = *editor;
    let next = *next;
    let prev = *prev;
    EditorKeys(handle).on_key_down(move |data| {
        if let WindowEventData::Keyboard(kb) = &data {
            if kb.get_key_code() == Some(WXK_TAB) {
                if kb.control_down() {
                    let pos = handle.get_current_pos();
                    handle.insert_text(pos, "\t");
                } else if kb.shift_down() {
                    prev.set_focus();
                } else {
                    next.set_focus();
                }
                // Handled here: do not skip, or Scintilla inserts a tab too.
                return;
            }
        }
        data.skip(true);
    });
}

fn build_editor(parent: &Panel) -> StyledTextCtrl {
    let editor = StyledTextCtrl::builder(parent).build();
    editor.set_text(SAMPLE_RISCV);
    editor.set_margin_type(1, MarginType::Number);
    editor.set_margin_width(1, 40);
    editor.set_read_only(false);
    editor.set_accessibility_label("Source editor");
    editor.set_accessibility_description("RISC-V assembly source editor");
    #[cfg(target_os = "windows")]
    editor.set_accessibility_role(AccRole::Document);
    editor
}

fn build_state_views(
    parent: &SplitterWindow,
) -> (
    Notebook,
    ListCtrl,
    ListCtrl,
    ListCtrl,
    ListCtrl,
    TextCtrl,
    Button,
) {
    let notebook = Notebook::builder(parent).build();
    notebook.set_accessibility_label("State views");
    #[cfg(target_os = "windows")]
    notebook.set_accessibility_role(AccRole::PageTabList);

    // Registers: a virtual report list. wxGrid measured invisible to UIA
    // clients in the spike, so the list is the register surface.
    let list_panel = Panel::builder(&notebook).build();
    list_panel.set_accessibility_label("Registers pane");
    let list = ListCtrl::builder(&list_panel)
        .with_style(ListCtrlStyle::Report | ListCtrlStyle::Virtual | ListCtrlStyle::SingleSel)
        .build();
    list.insert_column(0, "Reg", ListColumnFormat::Left, 60);
    list.insert_column(1, "Name", ListColumnFormat::Left, 90);
    list.insert_column(2, "Value", ListColumnFormat::Left, 200);
    list.set_item_count(32);
    assert!(list.set_virtual_text_callback(move |item, col| {
        REGISTERS.with(|rows| {
            rows.borrow()
                .get(item as usize)
                .and_then(|row| row.get(col as usize))
                .cloned()
                .unwrap_or_default()
        })
    }));
    list.set_accessibility_label("Register values");
    list.set_accessibility_description("All thirty-two integer registers with current values");
    #[cfg(target_os = "windows")]
    list.set_accessibility_role(AccRole::List);
    let list_sizer = BoxSizer::builder(Orientation::Vertical).build();
    list_sizer.add(&list, 1, SizerFlag::Expand | SizerFlag::All, 2);
    list_panel.set_sizer(list_sizer, true);
    notebook.add_page(&list_panel, "Registers", true, None);

    // Program tab: the assembled instructions with breakpoints. Virtual list;
    // rows come from the PROGRAM_ROWS registry after each assemble.
    let prog_panel = Panel::builder(&notebook).build();
    prog_panel.set_accessibility_label("Program pane");
    let prog_sizer = BoxSizer::builder(Orientation::Vertical).build();
    let prog_list = ListCtrl::builder(&prog_panel)
        .with_style(ListCtrlStyle::Report | ListCtrlStyle::Virtual | ListCtrlStyle::SingleSel)
        .build();
    prog_list.insert_column(0, "Address", ListColumnFormat::Left, 90);
    prog_list.insert_column(1, "Machine", ListColumnFormat::Left, 100);
    prog_list.insert_column(2, "Code", ListColumnFormat::Left, 260);
    prog_list.insert_column(3, "Breakpoint", ListColumnFormat::Left, 90);
    prog_list.insert_column(4, "Current", ListColumnFormat::Left, 70);
    prog_list.set_item_count(0);
    assert!(prog_list.set_virtual_text_callback(move |item, col| {
        PROGRAM_ROWS.with(|rows| {
            rows.borrow()
                .get(item as usize)
                .and_then(|row| row.get(col as usize))
                .cloned()
                .unwrap_or_default()
        })
    }));
    prog_list.set_accessibility_label("Program instructions");
    prog_list.set_accessibility_description(
        "Assembled instructions; toggle a breakpoint with Enter or the Toggle Breakpoint action",
    );
    #[cfg(target_os = "windows")]
    prog_list.set_accessibility_role(AccRole::List);
    prog_sizer.add(&prog_list, 1, SizerFlag::Expand | SizerFlag::All, 2);
    prog_panel.set_sizer(prog_sizer, true);
    notebook.add_page(&prog_panel, "Program", false, None);

    // Memory tab: a window of memory as hex bytes and ASCII, with a
    // jump-to-address field. The window base defaults to static data, which
    // is where course programs put their variables.
    let mem_panel = Panel::builder(&notebook).build();
    mem_panel.set_accessibility_label("Memory pane");
    let mem_sizer = BoxSizer::builder(Orientation::Vertical).build();

    let addr_row = BoxSizer::builder(Orientation::Horizontal).build();
    let addr_label = StaticText::builder(&mem_panel)
        .with_label("Address (hex):")
        .build();
    addr_row.add(
        &addr_label,
        0,
        SizerFlag::AlignCenterVertical | SizerFlag::All,
        2,
    );
    let mem_addr = TextCtrl::builder(&mem_panel).build();
    mem_addr.set_value("10010000");
    mem_addr.set_accessibility_label("Memory view address");
    mem_addr.set_accessibility_description("Hexadecimal address the memory view starts at");
    addr_row.add(&mem_addr, 1, SizerFlag::Expand | SizerFlag::All, 2);
    let mem_go = Button::builder(&mem_panel).with_label("Go").build();
    mem_go.set_accessibility_label("Go to memory address");
    addr_row.add(&mem_go, 0, SizerFlag::All, 2);
    mem_sizer.add_sizer(&addr_row, 0, SizerFlag::Expand, 0);

    let mem_list = ListCtrl::builder(&mem_panel)
        .with_style(ListCtrlStyle::Report | ListCtrlStyle::Virtual | ListCtrlStyle::SingleSel)
        .build();
    mem_list.insert_column(0, "Address", ListColumnFormat::Left, 100);
    mem_list.insert_column(1, "Bytes", ListColumnFormat::Left, 340);
    mem_list.insert_column(2, "ASCII", ListColumnFormat::Left, 170);
    mem_list.set_item_count(0);
    assert!(mem_list.set_virtual_text_callback(move |item, col| {
        MEMORY_ROWS.with(|rows| {
            rows.borrow()
                .get(item as usize)
                .and_then(|row| row.get(col as usize))
                .cloned()
                .unwrap_or_default()
        })
    }));
    mem_list.set_accessibility_label("Memory bytes");
    mem_list
        .set_accessibility_description("Memory contents, sixteen bytes per row with ASCII text");
    #[cfg(target_os = "windows")]
    mem_list.set_accessibility_role(AccRole::List);
    mem_sizer.add(&mem_list, 1, SizerFlag::Expand | SizerFlag::All, 2);
    mem_panel.set_sizer(mem_sizer, true);
    notebook.add_page(&mem_panel, "Memory", false, None);

    // Floating point tab: f registers with float interpretation and raw
    // bits (NaN-boxing aware via the core's read rule).
    let fp_panel = Panel::builder(&notebook).build();
    fp_panel.set_accessibility_label("Floating point pane");
    let fp_sizer = BoxSizer::builder(Orientation::Vertical).build();
    let fp_list = ListCtrl::builder(&fp_panel)
        .with_style(ListCtrlStyle::Report | ListCtrlStyle::Virtual | ListCtrlStyle::SingleSel)
        .build();
    fp_list.insert_column(0, "FReg", ListColumnFormat::Left, 70);
    fp_list.insert_column(1, "Name", ListColumnFormat::Left, 90);
    fp_list.insert_column(2, "Float", ListColumnFormat::Left, 170);
    fp_list.insert_column(3, "Double", ListColumnFormat::Left, 190);
    fp_list.insert_column(4, "Bits (hex)", ListColumnFormat::Left, 130);
    fp_list.set_item_count(32);
    assert!(fp_list.set_virtual_text_callback(move |item, col| {
        FP_ROWS.with(|rows| {
            rows.borrow()
                .get(item as usize)
                .and_then(|row| row.get(col as usize))
                .cloned()
                .unwrap_or_default()
        })
    }));
    fp_list.set_accessibility_label("Floating point register values");
    fp_list.set_accessibility_description(
        "All thirty-two floating point registers as float, double, and raw bits",
    );
    #[cfg(target_os = "windows")]
    fp_list.set_accessibility_role(AccRole::List);
    fp_sizer.add(&fp_list, 1, SizerFlag::Expand | SizerFlag::All, 2);
    fp_panel.set_sizer(fp_sizer, true);
    notebook.add_page(&fp_panel, "Floating Point", false, None);

    (
        notebook, list, fp_list, prog_list, mem_list, mem_addr, mem_go,
    )
}

/// The bottom notebook: Run I/O console plus the assembler messages list.
#[allow(clippy::type_complexity)]
fn build_bottom_views(parent: &SplitterWindow) -> (Notebook, TextCtrl, TextCtrl, Button, ListCtrl) {
    let notebook = Notebook::builder(parent).build();
    notebook.set_accessibility_label("Output views");
    #[cfg(target_os = "windows")]
    notebook.set_accessibility_role(AccRole::PageTabList);

    // Tab 1: Run I/O.
    let io_panel = Panel::builder(&notebook).build();
    io_panel.set_accessibility_label("Run I O pane");
    let sizer = BoxSizer::builder(Orientation::Vertical).build();

    let io_label = StaticText::builder(&io_panel)
        .with_label("Program output (read only)")
        .build();
    sizer.add(&io_label, 0, SizerFlag::All, 2);

    let io_output = TextCtrl::builder(&io_panel)
        .with_style(TextCtrlStyle::MultiLine | TextCtrlStyle::ReadOnly)
        .build();
    io_output.set_accessibility_label("Run I O output");
    io_output.set_accessibility_description("Read only output of the running program");
    #[cfg(target_os = "windows")]
    io_output.set_accessibility_role(AccRole::StaticText);
    sizer.add(&io_output, 1, SizerFlag::Expand | SizerFlag::All, 2);

    let input_row = BoxSizer::builder(Orientation::Horizontal).build();
    let input_label = StaticText::builder(&io_panel)
        .with_label("Program input:")
        .build();
    input_row.add(
        &input_label,
        0,
        SizerFlag::AlignCenterVertical | SizerFlag::All,
        2,
    );

    let io_input = TextCtrl::builder(&io_panel).build();
    io_input.set_accessibility_label("Program input");
    io_input.set_accessibility_description("Text forwarded to the program when you choose Send");
    #[cfg(target_os = "windows")]
    io_input.set_accessibility_role(AccRole::Text);
    input_row.add(&io_input, 1, SizerFlag::Expand | SizerFlag::All, 2);

    let send_input = Button::builder(&io_panel).with_label("Send").build();
    send_input.set_accessibility_label("Send program input");
    send_input.set_accessibility_description("Forward the input line to the running program");
    input_row.add(&send_input, 0, SizerFlag::All, 2);
    sizer.add_sizer(&input_row, 0, SizerFlag::Expand, 0);

    io_panel.set_sizer(sizer, true);
    notebook.add_page(&io_panel, "Run I/O", true, None);

    // Tab 2: Assembler messages.
    let msg_panel = Panel::builder(&notebook).build();
    msg_panel.set_accessibility_label("Assembler messages pane");
    let msg_sizer = BoxSizer::builder(Orientation::Vertical).build();
    let messages = ListCtrl::builder(&msg_panel)
        .with_style(ListCtrlStyle::Report | ListCtrlStyle::SingleSel)
        .build();
    messages.insert_column(0, "Severity", ListColumnFormat::Left, 80);
    messages.insert_column(1, "Where", ListColumnFormat::Left, 150);
    messages.insert_column(2, "Message", ListColumnFormat::Left, 420);
    messages.set_accessibility_label("Assembler messages");
    messages.set_accessibility_description("Assembly errors and warnings");
    msg_sizer.add(&messages, 1, SizerFlag::Expand | SizerFlag::All, 2);
    msg_panel.set_sizer(msg_sizer, true);
    notebook.add_page(&msg_panel, "Assembler Messages", false, None);

    (notebook, io_output, io_input, send_input, messages)
}

// --- Actions ---------------------------------------------------------------

fn do_assemble(widgets: &Widgets, shared: &Shared, narrator: &Narrator, cmd_tx: &Sender<Cmd>) {
    let text = widgets.editor.get_text();
    let name = shared
        .program_path
        .borrow()
        .as_ref()
        .and_then(|p| {
            std::path::Path::new(p)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "main.s".to_string());
    let files = vec![rvasm::InputFile { name, source: text }];
    let result = rvasm::assemble(
        &files,
        &rvasm::AsmConfig {
            rv64: *shared.rv64.borrow(),
            allow_compressed: *shared.allow_compressed.borrow(),
            ..rvasm::AsmConfig::default()
        },
    );

    widgets.messages.delete_all_items();
    *shared.diagnostic_spans.borrow_mut() = result.diagnostics.iter().map(|d| d.pos).collect();
    for (i, d) in result.diagnostics.iter().enumerate() {
        let severity = if d.is_error() { "error" } else { "warning" };
        let idx = widgets.messages.insert_item(i as i64, severity, None);
        let where_text = format!(
            "{}:{}:{}",
            files[d.pos.file].name,
            d.pos.line,
            d.pos.col + 1
        );
        widgets
            .messages
            .set_item_text_by_column(idx as i64, 1, &where_text);
        widgets
            .messages
            .set_item_text_by_column(idx as i64, 2, &d.message);
    }

    let first_error = result.diagnostics.iter().find(|d| d.is_error()).map(|d| {
        format!(
            "{}:{}:{}: {}",
            files[d.pos.file].name,
            d.pos.line,
            d.pos.col + 1,
            d.message
        )
    });
    let error_count = result.diagnostics.iter().filter(|d| d.is_error()).count();
    let ok = !result.has_errors();
    let instructions = result
        .program
        .as_ref()
        .map(|p| p.statements.len())
        .unwrap_or(0);
    narrator.speak(narration::assemble_done(
        *shared.verbosity.borrow(),
        ok,
        instructions,
        error_count,
        first_error.as_deref(),
    ));

    if ok {
        if let Some(program) = result.program {
            *shared.assembled.borrow_mut() = Some(program.clone());
            load_program_rows(&program);
            widgets
                .program_list
                .set_item_count(program.statements.len() as i64);
            widgets
                .program_list
                .refresh_items(0, program.statements.len() as i64 - 1);
            widgets.status_bar.set_status_text(
                &format!(
                    "Assembled, {} instructions. Ready to run.",
                    program.statements.len()
                ),
                0,
            );
            cmd_tx
                .send(Cmd::Load(
                    Box::new(program),
                    *shared.rv64.borrow(),
                    *shared.memcheck.borrow(),
                    *shared.check_cconv.borrow(),
                ))
                .ok();
        }
    } else {
        widgets
            .status_bar
            .set_status_text("Assembly failed; see Assembler Messages", 0);
    }
}

#[allow(clippy::too_many_arguments)]
fn bind_menu_events(
    widgets: &Widgets,
    shared: &std::rc::Rc<Shared>,
    narrator: &std::rc::Rc<Narrator>,
    cmd_tx: &Sender<Cmd>,
    memory_listeners: &tools::MemoryListeners,
    counts_listeners: &tools_lab::CountsListeners,
    listener_tags: &std::rc::Rc<std::cell::RefCell<u32>>,
    waiting_flag: &std::rc::Rc<std::cell::RefCell<bool>>,
) {
    let memory_listeners = std::rc::Rc::clone(memory_listeners);
    let counts_listeners = std::rc::Rc::clone(counts_listeners);
    let listener_tags = std::rc::Rc::clone(listener_tags);
    let waiting_flag = std::rc::Rc::clone(waiting_flag);
    let fr = widgets.frame;
    let w = widgets.clone();
    let sh = std::rc::Rc::clone(shared);
    let nar = std::rc::Rc::clone(narrator);
    let tx = cmd_tx.clone();
    fr.on_menu(move |event| {
        match event.get_id() {
            ID_NEW => {
                w.editor.set_text("");
                *sh.program_path.borrow_mut() = None;
                w.status_bar.set_status_text("New file", 0);
                nar.speak(narration::file_action(*sh.verbosity.borrow(), "New file", None));
                w.editor.set_focus();
            }
            ID_OPEN => {
                let dialog = FileDialog::builder(&fr)
                    .with_message("Open assembly source")
                    .with_wildcard("Assembly sources (*.s;*.asm)|*.s;*.asm|All files (*.*)|*.*")
                    .build();
                if dialog.show_modal() == ID_OK {
                    if let Some(path) = dialog.get_path() {
                        match std::fs::read_to_string(&path) {
                            Ok(text) => {
                                w.editor.set_text(&text);
                                *sh.program_path.borrow_mut() = Some(path.clone());
                                w.status_bar.set_status_text(&format!("Opened {path}"), 0);
                                let name = std::path::Path::new(&path)
                                    .file_name()
                                    .map(|f| f.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| path.clone());
                                nar.speak(narration::file_action(
                                    *sh.verbosity.borrow(),
                                    "Opened",
                                    Some(&name),
                                ));
                            }
                            Err(e) => w
                                .status_bar
                                .set_status_text(&format!("Cannot open {path}: {e}"), 0),
                        }
                    }
                }
                dialog.destroy();
            }
            ID_SAVE => {
                let existing = sh.program_path.borrow().clone();
                let path = match existing {
                    Some(p) => p,
                    None => {
                        let dialog = FileDialog::builder(&fr)
                            .with_message("Save assembly source")
                            .with_wildcard(
                                "Assembly sources (*.s;*.asm)|*.s;*.asm|All files (*.*)|*.*",
                            )
                            .build();
                        let chosen = if dialog.show_modal() == ID_OK {
                            dialog.get_path()
                        } else {
                            None
                        };
                        dialog.destroy();
                        match chosen {
                            Some(p) => p,
                            None => return,
                        }
                    }
                };
                let text = w.editor.get_text();
                match std::fs::write(&path, text) {
                    Ok(()) => {
                        *sh.program_path.borrow_mut() = Some(path.clone());
                        w.status_bar.set_status_text(&format!("Saved {path}"), 0);
                        let name = std::path::Path::new(&path)
                            .file_name()
                            .map(|f| f.to_string_lossy().into_owned())
                            .unwrap_or_else(|| path.clone());
                        nar.speak(narration::file_action(
                            *sh.verbosity.borrow(),
                            "Saved",
                            Some(&name),
                        ));
                    }
                    Err(e) => w
                        .status_bar
                        .set_status_text(&format!("Cannot save {path}: {e}"), 0),
                }
            }
            ID_SETTINGS => show_settings_dialog(&fr, &sh),
            ID_RECONNECT_SPEECH => {
                nar.reconnect();
                w.status_bar.set_status_text(&nar.backend_summary(), 0);
            }
            ID_EXIT => fr.close(true),
            ID_RUN_ASSEMBLE => do_assemble(&w, &sh, &nar, &tx),
            ID_RUN_RUN => run_program(&sh, &nar, &tx),
            ID_RUN_STEP => {
                tx.send(Cmd::Step).ok();
            }
            ID_RUN_BACKSTEP => {
                tx.send(Cmd::Backstep).ok();
            }
            ID_RUN_PAUSE => {
                tx.send(Cmd::Pause).ok();
            }
            ID_RUN_STOP => {
                tx.send(Cmd::Pause).ok();
            }
            ID_RUN_RESET => {
                tx.send(Cmd::Reset).ok();
                nar.speak(narration::reset_done(*sh.verbosity.borrow()));
            }
            ID_RUN_TOGGLE_BREAK => toggle_selected_breakpoint(&w, &sh, &nar, &tx),
            ID_RUN_TOGGLE_WATCH => toggle_selected_watchpoint(&w, &tx),
            ID_TOOL_FLOAT => {
                tools_float::FloatRepTool::open();
            }
            ID_TOOL_LAB => {
                let tag = tools::next_tag(&listener_tags);
                let listeners = memory_listeners.clone();
                let tx2 = tx.clone();
                tools_lab::DigitalLabSim::open(tx2, listeners, tag);
                // One read to paint the initial display state; the listener
                // also refreshes on every later tagged read.
                tx.send(Cmd::ReadMemory {
                    addr: 0xffff_0000,
                    len: 0x16,
                    tag,
                })
                .ok();
            }
            ID_TOOL_COUNTER => {
                let tag = tools::next_tag(&listener_tags);
                let counts = counts_listeners.clone();
                let tx2 = tx.clone();
                tools_lab::InstructionCounter::open(tx2, counts, tag);
            }
            ID_TOOL_TIMER => {
                let tx2 = tx.clone();
                let waiting = waiting_flag.clone();
                tools_timer::TimerTool::open(tx2, waiting);
            }
            ID_TOOL_DECODE => {
                tools_decode::DecodeTool::open();
            }
            ID_TOOL_BITMAP => {
                let tag = tools::next_tag(&listener_tags);
                let listeners = memory_listeners.clone();
                let tx2 = tx.clone();
                let wake: Rc<dyn Fn()> = Rc::new(|| {});
                tools::BitmapTool::open(tx2, listeners, tag, wake);
            }
            ID_SHORTCUTS => show_shortcuts_dialog(&fr),
            ID_ABOUT => w
                .status_bar
                .set_status_text("AsAccess: an accessibility-first RISC-V IDE", 0),
            _ => {}
        }
    });
}

// --- Dialogs ---------------------------------------------------------------

fn show_settings_dialog(frame: &Frame, shared: &Rc<Shared>) {
    let dialog = Dialog::builder(frame, "Settings")
        .with_style(DialogStyle::DefaultDialogStyle | DialogStyle::ResizeBorder)
        .with_size(420, 200)
        .build();
    dialog.set_accessibility_label("Settings dialog");

    let panel = Panel::builder(&dialog).build();
    let sizer = BoxSizer::builder(Orientation::Vertical).build();

    let verbosity_label = StaticText::builder(&panel)
        .with_label("Announcement verbosity:")
        .build();
    let verbosity = Choice::builder(&panel)
        .with_choices(vec![
            "Off".to_string(),
            "Brief".to_string(),
            "Verbose".to_string(),
        ])
        .with_selection(Some(match *shared.verbosity.borrow() {
            Verbosity::Off => 0,
            Verbosity::Brief => 1,
            Verbosity::Verbose => 2,
        }))
        .build();
    verbosity.set_accessibility_label("Announcement verbosity");
    verbosity.set_accessibility_description("How much narration detail the screen reader speaks");
    let verbosity_row = BoxSizer::builder(Orientation::Horizontal).build();
    verbosity_row.add(
        &verbosity_label,
        0,
        SizerFlag::AlignCenterVertical | SizerFlag::All,
        4,
    );
    verbosity_row.add(&verbosity, 1, SizerFlag::Expand | SizerFlag::All, 4);
    sizer.add_sizer(&verbosity_row, 0, SizerFlag::Expand, 0);

    let xlen_label = StaticText::builder(&panel)
        .with_label("Instruction set:")
        .build();
    let xlen = Choice::builder(&panel)
        .with_choices(vec!["RV32 (default)".to_string(), "RV64".to_string()])
        .with_selection(Some(if *shared.rv64.borrow() { 1 } else { 0 }))
        .build();
    xlen.set_accessibility_label("Instruction set");
    xlen.set_accessibility_description(
        "RV32 or RV64; applies the next time you assemble. RV64 stack operations should keep eight byte alignment",
    );
    let xlen_row = BoxSizer::builder(Orientation::Horizontal).build();
    xlen_row.add(
        &xlen_label,
        0,
        SizerFlag::AlignCenterVertical | SizerFlag::All,
        4,
    );
    xlen_row.add(&xlen, 1, SizerFlag::Expand | SizerFlag::All, 4);
    sizer.add_sizer(&xlen_row, 0, SizerFlag::Expand, 0);

    let memcheck = CheckBox::builder(&panel)
        .with_label("Halt on reads of uninitialized memory (memcheck)")
        .with_value(*shared.memcheck.borrow())
        .build();
    memcheck.set_accessibility_label("Memcheck");
    memcheck.set_accessibility_description("Pause the program when it reads memory it never wrote");
    sizer.add(&memcheck, 0, SizerFlag::All, 4);

    let cconv = CheckBox::builder(&panel)
        .with_label("Check the calling convention on function returns")
        .with_value(*shared.check_cconv.borrow())
        .build();
    cconv.set_accessibility_label("Calling convention checker");
    cconv.set_accessibility_description(
        "Pause when a function return changes the stack pointer or an s register from its value at the call",
    );
    sizer.add(&cconv, 0, SizerFlag::All, 4);

    let compressed = CheckBox::builder(&panel)
        .with_label("Assemble compressed (c.*) instructions")
        .with_value(*shared.allow_compressed.borrow())
        .build();
    compressed.set_accessibility_label("Compressed instructions");
    compressed.set_accessibility_description(
        "Allow sixteen bit c dot instructions; applies the next time you assemble",
    );
    sizer.add(&compressed, 0, SizerFlag::All, 4);

    // wxID_CANCEL makes wx's dialog char hook close on Escape from anywhere
    // in the dialog; the click handler still ends with ID_OK.
    let close_btn = Button::builder(&panel)
        .with_label("Close")
        .with_id(ID_CANCEL)
        .build();
    close_btn.set_accessibility_label("Close settings");
    let dlg = dialog;
    let choice = verbosity;
    let xlen_choice = xlen;
    let memcheck_box = memcheck;
    let cconv_box = cconv;
    let compressed_box = compressed;
    let sh = shared.clone();
    close_btn.on_click(move |_| {
        if let Some(sel) = choice.get_selection() {
            *sh.verbosity.borrow_mut() = match sel {
                0 => Verbosity::Off,
                1 => Verbosity::Brief,
                _ => Verbosity::Verbose,
            };
        }
        if let Some(sel) = xlen_choice.get_selection() {
            *sh.rv64.borrow_mut() = sel == 1;
        }
        *sh.memcheck.borrow_mut() = memcheck_box.get_value();
        *sh.check_cconv.borrow_mut() = cconv_box.get_value();
        *sh.allow_compressed.borrow_mut() = compressed_box.get_value();
        dlg.end_modal(ID_OK);
    });
    sizer.add(
        &close_btn,
        0,
        SizerFlag::AlignCenterHorizontal | SizerFlag::All,
        8,
    );

    panel.set_sizer(sizer, true);
    let dialog_sizer = BoxSizer::builder(Orientation::Vertical).build();
    dialog_sizer.add(&panel, 1, SizerFlag::Expand, 0);
    dialog.set_sizer(dialog_sizer, true);

    let _ = dialog.show_modal();
    dialog.destroy();
}

fn show_shortcuts_dialog(frame: &Frame) {
    let dialog = Dialog::builder(frame, "Keyboard Shortcuts")
        .with_style(DialogStyle::DefaultDialogStyle | DialogStyle::ResizeBorder)
        .with_size(460, 400)
        .build();
    dialog.set_accessibility_label("Keyboard shortcuts dialog");

    let panel = Panel::builder(&dialog).build();
    let sizer = BoxSizer::builder(Orientation::Vertical).build();

    let shortcuts: [(&str, &str); 16] = [
        ("Assemble", "F3"),
        ("Run program", "F5"),
        ("Pause run", "F9"),
        ("Step instruction", "F7"),
        ("Backstep instruction", "F8"),
        ("Stop program", "F11"),
        ("Reset program", "F12"),
        ("Toggle breakpoint", "Ctrl+D"),
        ("Toggle memory watchpoint", "Ctrl+W"),
        ("Move focus out of the editor", "Tab or Shift+Tab"),
        ("Insert a tab character in the editor", "Ctrl+Tab"),
        ("New file", "Ctrl+N"),
        ("Open file", "Ctrl+O"),
        ("Save file", "Ctrl+S"),
        ("Settings", "File menu"),
        ("Show this dialog", "F1"),
    ];

    let list = ListCtrl::builder(&panel)
        .with_style(ListCtrlStyle::Report | ListCtrlStyle::SingleSel)
        .build();
    list.insert_column(0, "Action", ListColumnFormat::Left, 240);
    list.insert_column(1, "Shortcut", ListColumnFormat::Left, 160);
    for (i, (action, key)) in shortcuts.iter().enumerate() {
        let idx = list.insert_item(i as i64, action, None);
        list.set_item_text_by_column(idx as i64, 1, key);
    }
    list.set_accessibility_label("Keyboard shortcuts list");
    sizer.add(&list, 1, SizerFlag::Expand | SizerFlag::All, 4);

    let close_btn = Button::builder(&panel)
        .with_label("Close")
        .with_id(ID_CANCEL)
        .build();
    close_btn.set_accessibility_label("Close keyboard shortcuts");
    let dlg = dialog;
    close_btn.on_click(move |_| {
        dlg.end_modal(ID_OK);
    });
    sizer.add(
        &close_btn,
        0,
        SizerFlag::AlignCenterHorizontal | SizerFlag::All,
        6,
    );

    panel.set_sizer(sizer, true);
    let dialog_sizer = BoxSizer::builder(Orientation::Vertical).build();
    dialog_sizer.add(&panel, 1, SizerFlag::Expand, 0);
    dialog.set_sizer(dialog_sizer, true);

    let _ = dialog.show_modal();
    dialog.destroy();
}
