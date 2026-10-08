# Using RISC-Free

A walkthrough of the application for keyboard and screen reader users, and the reference for the command-line interface. The GUI reads the same assembly the CLI does, so you can prototype in either.

## The main window

The window opens maximized and is laid out in four bands, top to bottom:

1. The menu bar: File, Run, Tools, Help.
2. The run controls: Assemble, Run, Step, Backstep, Pause, Stop, Reset. Plain buttons, no icons, all reachable with Tab.
3. The editor, a code editor with line numbers. Tab and Shift+Tab move focus in and out of it; Ctrl+Tab inserts a literal tab character. A lone Alt tap opens the menu bar.
4. The state and output area, two tab groups stacked on the right. State views: Registers, Program, Memory, Floating Point. Output views: Run I/O and Assembler Messages.

A status bar along the bottom carries the current state, such as pc, instruction count, and halt reasons.

## A first run

1. Open a file with Ctrl+O, or start fresh with Ctrl+N. Example programs live in `examples/asm/` in the repository.
2. Press F3 to assemble. You will hear "Assembled. N instructions, no errors." or, on failure, "Assembly failed. N errors." with the details waiting in the Assembler Messages tab. Arrow onto a message row to hear severity, location, and text; Enter moves the editor caret to that line.
3. Press F5 to run. Programs that print send their output to the Run I/O tab, which is a read-only transcript you can traverse by line.
4. If a program reads input, type into the Program input field in the Run I/O tab and press Send. Input is delivered one line at a time.
5. When the program exits you will hear "Program finished. N instructions executed." F12 resets the machine to its initial state at any point.

## Keyboard reference

- F3: assemble
- F5: run (pressing again continues past a breakpoint stop)
- F7: step one instruction
- F8: backstep, undo one instruction
- F9: pause a run
- F11: stop a run
- F12: reset the machine
- Ctrl+D or Enter on a Program row: toggle a breakpoint
- Ctrl+W on a Program row: toggle a memory watchpoint
- F1: keyboard shortcut dialog
- Ctrl+N, Ctrl+O, Ctrl+S: new, open, save

These match RARS's conventions on purpose, so course habits transfer.

## Debugging

Step with F7 and each executed instruction is announced by name with its register effects, for example "add a2, a0, a1. a2 is 12." F8 backsteps and announces what was undone. The Program tab lists every assembled instruction with its address, machine encoding, source text, breakpoint state, and a Current marker at the pc. Toggle breakpoints from the row with Enter or Ctrl+D; the new state is spoken so you never have to arrow back and check.

Watchpoints (Ctrl+W on a Program row) pause the run when that address is written. Memcheck, a calling-convention checker, and other run-time strictness live in Settings and are opt in per session.

## The panes

- Registers: all thirty-two integer registers, reading as register number, ABI name, and current value.
- Program: the assembled instruction listing described above.
- Memory: a hex and ASCII window over memory. Type a hexadecimal address in the field and press Go; rows read address, bytes, and ASCII.
- Floating Point: the thirty-two fp registers with float, double, and raw bit values.
- Run I/O: the program console, output transcript plus the input line.
- Assembler Messages: diagnostics from the last assemble.

## Tools

The Tools menu opens the optional instruments, each in its own window:

- Bitmap Display: watch a memory region as a pixel grid, rendered as text, one image row per line with each pixel as six hexadecimal color digits.
- Digital Lab Sim: the seven-segment displays a program controls through MMIO, and a hex keypad that delivers scan codes (and optional interrupts) to the program.
- Float Representation: convert between raw hex bits and float values.
- Instruction Counter: per-opcode execution counts since the last reset.
- Timer Tool: arm or clear the machine timer for interrupt experiments.
- Instruction Decode: break any 32-bit instruction word into its fields, in plain language.

## Settings

The Settings dialog carries: announcement verbosity (Off, Brief, Verbose), instruction set (RV32 or RV64, applied at the next assemble), memcheck, the calling-convention checker, and compressed instruction support. OK applies and closes, Cancel discards, Escape counts as Cancel.

## Command line

```text
riscfree-cli [options] <file.s> [more.s ...] [-- program args]

--run              assemble and execute
--max-steps N      stop after N instructions
--dump-regs        print non-zero registers after the run
--rv64             assemble and execute as RV64
--compressed       accept C (compressed) 16-bit instructions
--timer N          raise timer interrupts (cause 16) every N instructions
--trace-json FILE  write one JSON object per executed instruction to FILE
```

Console output goes to stdout and input comes from stdin, so the CLI pipes cleanly into scripts and graders.
