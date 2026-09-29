# Compatibility notes: deliberate divergences from RARS

AsAccess aims for drop-in compatibility with RARS course material: same memory map, syscall numbers, directives, pseudo-instructions, and run-control keys. Where behavior deliberately differs, it is listed here so ported course material can be reasoned about. Anything not listed matches RARS's observed behavior (the differential corpus in `tests/` is the arbiter for ambiguous cases).

## Interaction and presentation

1. **Dialog syscalls are inline by default.** RARS shows modal input/message dialogs for syscalls 50-60. AsAccess renders them as prompts and messages in the Run I/O console (inline, non-modal), which works better with screen readers and avoids interrupting a run for routine output. A compatibility setting to restore modal dialogs is planned. The program-facing contract is unchanged: same syscall numbers, results, and buffer conventions.
2. **ConfirmDialog (syscall 50) returns 1/0**, not RARS's GUI 1/2/0 (yes/no/cancel). There is no cancel in the inline prompt model.
3. **Status bar and messages replace popup reporting.** Assembler errors go to the navigable Assembler Messages list instead of dialogs; routine program output never interrupts as a modal.
4. **Run I/O transcript history is not cleared on reset.** Reset restores the machine state; the transcript keeps history so output from before a reset stays readable. RARS clears Run I/O on each run.
5. **Basic-instruction display names.** Expanded pseudo-instructions render with modern ABI names (`a0`, `ra`, `sp`) rather than RARS's `$`-prefixed MIPS-era names, and x1 renders as `ra` rather than `at` in the basic view.

## Assembler

6. **Macros must be defined before their call site.** Expansion is inline in the first pass; RARS-style forward references to macros defined later in the file do not resolve.
7. **Nested macro definitions are rejected** (`E-UNSUPPORTED`) rather than silently accepted.
8. **`.float`/`.double` accept Rust-style float literals** (decimal, scientific notation, `inf`, `nan`) rather than Java's exact float syntax.
9. **Diagnostics carry column numbers and stable codes** (e.g. `E-IMM`, `E-UNDEF`) in addition to line numbers; message wording is AsAccess's own and written to read well aloud.

## Machine

10. **`fence` is a no-op**, matching RARS, and `fence.i` likewise.
11. **The exiting `ecall` is not counted in `instret`.** The instruction that halts the machine (exit ecall) retires but the counter stops at the instruction before it. Programs that compare cycle counts against RARS may differ by one for exit paths.
12. **MMIO keyboard via GUI input line.** The MMIO receiver port is fed by the Run I/O input line (or CLI stdin, where non-blocking polling is documented as unavailable). RARS feeds it from its dedicated Keyboard/Display tool window.
13. **Interrupt-enable bits are storage only** until trap delivery lands: writing the receiver/transmitter interrupt-enable bits has no effect yet; polled MMIO works fully.
14. **`peek` semantics for the memory view**: unmapped addresses read as zeros in the Memory tab (matching machine read semantics), and the MMIO window reads as its live device registers.
