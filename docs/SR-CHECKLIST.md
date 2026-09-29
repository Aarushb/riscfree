# Screen reader release checklist

Run before every release and after any GUI change. Record the date, tester,
screen reader version, and results in the table at the bottom. A release is
not done while any box is unchecked.

## NVDA (primary Windows target)

- [ ] App launches with NVDA running; focus lands in the editor and the window name is announced.
- [ ] Tab order: toolbar buttons, editor, state notebook tabs, register list, program list, memory pane, Run I/O output, program input, Send.
- [ ] Editor: typing is echoed; arrowing through lines reads code; line numbers do not pollute speech; error markers are perceivable (via the Assembler Messages route).
- [ ] Assemble with a deliberate error: brief announcement says "Assembly failed, N errors"; the Assembler Messages list reads severity, location, and message per row; Enter jumps the editor to the line.
- [ ] Assemble clean: announcement "Assembled, N instructions, no errors."
- [ ] F5 runs a printing program; output lands in the read-only Run I/O control and is traversable by line; the input line accepts text and Send forwards it to a reading program.
- [ ] F7 step announces the executed line and changed registers (Brief); Verbose adds memory writes.
- [ ] F8 backstep announces the undone state.
- [ ] F5 from a breakpoint stop continues past it; re-arrival stops again.
- [ ] Program exit announces "Program finished" with the instruction count.
- [ ] Registers tab: rows read as "x10, a0, value"; decimal/hex formatting is what is spoken.
- [ ] Program tab: Enter/Ctrl+D toggles a breakpoint and the Breakpoint column reads "on"; the PC row is identifiable.
- [ ] Memory tab: jump-to-address works; rows read address, hex bytes, ASCII.
- [ ] Floating Point tab: rows read register name, float, double, bits.
- [ ] Tools > Bitmap Display: grid rows read as hex pixel colors; changing base/size updates the grid.
- [ ] F1 opens the shortcut dialog; the list is fully readable.
- [ ] Settings verbosity Off silences all announcements; Verbose adds memory and location detail.

## JAWS (secondary Windows target)

- [ ] App launches and main window name is read.
- [ ] Editor: text readable, caret movement announced (Scintilla path may differ from NVDA's — record behavior honestly; the plain-text fallback mode is the accepted workaround if Scintilla reads poorly).
- [ ] Buttons, menus, status bar fields, and all list panes report names and values.
- [ ] Assemble/run/step announcements arrive (Prism routing).
- [ ] Record any pane where JAWS reads nothing and whether the fallback editor mode resolves it.

## Narrator (baseline sanity)

- [ ] Main window controls are named; buttons activate via keyboard.

## Results

| Date | Screen reader + version | Tester | Result | Notes |
|---|---|---|---|---|
| | | | | |
