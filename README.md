# AsAccess

An accessibility-first RISC-V assembler, emulator, and IDE for the desktop, built to replace RARS for coursework. Native Windows app, written in Rust.

## Why this exists

I am a blind computing science student, and RARS is the simulator my courses actually use for RISC-V. It is a good simulator wrapped in a Java Swing UI that screen readers can barely navigate, so I built [rars_access](https://github.com/Aarushb/rars_access), a command-line wrapper that makes the common workflows usable. That wrapper carried me through my labs, and I am proud of it, but a wrapper over an inaccessible GUI is still a workaround. Every pane RARS cannot name, every value I have to compute by hand because a dialog will not read itself, is extra work my sighted classmates never see.

AsAccess is the real thing: an IDE where accessibility is part of the architecture rather than something bolted on later. Sighted students get a fast, native desktop app, and blind and low-vision students get the same app where every register, memory cell, assembler error, and runtime event is reachable from the keyboard and spoken by their screen reader. Same course material, same labs, no second-class seat.

## What it does today

- Full RISC-V teaching core: RV32 and RV64, integer base plus M, Zicsr, F/D floating point, and the C compressed extension, with RARS-compatible memory layout, syscalls, and function keys.
- An assembler with real diagnostics: errors carry severity, file, line, and column, land in a navigable message list, and jumping to one lands your editor caret on the offending line. Macros, `.eqv`, and the usual directives all work.
- A debugger: run, pause, step, backstep (yes, undo an instruction), breakpoints, memory watchpoints, memcheck for reads of uninitialized memory, and a calling-convention checker that catches clobbered `s` registers and stack imbalance on function return.
- The tools RARS courses rely on: bitmap display, digital lab sim with the seven-segment displays and hex keypad, float representation, instruction counter, timer tool, and an instruction decoder.
- A headless CLI for scripting and autograding: assemble, run, dump registers, and emit JSON Lines execution traces for machine-checked grading.
- A differential corpus of example and test programs, run against RARS itself where RARS can run them, so compatibility is verified rather than assumed.

## How accessibility is done

This is the part I care about most, so it gets its own section.

- Every control is a native widget with a real name, role, and description in the UI Automation tree. No custom-drawn panes, no silent controls, no focus traps. Tab, Shift+Tab, and the arrow keys walk the whole interface.
- State changes that have no natural focus change speak directly through [Prism](https://github.com/ethindp/prism), which routes to whichever screen reader bridge is active: NVDA, JAWS, SAPI, and friends. Assembling, running, halting, breakpoint hits, and program output are announced without you having to hunt for them. Announcements have three verbosity levels, including off.
- RARS's keyboard conventions are kept: F3 assembles, F5 runs, F7 steps, F8 steps back, F12 resets. Screen reader users should not have to learn a second set of keys from their classmates.
- The editor follows the standard Windows conventions: Tab moves focus, Ctrl+Tab inserts the tab character, Escape closes dialogs.
- All of this is verified, not claimed. A scripted UI Automation test assembles and runs a program through the real GUI on every change, and the manual [screen reader checklist](docs/SR-CHECKLIST.md) is walked with a real NVDA session before any release, by me, with the speech log to prove it.

## Getting started

Building from source needs Rust, Visual Studio Build Tools, and a local Prism build; the exact steps live in [docs/BUILDING.md](docs/BUILDING.md). The short version:

```bash
cargo run --release -p asaccess          # the GUI
cargo run --release -p asaccess-cli -- --run examples/asm/fibonacci.s
```

Once built, open any `.s` file, press F3 to assemble and F5 to run. Examples for every subsystem (console I/O, files, interrupts, the bitmap display, compressed instructions) live in `examples/asm/`.

## Documentation

- [Building](docs/BUILDING.md): toolchain setup, the Prism build, and packaging the portable zip.
- [Compatibility](docs/COMPAT.md): every deliberate behavior difference from RARS, listed so ported course material can be reasoned about.
- [Screen reader checklist](docs/SR-CHECKLIST.md): the verification pass every release runs, with results.

## Status and roadmap

Windows first, and in active development. The feature set above is complete and kept honest by an automated suite of 250+ tests, a scripted UI Automation run against the real GUI, and my own NVDA sessions. What is next: JAWS runtime verification beyond NVDA, more course-material dogfooding as my terms demand it, and packaging polish. If you are a screen reader user teaching or taking RISC-V, I especially want your feedback: what is missing for your course is exactly what I want on the roadmap.

The long-term dream, same one I wrote about in rars_access, is that nobody should need a wrapper like mine at all. Accessibility built in from the start is better for everyone, and it is not harder if you decide to do it before the first line of code. This project is my proof of that.

## Contributing

Issues, pull requests, and feedback are all welcome. If you use a screen reader and something in here fights you, that is a bug worth filing even if it seems minor. Accessibility regressions are treated like correctness regressions here.

## License

[MIT](LICENSE)
