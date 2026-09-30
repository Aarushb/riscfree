//! MMIO devices at 0xffff0000: RARS's Keyboard and Display Simulator ports.
//! The receiver holds keystrokes polled from the host; the transmitter
//! forwards bytes to the host console. Interrupt sources live here too: the
//! receiver raises its request while a key waits under its enable bit, and
//! the transmitter latches a request on the rising edge of its enable bit
//! (see the note on `xmit_edge` for why the edge model).

use crate::host::Host;
use std::collections::VecDeque;

/// Window size above the base (RARS maps the MMIO block in the top 64 KiB).
pub(crate) const WINDOW: u32 = 0x1_0000;

/// Register offsets from the MMIO base.
pub(crate) mod reg {
    pub const RECV_CTRL: u32 = 0x00;
    pub const RECV_DATA: u32 = 0x04;
    pub const XMIT_CTRL: u32 = 0x08;
    pub const XMIT_DATA: u32 = 0x0c;
    // Digital Lab Sim (RARS tool) registers, byte-wide each.
    /// Seven-segment display 1: one bit per segment (bit 0 = a ... bit 6 = g).
    pub const SEG_DISPLAY_1: u32 = 0x10;
    /// Seven-segment display 2, same encoding as display 1.
    pub const SEG_DISPLAY_2: u32 = 0x11;
    /// Hex keypad control: low nibble is the scanned row (RARS writes
    /// 0x0N for row N), bit 7 enables keypad interrupts.
    pub const HEX_KEYS_CTRL: u32 = 0x12;
    /// Counter control: the GUI tool drives the timing, so the machine
    /// only stores the byte for the program to read back.
    pub const COUNTER_CTRL: u32 = 0x13;
    /// Hex keypad result: the scan code of the pressed key (0 = none).
    /// Device-written; program writes are ignored.
    pub const HEX_KEYS_RESULT: u32 = 0x14;
}

pub(crate) struct Mmio {
    base: u32,
    /// Typed but not yet read keys (bit 0 of receiver control reads 1 while
    /// this is non-empty; reading receiver data pops one).
    pending: VecDeque<u8>,
    pub(crate) recv_ie: bool,
    pub(crate) xmit_ie: bool,
    /// Transmitter interrupt request latch. RARS fires the display interrupt
    /// when the transmitter *becomes* ready; our transmitter is always ready,
    /// so a level model would request forever. Instead the request is latched
    /// on the 0→1 edge of the enable bit and consumed by trap delivery, so
    /// each re-enable yields exactly one interrupt (documented interpretation).
    xmit_edge: bool,
    /// Last cursor-position command (byte 7): X = bits 20-31, Y = bits 8-19.
    /// Recorded for the GUI's future cursor support; our display is the text
    /// stream, so positioning itself is a no-op.
    #[allow(dead_code)]
    cursor: Option<(u32, u32)>,
    /// Digital Lab Sim: last bytes written to the two seven-segment displays
    /// (segment bits), the keypad control byte, the counter control byte,
    /// and the last pressed scan code.
    seg_displays: [u8; 2],
    pub(crate) keys_ctrl: u8,
    counter_ctrl: u8,
    keys_result: u8,
    /// Latched keypad interrupt request: set by a press while the keypad
    /// enable bit (control bit 7) is on, consumed by trap delivery.
    pub(crate) keys_pending: bool,
}

impl Mmio {
    pub(crate) fn new(base: u32) -> Self {
        Mmio {
            base,
            pending: VecDeque::new(),
            recv_ie: false,
            xmit_ie: false,
            xmit_edge: false,
            cursor: None,
            seg_displays: [0; 2],
            keys_ctrl: 0,
            counter_ctrl: 0,
            keys_result: 0,
            keys_pending: false,
        }
    }

    /// True when an address lands in the MMIO window.
    pub(crate) fn in_window(&self, addr: u32) -> bool {
        addr.wrapping_sub(self.base) < WINDOW
    }

    /// A key sits in the receiver queue (bit 0 of receiver control).
    pub(crate) fn has_input(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Test hook: drop every queued keystroke.
    #[cfg(test)]
    pub(crate) fn clear_input(&mut self) {
        self.pending.clear();
    }

    /// Drain whatever keystrokes the host has buffered into the receiver.
    /// Called from the trap delivery point so interrupt-driven programs see
    /// keys without touching the MMIO port; gated by `recv_ie` up in the
    /// machine so polling never steals input from `read_line` syscalls in
    /// non-interrupt programs.
    pub(crate) fn poll_input(&mut self, host: &mut dyn Host) {
        while let Some(c) = host.poll_input_char() {
            self.pending.push_back(c);
        }
    }

    /// Consume the transmitter request latch; true when one was pending.
    pub(crate) fn take_xmit_edge(&mut self) -> bool {
        std::mem::take(&mut self.xmit_edge)
    }

    /// True while the transmitter request latch is set (read-only peek).
    pub(crate) fn xmit_edge_latched(&self) -> bool {
        self.xmit_edge
    }

    /// Re-arm the transmitter request latch (backstep undid a delivery).
    pub(crate) fn restore_xmit_edge(&mut self) {
        self.xmit_edge = true;
    }

    /// A hex-keypad press from the GUI tool: the scan code lands in the
    /// result register (where the program's load of +0x14 finds it), and a
    /// press while the keypad enable bit (control bit 7) is on latches the
    /// HEX_KEYS interrupt request for the next delivery point.
    pub(crate) fn press_hex_key(&mut self, scan_code: u8) {
        self.keys_result = scan_code;
        if self.keys_ctrl & 0x80 != 0 {
            self.keys_pending = true;
        }
    }

    pub(crate) fn load(&mut self, addr: u32, width: u32, host: &mut dyn Host) -> u64 {
        let off = addr.wrapping_sub(self.base);
        // Every receiver access polls the host first: this model has no
        // callback when a key arrives, so polling on access is what makes the
        // ready bit reflect reality.
        if off == reg::RECV_CTRL || off == reg::RECV_DATA {
            self.poll_input(host);
        }
        let word: u32 = match off {
            reg::RECV_CTRL => u32::from(!self.pending.is_empty()) | (u32::from(self.recv_ie) << 1),
            reg::RECV_DATA => match self.pending.pop_front() {
                Some(c) => u32::from(c),
                // RARS reads 0 with ready clear when no key is waiting.
                None => 0,
            },
            // The display is always ready in this sequential model.
            reg::XMIT_CTRL => 1 | (u32::from(self.xmit_ie) << 1),
            // Write-only port.
            reg::XMIT_DATA => 0,
            // Digital Lab Sim: stored bytes read back as the program (or the
            // GUI tool, via the machine API) last wrote them.
            reg::SEG_DISPLAY_1 => u32::from(self.seg_displays[0]),
            reg::SEG_DISPLAY_2 => u32::from(self.seg_displays[1]),
            reg::HEX_KEYS_CTRL => u32::from(self.keys_ctrl),
            reg::COUNTER_CTRL => u32::from(self.counter_ctrl),
            reg::HEX_KEYS_RESULT => u32::from(self.keys_result),
            // Unmapped registers inside the window read 0.
            _ => 0,
        };
        match width {
            1 => u64::from(word as u8),
            2 => u64::from(word as u16),
            _ => u64::from(word),
        }
    }

    pub(crate) fn store(&mut self, addr: u32, value: u64, host: &mut dyn Host) {
        let off = addr.wrapping_sub(self.base);
        match off {
            // The ready bits themselves are read-only state.
            reg::RECV_CTRL => self.recv_ie = value & 0b10 != 0,
            reg::XMIT_CTRL => {
                let new_ie = value & 0b10 != 0;
                // RARS raises the display interrupt when the transmitter
                // becomes ready. Ready is permanently true here, so the
                // enable-bit edge stands in for the ready edge: each 0→1
                // transition requests one interrupt.
                if new_ie && !self.xmit_ie {
                    self.xmit_edge = true;
                }
                self.xmit_ie = new_ie;
            }
            reg::XMIT_DATA => {
                let byte = value as u8;
                if byte == 0x07 {
                    // RARS's cursor-position command rather than a printable
                    // bell: X in bits 20-31, Y in bits 8-19. We store the
                    // coordinates but the text-stream display ignores them.
                    let x = ((value >> 20) & 0xfff) as u32;
                    let y = ((value >> 8) & 0xfff) as u32;
                    self.cursor = Some((x, y));
                } else {
                    // Forward the raw byte (Latin-1 mapped), so ASCII 12 rides
                    // through as the form-feed clear marker for the UI.
                    let text = (byte as char).to_string();
                    host.write_output(&text);
                }
            }
            // Receiver data ignores program writes (device-owned), as do the
            // unmapped registers.
            //
            // Digital Lab Sim registers: the displays, keypad control, and
            // counter control store their byte; the keypad result is
            // device-written (Machine::press_hex_key), so program writes
            // there are ignored.
            reg::SEG_DISPLAY_1 => self.seg_displays[0] = value as u8,
            reg::SEG_DISPLAY_2 => self.seg_displays[1] = value as u8,
            reg::HEX_KEYS_CTRL => self.keys_ctrl = value as u8,
            reg::COUNTER_CTRL => self.counter_ctrl = value as u8,
            reg::HEX_KEYS_RESULT => {}
            _ => {}
        }
    }
}

/// Tool-facing machine API for the Digital Lab Sim registers: the GUI drives
/// these devices from outside the simulation, the way RARS's tool windows do.
impl crate::Machine {
    /// Push one seven-segment display's segment bits (bit 0 = a ... bit 6 =
    /// g) as the tool window would; a program reading +0x10/+0x11 sees the
    /// last written value. `display` is 0 or 1; others are ignored.
    pub fn set_display_segments(&mut self, display: u32, segments: u8) {
        if let Some(slot) = self.mmio.seg_displays.get_mut(display as usize) {
            *slot = segments;
        }
    }

    /// The segment bits last written to a seven-segment display (0 or 1;
    /// other indices read 0) for the tool window to render.
    pub fn display_segments(&self, display: u32) -> u8 {
        self.mmio
            .seg_displays
            .get(display as usize)
            .copied()
            .unwrap_or(0)
    }

    /// Simulate a hex-keypad press: the scan code (e.g. 0x11 for row 1,
    /// column 1 through 0x88 for row 4, column 4) becomes readable at
    /// +0x14, and — when the program enabled keypad interrupts via bit 7
    /// of the +0x12 control byte — requests the `irq::HEX_KEYS` interrupt
    /// at the next delivery point.
    pub fn press_hex_key(&mut self, scan_code: u8) {
        self.mmio.press_hex_key(scan_code);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csr;
    use crate::ScriptHost;

    /// Hand-encoded uret (the assembler has no uret mnemonic).
    const URET: u32 = 0x0020_0073;

    fn sym(m: &crate::Machine, name: &str) -> u32 {
        m.program().symbols.get(name).unwrap().addr
    }

    #[test]
    fn window_bounds() {
        let mmio = Mmio::new(0xffff_0000);
        assert!(!mmio.in_window(0xffff_0000 - 1));
        assert!(mmio.in_window(0xffff_0000));
        assert!(mmio.in_window(0xffff_ffff));
    }

    #[test]
    fn transmitter_forwards_bytes_and_takes_cursor_commands() {
        let mut mmio = Mmio::new(0xffff_0000);
        let mut host = ScriptHost::default();
        mmio.store(0xffff_000c, u64::from(b'A'), &mut host);
        mmio.store(0xffff_000c, 12, &mut host); // form feed: forwarded raw
                                                // Cursor command: byte 7, X = 10, Y = 5 — no output produced.
        mmio.store(0xffff_000c, 7 | (10 << 20) | (5 << 8), &mut host);
        assert_eq!(host.take_output(), "A\u{c}");
    }

    #[test]
    fn receiver_data_queues_then_reads_zero() {
        let mut mmio = Mmio::new(0xffff_0000);
        let mut host = ScriptHost::with_input(vec!["hi".into()]);
        assert_eq!(mmio.load(0xffff_0004, 4, &mut host), u64::from(b'h'));
        assert_eq!(mmio.load(0xffff_0004, 4, &mut host), u64::from(b'i'));
        assert_eq!(mmio.load(0xffff_0004, 4, &mut host), 0);
    }

    #[test]
    fn subword_loads_read_low_bytes_of_the_register() {
        let mut mmio = Mmio::new(0xffff_0000);
        let mut host = ScriptHost::default();
        mmio.store(0xffff_0008, 0b10, &mut host); // enable transmitter interrupt
        assert_eq!(mmio.load(0xffff_0008, 1, &mut host), 3);
        assert_eq!(mmio.load(0xffff_0008, 2, &mut host), 3);
    }

    #[test]
    fn transmitter_edge_latches_one_request_per_enable() {
        let mut mmio = Mmio::new(0xffff_0000);
        let mut host = ScriptHost::default();
        assert!(!mmio.xmit_ie);
        mmio.store(0xffff_0008, 0b10, &mut host); // rising edge: one request
        assert!(mmio.xmit_ie);
        mmio.store(0xffff_0008, 0b10, &mut host); // writing 1 again is no edge
        assert!(mmio.take_xmit_edge());
        assert!(!mmio.take_xmit_edge());
        // Disable then re-enable: a fresh edge, a fresh request.
        mmio.store(0xffff_0008, 0, &mut host);
        mmio.store(0xffff_0008, 0b10, &mut host);
        assert!(mmio.take_xmit_edge());
    }

    #[test]
    fn receiver_poll_drains_host_keys() {
        let mut mmio = Mmio::new(0xffff_0000);
        let mut host = ScriptHost::with_input(vec!["ab".into()]);
        assert!(!mmio.has_input());
        mmio.poll_input(&mut host);
        assert!(mmio.has_input());
    }

    #[test]
    fn machine_routes_transmitter_data_to_output() {
        let src = "\
    li t0, 0xffff000c
    li t1, 'A'
    sw t1, 0(t0)
";
        let host = ScriptHost::default();
        let mut m = crate::testutil::machine_with(src, Box::new(host.clone()));
        m.run(None);
        assert_eq!(host.take_output(), "A");
    }

    #[test]
    fn machine_transmitter_control_reads_ready() {
        let src = "\
    li t0, 0xffff0008
    lw a0, 0(t0)
";
        let mut m = crate::testutil::machine(src);
        m.run(None);
        assert_eq!(m.reg(10), 1); // display ready, interrupt disabled
    }

    #[test]
    fn machine_receiver_ready_bit_and_data_flow() {
        let src = "\
    li t0, 0xffff0000
    lw a0, 0(t0)        # ready = 1, ie = 0
    li t1, 2
    sw t1, 0(t0)        # set the interrupt-enable bit
    lw a1, 0(t0)        # ready | ie = 3
    li t2, 0xffff0004
    lw a2, 0(t2)        # pops the queued key
    lw a3, 0(t0)        # drained: ie only
    lw a4, 0(t2)        # no key -> 0
";
        // Queue order is back-to-front for ScriptHost lines; one char "K".
        let host = ScriptHost::with_input(vec!["K".into()]);
        let mut m = crate::testutil::machine_with(src, Box::new(host));
        m.run(None);
        assert_eq!(m.reg(10), 1);
        assert_eq!(m.reg(11), 3);
        assert_eq!(m.reg(12), u64::from(b'K'));
        assert_eq!(m.reg(13), 2);
        assert_eq!(m.reg(14), 0);
    }

    // ---- Digital Lab Sim registers ----

    #[test]
    fn digital_lab_sim_bytes_store_and_round_trip() {
        // Display bytes, keypad control, and counter control store and read
        // back; the keypad result ignores program writes.
        let mut mmio = Mmio::new(0xffff_0000);
        let mut host = ScriptHost::default();
        mmio.store(0xffff_0010, 0x3f, &mut host); // display 1
        mmio.store(0xffff_0011, 0x06, &mut host); // display 2
        mmio.store(0xffff_0012, 0x81, &mut host); // keypad: row 1, ie on
        mmio.store(0xffff_0013, 0x0f, &mut host); // counter control
        assert_eq!(mmio.load(0xffff_0010, 1, &mut host), 0x3f);
        assert_eq!(mmio.load(0xffff_0011, 1, &mut host), 0x06);
        assert_eq!(mmio.load(0xffff_0012, 1, &mut host), 0x81);
        assert_eq!(mmio.load(0xffff_0013, 1, &mut host), 0x0f);
        mmio.store(0xffff_0014, 0x55, &mut host); // program writes ignored
        assert_eq!(mmio.load(0xffff_0014, 1, &mut host), 0);
    }

    #[test]
    fn machine_display_bytes_round_trip_through_the_program() {
        let src = "\
    li t0, 0xffff0010
    li t1, 0x3f
    sb t1, 0(t0)        # display 1
    li t1, 0x06
    sb t1, 1(t0)        # display 2 at 0xffff0011
    lb a0, 0(t0)        # program reads the stored bytes back
    lb a1, 1(t0)
";
        let mut m = crate::testutil::machine(src);
        m.run(None);
        assert_eq!(m.reg(10), 0x3f);
        assert_eq!(m.reg(11), 0x06);
        // The tool-side API sees what the program wrote...
        assert_eq!(m.display_segments(0), 0x3f);
        assert_eq!(m.display_segments(1), 0x06);
        assert_eq!(m.display_segments(2), 0); // only two displays exist
                                              // ...and the tool-side push lands where a program would read it.
        m.set_display_segments(1, 0x5b);
        let mut host = ScriptHost::default();
        assert_eq!(m.mmio.load(0xffff_0011, 1, &mut host), 0x5b);
    }

    #[test]
    fn keypad_press_interrupts_when_enabled() {
        // The program scans row 1 with the keypad interrupt enabled; the
        // tool presses a key and the handler sees cause 0x200 and the scan
        // code at the result register.
        let src = "\
main:
    li t0, 0xffff0012
    li t1, 0x81         # row 1 scan, bit 7 = keypad interrupt enable
    sb t1, 0(t0)
    la t2, handler
    csrrw x0, utvec, t2
    li t3, 1
    csrrw x0, ustatus, t3 # global UIE
spin:
    beq x0, x0, spin
handler:
    csrrs s1, ucause, x0
    li t4, 0xffff0014
    lb a0, 0(t4)          # the scan code the tool wrote
uret_slot:
    nop
";
        let mut m = crate::testutil::machine(src);
        let uret_addr = sym(&m, "uret_slot");
        m.mem.write_bytes(uret_addr, &URET.to_le_bytes());
        // Run the program up to its spin loop so it has already written the
        // keypad control byte and enabled UIE, then the tool presses the key.
        let spin = sym(&m, "spin");
        while m.pc() != spin {
            m.step();
        }
        m.press_hex_key(0x11); // row 1, column 1
        let events = m.run(Some(200));
        assert_eq!(
            events.last(),
            Some(&crate::Event::Halted(crate::Halt::Limit))
        );
        // The trap carried the HEX_KEYS cause and the handler read the code.
        assert_eq!(
            m.reg(9),
            crate::irq::INTERRUPT_BIT | u64::from(crate::irq::HEX_KEYS)
        );
        assert_eq!(
            m.csr(csr::UCAUSE),
            crate::irq::INTERRUPT_BIT | u64::from(crate::irq::HEX_KEYS)
        );
        assert_eq!(m.reg(10), 0x11);
        // Consumed at delivery: the spin loop does not re-trap.
        assert!(!m.mmio.keys_pending);
    }

    #[test]
    fn keypad_press_without_enable_neither_interrupts_nor_latches() {
        let src = "\
    li t0, 0xffff0012
    li t1, 0x02         # row 2 scan, interrupts disabled
    sb t1, 0(t0)
";
        let mut m = crate::testutil::machine(src);
        m.run(None);
        m.press_hex_key(0x88); // row 4, column 4
                               // No request latched, so nothing can deliver.
        assert!(!m.mmio.keys_pending);
        assert_eq!(m.deliverable_interrupt(), None);
        // The scan code is still readable at the result register.
        let mut host = ScriptHost::default();
        assert_eq!(m.mmio.load(0xffff_0014, 1, &mut host), 0x88);
    }

    #[test]
    fn vectored_mode_offsets_hex_keys_by_cause() {
        let mut m = crate::testutil::machine("    nop\n    nop\n");
        let handler = m.program().text_base + 0x400;
        m.csrs.insert(csr::UTVEC, u64::from(handler) | 2); // vectored
        m.csrs.insert(csr::USTATUS, 1); // UIE
        m.mmio.keys_ctrl = 0x80;
        m.press_hex_key(0x21);
        let before = m.pc();
        m.step(); // the nop retires, then the delivery point traps
        assert_eq!(m.pc(), handler + 4 * crate::irq::HEX_KEYS);
        assert_eq!(
            m.csr(csr::UCAUSE),
            crate::irq::INTERRUPT_BIT | u64::from(crate::irq::HEX_KEYS)
        );
        assert_eq!(m.csr(csr::UEPC), u64::from(before + 4));
        // Backstep re-raises the request, so stepping traps again.
        assert!(m.backstep());
        assert!(m.mmio.keys_pending);
        m.step();
        assert_eq!(m.pc(), handler + 4 * crate::irq::HEX_KEYS);
    }
}
