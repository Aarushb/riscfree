//! asaccess-cli: headless assembler and simulator. Exit codes: 0 success,
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

    let result = rvasm::assemble(&files, &AsmConfig::default());
    for d in &result.diagnostics {
        let file = files[d.pos.file].name.as_str();
        let code = d.code;
        let sev = if d.is_error() { "error" } else { "warning" };
        eprintln!("{sev} [{code}] {file}:{}:{}: {}", d.pos.line, d.pos.col + 1, d.message);
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

    let mut machine = Machine::new(program, Box::new(StdHost::default()), MachineConfig::default());
    if !prog_args.is_empty() {
        machine.set_program_args(&prog_args);
    }
    let events = machine.run(max_steps);
    for e in &events {
        if let Event::Halted(h) = e {
            match h {
                Halt::Exit { code: _ } => {}
                Halt::DroppedOff => eprintln!("simulation: program dropped off the bottom of the text segment"),
                Halt::Breakpoint => eprintln!("simulation: stopped at breakpoint (0x{:08x})", machine.pc()),
                Halt::Ebreak => eprintln!("simulation: ebreak (0x{:08x})", machine.pc()),
                Halt::Error { message } => {
                    eprintln!("simulation error: {message}");
                    return ExitCode::from(3);
                }
                Halt::Limit => eprintln!("simulation: reached the step limit"),
            }
        }
    }
    if dump_regs {
        for idx in 0..32 {
            let v = machine.reg(idx);
            if v != 0 {
                println!("x{:<2} = 0x{v:016x} ({})", v, v as u32 as i32);
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

fn print_help() {
    println!(
        "asaccess-cli [options] <file.s> [more.s ...] [-- program args]\n\
         options:\n\
           --run         assemble and execute\n\
           --max-steps N stop after N instructions\n\
           --dump-regs   print non-zero registers after the run\n\
         the program's console output goes to stdout; input comes from stdin."
    );
}
