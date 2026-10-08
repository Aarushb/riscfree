# Compatibility notes: deliberate divergences from RARS

RISC-Free aims for drop-in compatibility with RARS course material: same memory map, syscall numbers, directives, pseudo-instructions, and run-control keys. Where behavior deliberately differs, it is listed here so ported course material can be reasoned about. Anything not listed matches RARS's observed behavior (the differential corpus in `tests/` is the arbiter for ambiguous cases).

## Interaction and presentation

1. **Dialog syscalls are inline by default.** RARS shows modal input/message dialogs for syscalls 50-60. RISC-Free renders them as prompts and messages in the Run I/O console (inline, non-modal), which works better with screen readers and avoids interrupting a run for routine output. A compatibility setting to restore modal dialogs is planned. The program-facing contract is unchanged: same syscall numbers, results, and buffer conventions.
2. **ConfirmDialog (syscall 50) returns 1/0**, not RARS's GUI 1/2/0 (yes/no/cancel). There is no cancel in the inline prompt model.
3. **Status bar and messages replace popup reporting.** Assembler errors go to the navigable Assembler Messages list instead of dialogs; routine program output never interrupts as a modal.
4. **Run I/O transcript history is not cleared on reset.** Reset restores the machine state; the transcript keeps history so output from before a reset stays readable. RARS clears Run I/O on each run.
5. **Basic-instruction display names.** Expanded pseudo-instructions render with modern ABI names (`a0`, `ra`, `sp`) rather than RARS's `$`-prefixed MIPS-era names, and x1 renders as `ra` rather than `at` in the basic view.

## Assembler

6. **Macros must be defined before their call site.** Expansion is inline in the first pass; RARS-style forward references to macros defined later in the file do not resolve.
7. **Nested macro definitions are rejected** (`E-UNSUPPORTED`) rather than silently accepted.
8. **`.float`/`.double` accept Rust-style float literals** (decimal, scientific notation, `inf`, `nan`) rather than Java's exact float syntax.
9. **Diagnostics carry column numbers and stable codes** (e.g. `E-IMM`, `E-UNDEF`) in addition to line numbers; message wording is RISC-Free's own and written to read well aloud.

## Machine

10. **`fence` is a no-op**, matching RARS, and `fence.i` likewise.
11. **The exiting `ecall` is not counted in `instret`.** The instruction that halts the machine (exit ecall) retires but the counter stops at the instruction before it. Programs that compare cycle counts against RARS may differ by one for exit paths.
12. **MMIO keyboard via GUI input line.** The MMIO receiver port is fed by the Run I/O input line (or CLI stdin, where non-blocking polling is documented as unavailable). RARS feeds it from its dedicated Keyboard/Display tool window.
13. **Interrupt-enable bits are storage only** until trap delivery lands: writing the receiver/transmitter interrupt-enable bits has no effect yet; polled MMIO works fully.
14. **`peek` semantics for the memory view**: unmapped addresses read as zeros in the Memory tab (matching machine read semantics), and the MMIO window reads as its live device registers.

## Interrupts and traps

15. **Interrupt enable gating**: a device interrupt fires when the device's own interrupt-enable bit is set AND user interrupt enable (`ustatus.UIE`) is on. A request raised while gated stays pending and fires when enables allow.
16. **Transmitter interrupt is edge-triggered**: RARS fires the display interrupt when the transmitter becomes ready; ours is always ready, so the 0-to-1 transition of the enable bit latches exactly one interrupt, re-armed only by disabling and re-enabling.
17. **Priority within the external class**: pending external interrupts deliver lowest-cause-first as a deterministic stand-in for RARS's unspecified device-registration order (keyboard 0x40 before display 0x80).
18. **`wfi` waits on our event model**: with nothing pending it parks and the run loop services host keyboard input and armed timers; virtual time fast-forwards to the next timer tick rather than wall-clock waiting.
19. **Synchronous exceptions vector only when a handler is configured** (`utvec != 0`); otherwise they halt with an error like RARS without an exception handler. Breakpoints fire on trap-handler entry, which RARS does not do (their issue: PR #225 unmerged).
20. **CSR `time` reads the instruction clock**, not wall time; the syscall-30 wall-clock read is unchanged.

## RV64 mode

21. **Initial `$sp` is 0x7fffeff8, two words below RARS's 0x7fffeffc.** RARS's fixed initial `$sp` is only four-byte aligned, which is not enough for RV64 doubleword `sd`/`ld` prologues; starting eight-byte aligned lets both word and doubleword stack frames work without an initial alignment adjustment.
22. **`negw`, `sext.w`, `zext.*` pseudo-ops are not implemented yet**; the underlying instructions are. RARS's auipc-based label-form `ld` is replaced by the lui+`%lo` form, which is safe given the low-4GB memory map both tools share.

## Extensions beyond RARS

23. **The C compressed extension is opt-in per run.** Pass `--compressed` on the CLI (or enable compressed in the GUI settings) to assemble 16-bit `c.*` instructions and execute them. RARS has no compressed support, so course material never uses it; programs that stick to 32-bit instructions are unaffected either way.
