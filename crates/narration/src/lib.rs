//! narration: pure functions from machine and UI events to announcement
//! text, with a verbosity policy. Screen reader rules encoded here:
//! native widgets announce their own state, so this module only handles
//! events with no natural focus change, and never repeats operational
//! instructions. Every string this module can produce is unit-tested.

use rvm::{Change, Halt};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Verbosity {
    /// No announcements at all.
    Off,
    /// Concise spoken summaries; the default.
    #[default]
    Brief,
    /// Adds memory effects and locations.
    Verbose,
}

const ABI: &[&str] = &[
    "zero", "ra", "sp", "gp", "tp", "t0", "t1", "t2", "s0", "s1", "a0", "a1", "a2", "a3", "a4",
    "a5", "a6", "a7", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11", "t3", "t4",
    "t5", "t6",
];

fn reg_name(index: usize) -> &'static str {
    ABI.get(index).copied().unwrap_or("??")
}

/// Result of an assemble run. `first_error` is already formatted as
/// `file:line:col: message` by the caller.
pub fn assemble_done(
    v: Verbosity,
    ok: bool,
    instructions: usize,
    error_count: usize,
    first_error: Option<&str>,
) -> Option<String> {
    if v == Verbosity::Off {
        return None;
    }
    if ok {
        let mut text = format!("Assembled. {instructions} instructions, no errors.");
        if v == Verbosity::Verbose {
            text = format!("{text} Ready to run.");
        }
        Some(text)
    } else {
        let mut text = format!(
            "Assembly failed. {error_count} error{}.",
            if error_count == 1 { "" } else { "s" }
        );
        if v == Verbosity::Verbose {
            if let Some(err) = first_error {
                text = format!("{text} First: {err}");
            }
        }
        Some(text)
    }
}

/// Announcement after one stepped instruction. Brief names the statement and
/// the register changes; verbose adds memory writes and mentions the jump
/// target for control flow. Register-only summaries cap at three names so a
/// multi-write instruction cannot drone on.
pub fn step_done(v: Verbosity, statement_text: &str, changes: &[Change]) -> Option<String> {
    if v == Verbosity::Off {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    let reg_changes: Vec<&Change> = changes
        .iter()
        .filter(|c| matches!(c, Change::Reg { .. }))
        .collect();
    for c in reg_changes.iter().take(3) {
        if let Change::Reg { index, new, .. } = c {
            parts.push(format!("{} is {}", reg_name(*index), format_value(*new)));
        }
    }
    if reg_changes.len() > 3 {
        parts.push("and more registers changed".to_string());
    }
    if v == Verbosity::Verbose {
        for c in changes.iter().filter(|c| matches!(c, Change::Mem { .. })) {
            if let Change::Mem {
                addr, new, width, ..
            } = c
            {
                parts.push(format!(
                    "wrote {width} bytes at 0x{addr:08x}, now {}",
                    format_value(*new)
                ));
            }
        }
        for c in changes.iter().filter(|c| matches!(c, Change::Pc { .. })) {
            if let Change::Pc { new, .. } = c {
                parts.push(format!("continuing at 0x{new:08x}"));
            }
        }
    }
    if parts.is_empty() {
        Some(statement_text.trim().to_string())
    } else {
        Some(format!("{}. {}", statement_text.trim(), parts.join("; ")))
    }
}

/// Announcement when the machine halts for any reason.
pub fn halted(v: Verbosity, halt: &Halt, instret: u64, location: Option<&str>) -> Option<String> {
    if v == Verbosity::Off {
        return None;
    }
    let count = format!("{instret} instructions executed");
    let base = match halt {
        Halt::Exit { code } => {
            if *code == 0 {
                format!("Program finished. {count}.")
            } else {
                format!("Program exited with code {code}. {count}.")
            }
        }
        Halt::DroppedOff => format!("Program dropped off the bottom of the text segment. {count}."),
        Halt::Breakpoint => match location {
            Some(loc) => format!("Stopped at breakpoint, {loc}."),
            None => "Stopped at breakpoint.".to_string(),
        },
        Halt::Ebreak => match location {
            Some(loc) => format!("Stopped by ebreak, {loc}."),
            None => "Stopped by ebreak.".to_string(),
        },
        Halt::Error { message } => format!("Simulation error: {message}."),
        Halt::Limit => format!("Stopped at the step limit. {count}."),
        Halt::Watchpoint { description } => format!("{description}."),
        Halt::Memcheck { description } => format!("{description}."),
        Halt::CallingConvention { description } => format!("{description}."),
    };
    if v == Verbosity::Verbose {
        if let Some(loc) = location {
            if !matches!(halt, Halt::Breakpoint | Halt::Ebreak) {
                return Some(format!("{base} At {loc}."));
            }
        }
    }
    Some(base)
}

/// Announcement when the user pauses a run (F9).
pub fn run_paused(v: Verbosity) -> Option<String> {
    match v {
        Verbosity::Off => None,
        _ => Some("Paused.".to_string()),
    }
}

/// Announcement when the user stops a run (F11), distinct from a halt the
/// program reached on its own.
pub fn run_stopped(v: Verbosity) -> Option<String> {
    match v {
        Verbosity::Off => None,
        _ => Some("Program stopped.".to_string()),
    }
}

/// Announcement when a run starts. Programs that block on console input
/// produce neither output nor a halt for a while, and a silent run key
/// sounds identical to a dead one.
pub fn run_started(v: Verbosity) -> Option<String> {
    match v {
        Verbosity::Off => None,
        _ => Some("Program running.".to_string()),
    }
}

/// Announcement after a reset: a state change with no focus change of its
/// own, so the reader must confirm it happened.
pub fn reset_done(v: Verbosity) -> Option<String> {
    match v {
        Verbosity::Off => None,
        _ => Some("Program reset.".to_string()),
    }
}

/// Announcement after a backstep. Same shape as step_done, prefixed so the
/// two directions are never confused by ear.
pub fn backstep_done(v: Verbosity, statement_text: &str, changes: &[Change]) -> Option<String> {
    let inner = step_done(v, statement_text, changes)?;
    Some(format!("Backstepped. {inner}"))
}

/// Announcement for a breakpoint toggle in the program list: the list row
/// shows the new state, but speaking it saves the trip back to the row.
pub fn breakpoint_changed(v: Verbosity, on: bool, line: u32) -> Option<String> {
    match v {
        Verbosity::Off => None,
        _ => Some(format!(
            "Breakpoint {} at line {line}.",
            if on { "on" } else { "off" }
        )),
    }
}

/// Announcement for editor file actions. `name` is the file title when one
/// applies (opened or saved path); None for actions like New.
pub fn file_action(v: Verbosity, action: &str, name: Option<&str>) -> Option<String> {
    match v {
        Verbosity::Off => None,
        _ => Some(match name {
            Some(name) => format!("{action} {name}."),
            None => format!("{action}."),
        }),
    }
}

/// New program output while the console is not focused. Bulk output is only
/// summarized; verbose counts lines, brief just says output arrived.
pub fn output_arrived(v: Verbosity, new_lines: usize) -> Option<String> {
    match v {
        Verbosity::Off => None,
        Verbosity::Brief => Some("New program output.".to_string()),
        Verbosity::Verbose => Some(format!(
            "New program output: {new_lines} new line{}.",
            if new_lines == 1 { "" } else { "s" }
        )),
    }
}

fn format_value(v: u64) -> String {
    let signed = v as u32 as i32;
    if (-1000..=0xffff).contains(&signed) {
        format!("{signed}")
    } else {
        format!("{signed} (0x{:08x})", v as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assemble_results() {
        assert_eq!(
            assemble_done(Verbosity::Brief, true, 42, 0, None).as_deref(),
            Some("Assembled. 42 instructions, no errors.")
        );
        assert_eq!(
            assemble_done(Verbosity::Verbose, true, 42, 0, None).as_deref(),
            Some("Assembled. 42 instructions, no errors. Ready to run.")
        );
        assert_eq!(
            assemble_done(Verbosity::Brief, false, 0, 2, Some("t.s:3:1: boom")).as_deref(),
            Some("Assembly failed. 2 errors.")
        );
        assert_eq!(
            assemble_done(Verbosity::Verbose, false, 0, 1, Some("t.s:3:1: boom")).as_deref(),
            Some("Assembly failed. 1 error. First: t.s:3:1: boom")
        );
        assert_eq!(assemble_done(Verbosity::Off, true, 1, 0, None), None);
    }

    #[test]
    fn step_brief_names_statement_and_registers() {
        let changes = vec![
            Change::Reg {
                index: 10,
                old: 0,
                new: 5,
            },
            Change::Pc { old: 0, new: 4 },
        ];
        assert_eq!(
            step_done(Verbosity::Brief, "li a0, 5", &changes).as_deref(),
            Some("li a0, 5. a0 is 5")
        );
    }

    #[test]
    fn step_verbose_adds_memory_and_pc() {
        let changes = vec![
            Change::Reg {
                index: 10,
                old: 0,
                new: 5,
            },
            Change::Mem {
                addr: 0x7fff_eff0,
                old: 0,
                new: 5,
                width: 4,
            },
            Change::Pc {
                old: 0,
                new: 0x0040_0010,
            },
        ];
        assert_eq!(
            step_done(Verbosity::Verbose, "sw a0, 0(sp)", &changes).as_deref(),
            Some("sw a0, 0(sp). a0 is 5; wrote 4 bytes at 0x7fffeff0, now 5; continuing at 0x00400010")
        );
    }

    #[test]
    fn pause_and_stop_announcements() {
        assert_eq!(run_paused(Verbosity::Brief).as_deref(), Some("Paused."));
        assert_eq!(run_stopped(Verbosity::Brief).as_deref(), Some("Program stopped."));
        assert_eq!(run_paused(Verbosity::Off), None);
        assert_eq!(run_stopped(Verbosity::Off), None);
    }

    #[test]
    fn run_started_announcement() {
        assert_eq!(
            run_started(Verbosity::Brief).as_deref(),
            Some("Program running.")
        );
        assert_eq!(run_started(Verbosity::Off), None);
    }

    #[test]
    fn reset_and_backstep_announcements() {
        assert_eq!(
            reset_done(Verbosity::Brief).as_deref(),
            Some("Program reset.")
        );
        assert_eq!(reset_done(Verbosity::Off), None);
        let changes = vec![Change::Reg {
            index: 10,
            old: 5,
            new: 0,
        }];
        assert_eq!(
            backstep_done(Verbosity::Brief, "li a0, 5", &changes).as_deref(),
            Some("Backstepped. li a0, 5. a0 is 0")
        );
        assert_eq!(backstep_done(Verbosity::Off, "li a0, 5", &changes), None);
    }

    #[test]
    fn breakpoint_toggle_announcements() {
        assert_eq!(
            breakpoint_changed(Verbosity::Brief, true, 5).as_deref(),
            Some("Breakpoint on at line 5.")
        );
        assert_eq!(
            breakpoint_changed(Verbosity::Brief, false, 5).as_deref(),
            Some("Breakpoint off at line 5.")
        );
        assert_eq!(breakpoint_changed(Verbosity::Off, true, 5), None);
    }

    #[test]
    fn file_action_announcements() {
        assert_eq!(
            file_action(Verbosity::Brief, "Opened", Some("demo.s")).as_deref(),
            Some("Opened demo.s.")
        );
        assert_eq!(
            file_action(Verbosity::Brief, "New file", None).as_deref(),
            Some("New file.")
        );
        assert_eq!(file_action(Verbosity::Off, "Saved", Some("x.s")), None);
    }

    #[test]
    fn step_caps_three_registers() {
        let changes: Vec<Change> = (10..16)
            .map(|i| Change::Reg {
                index: i,
                old: 0,
                new: i as u64,
            })
            .collect();
        let text = step_done(Verbosity::Brief, "many writes", &changes).unwrap();
        assert!(text.contains("and more registers changed"), "{text}");
        assert_eq!(text.matches(" is ").count(), 3);
    }

    #[test]
    fn step_without_register_changes_speaks_statement() {
        assert_eq!(
            step_done(Verbosity::Brief, "nop", &[]).as_deref(),
            Some("nop")
        );
    }

    #[test]
    fn halt_messages() {
        assert_eq!(
            halted(Verbosity::Brief, &Halt::Exit { code: 0 }, 812, None).as_deref(),
            Some("Program finished. 812 instructions executed.")
        );
        assert_eq!(
            halted(Verbosity::Brief, &Halt::Exit { code: 3 }, 10, None).as_deref(),
            Some("Program exited with code 3. 10 instructions executed.")
        );
        assert_eq!(
            halted(Verbosity::Brief, &Halt::DroppedOff, 5, None).as_deref(),
            Some("Program dropped off the bottom of the text segment. 5 instructions executed.")
        );
        assert_eq!(
            halted(Verbosity::Brief, &Halt::Breakpoint, 3, Some("line 22")).as_deref(),
            Some("Stopped at breakpoint, line 22.")
        );
        assert_eq!(
            halted(
                Verbosity::Verbose,
                &Halt::Exit { code: 0 },
                5,
                Some("line 9")
            )
            .as_deref(),
            Some("Program finished. 5 instructions executed. At line 9.")
        );
        assert_eq!(
            halted(Verbosity::Off, &Halt::Exit { code: 0 }, 5, None),
            None
        );
    }

    #[test]
    fn output_summaries() {
        assert_eq!(
            output_arrived(Verbosity::Brief, 12).as_deref(),
            Some("New program output.")
        );
        assert_eq!(
            output_arrived(Verbosity::Verbose, 12).as_deref(),
            Some("New program output: 12 new lines.")
        );
        assert_eq!(
            output_arrived(Verbosity::Verbose, 1).as_deref(),
            Some("New program output: 1 new line.")
        );
        assert_eq!(output_arrived(Verbosity::Off, 12), None);
    }
}
