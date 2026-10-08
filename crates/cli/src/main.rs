//! riscfree-cli: headless assembler and simulator. Exit codes: 0 success,
//! 1 usage error, 2 assembly errors, 3 simulation error.

use rvasm::{AsmConfig, InputFile};
use rvm::{Event, Halt, Machine, MachineConfig, StdHost};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut files: Vec<InputFile> = Vec::new();
    let mut run = false;
    let mut max_steps: Option<u64> = None;
    let mut dump_regs = false;
    let mut rv64 = false;
    let mut compressed = false;
    let mut trace_path: Option<String> = None;
    let mut timer_interval: Option<u64> = None;
    let mut prog_args: Vec<String> = Vec::new();

    let mut i = 0usize;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => {
                print_help();
                return ExitCode::SUCCESS;
            }
            "--run" => run = true,
            "--max-steps" => {
                i += 1;
                max_steps = args.get(i).and_then(|v| v.parse().ok());
                if max_steps.is_none() {
                    eprintln!("--max-steps needs a number");
                    return ExitCode::from(1);
                }
            }
            "--rv64" => rv64 = true,
            "--compressed" => compressed = true,
            "--trace-json" => {
                i += 1;
                trace_path = args.get(i).cloned();
                if trace_path.is_none() {
                    eprintln!("--trace-json needs a file path");
                    return ExitCode::from(1);
                }
            }
            "--timer" => {
                i += 1;
                timer_interval = args.get(i).and_then(|v| v.parse().ok());
                if timer_interval.is_none() {
                    eprintln!("--timer needs an instruction count");
                    return ExitCode::from(1);
                }
            }
            "--dump-regs" => dump_regs = true,
            "--" => {
                prog_args = args[i + 1..].to_vec();
                break;
            }
            f => files.push(InputFile {
                name: f.to_string(),
                source: std::fs::read_to_string(f).unwrap_or_else(|e| {
                    eprintln!("cannot read {f}: {e}");
                    std::process::exit(1);
                }),
            }),
        }
        i += 1;
    }

    if files.is_empty() {
        print_help();
        return ExitCode::from(1);
    }

    let result = rvasm::assemble(
        &files,
        &AsmConfig {
            rv64,
            allow_compressed: compressed,
            ..AsmConfig::default()
        },
    );
    for d in &result.diagnostics {
        let file = files[d.pos.file].name.as_str();
        let code = d.code;
        let sev = if d.is_error() { "error" } else { "warning" };
        eprintln!(
            "{sev} [{code}] {file}:{}:{}: {}",
            d.pos.line,
            d.pos.col + 1,
            d.message
        );
    }
    if result.has_errors() || result.program.is_none() {
        return ExitCode::from(2);
    }
    let program = result.program.unwrap();

    if !run {
        println!(
            "assembled {} instructions, {} bytes of data, {} symbols",
            program.statements.len(),
            program.data.bytes.len(),
            program.symbols.iter().count()
        );
        return ExitCode::SUCCESS;
    }

    let mut machine = Machine::new(
        program,
        Box::new(StdHost::default()),
        MachineConfig {
            rv64,
            ..MachineConfig::default()
        },
    );
    if !prog_args.is_empty() {
        machine.set_program_args(&prog_args);
    }
    if let Some(interval) = timer_interval {
        machine.set_timer(interval);
    }
    if let Some(trace_path) = &trace_path {
        // Per-instruction JSON lines for autograders, written to a file so
        // the program's own stdout stays clean. One object per line.
        let Ok(mut trace_out) = std::fs::File::create(trace_path) else {
            eprintln!("cannot write trace file {trace_path}");
            return ExitCode::from(1);
        };
        use std::io::Write;
        loop {
            let pc_before = machine.pc();
            let outcome = machine.step();
            if !outcome.executed {
                for e in &outcome.events {
                    if let Event::Halted(h) = e {
                        print_halt(h, &machine);
                        if let Event::Halted(Halt::Error { .. }) = e {
                            return ExitCode::from(3);
                        }
                    }
                }
                break;
            }
            let regs: Vec<(usize, u64)> = outcome
                .changes
                .iter()
                .filter_map(|c| match c {
                    rvm::Change::Reg { index, new, .. } => Some((*index, *new)),
                    _ => None,
                })
                .collect();
            let mem: Vec<(u32, u64, u8)> = outcome
                .changes
                .iter()
                .filter_map(|c| match c {
                    rvm::Change::Mem {
                        addr, new, width, ..
                    } => Some((*addr, *new, *width)),
                    _ => None,
                })
                .collect();
            let regs_json = regs
                .iter()
                .map(|(i, v)| format!("[{i},{v}]"))
                .collect::<Vec<_>>()
                .join(",");
            let mem_json = mem
                .iter()
                .map(|(a, v, w)| format!("[{a},{v},{w}]"))
                .collect::<Vec<_>>()
                .join(",");
            writeln!(
                trace_out,
                "{{\"pc\":{pc_before},\"word\":{},\"regs\":[{regs_json}],\"mem\":[{mem_json}]}}",
                machine
                    .program()
                    .statement_at(pc_before)
                    .map(|s| s.encoding)
                    .unwrap_or(0)
            )
            .ok();
            if machine.is_terminated() {
                break;
            }
            if let Some(limit) = max_steps {
                if machine.instret() >= limit {
                    eprintln!("simulation: reached the step limit");
                    break;
                }
            }
        }
    } else {
        let events = machine.run(max_steps);
        for e in &events {
            if let Event::Halted(h) = e {
                print_halt(h, &machine);
            }
        }
    }
    if dump_regs {
        for idx in 0..32 {
            let v = machine.reg(idx);
            if v != 0 {
                println!("x{idx:<2} = 0x{v:016x} ({})", v as u32 as i32);
            }
        }
        println!("pc   = 0x{:08x}", machine.pc());
        println!("instructions executed: {}", machine.instret());
    }
    match machine.exit_code() {
        Some(code) if code != 0 => ExitCode::from(code.clamp(1, 255) as u8),
        _ => ExitCode::SUCCESS,
    }
}

fn print_halt(halt: &Halt, machine: &Machine) {
    match halt {
        Halt::Exit { code: _ } => {}
        Halt::DroppedOff => {
            eprintln!("simulation: program dropped off the bottom of the text segment")
        }
        Halt::Breakpoint => eprintln!("simulation: stopped at breakpoint (0x{:08x})", machine.pc()),
        Halt::Ebreak => eprintln!("simulation: ebreak (0x{:08x})", machine.pc()),
        Halt::Error { message } => {
            eprintln!("simulation error: {message}");
            std::process::exit(3);
        }
        Halt::Watchpoint { description }
        | Halt::Memcheck { description }
        | Halt::CallingConvention { description } => {
            eprintln!("simulation: {description}");
        }
        Halt::Limit => eprintln!("simulation: reached the step limit"),
    }
}

fn print_help() {
    println!(
        "riscfree-cli [options] <file.s> [more.s ...] [-- program args]\n\
         options:\n\
           --run         assemble and execute\n\
           --max-steps N stop after N instructions\n\
           --dump-regs   print non-zero registers after the run
           --rv64        assemble and execute as RV64
           --compressed  accept C (compressed) 16-bit instructions
           --timer N     raise timer interrupts (cause 16) every N instructions
           --trace-json <file>
                         write one JSON object per executed instruction to <file>\n\
         the program's console output goes to stdout; input comes from stdin."
    );
}
