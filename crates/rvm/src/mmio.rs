//! MMIO devices at 0xffff0000: RARS's Keyboard and Display Simulator ports.
//! The receiver holds keystrokes polled from the host; the transmitter
//! forwards bytes to the host console. Interrupt delivery is a later task —
//! the enable bits are plain storage until trap routing lands.

use crate::host::Host;
use std::collections::VecDeque;

/// Window size above the base (RARS maps the MMIO block in the top 64 KiB).
pub(crate) const WINDOW: u32 = 0x1_0000;

/// Register offsets from the MMIO base.
mod reg {
    pub const RECV_CTRL: u32 = 0x00;
    pub const RECV_DATA: u32 = 0x04;
    pub const XMIT_CTRL: u32 = 0x08;
    pub const XMIT_DATA: u32 = 0x0c;
}

pub(crate) struct Mmio {
    base: u32,
    /// Typed but not yet read keys (bit 0 of receiver control reads 1 while
    /// this is non-empty; reading receiver data pops one).
    pending: VecDeque<u8>,
    recv_ie: bool,
    xmit_ie: bool,
    /// Last cursor-position command (byte 7): X = bits 20-31, Y = bits 8-19.
    /// Recorded for the GUI's future cursor support; our display is the text
    /// stream, so positioning itself is a no-op.
    #[allow(dead_code)]
    cursor: Option<(u32, u32)>,
}

impl Mmio {
    pub(crate) fn new(base: u32) -> Self {
        Mmio { base, pending: VecDeque::new(), recv_ie: false, xmit_ie: false, cursor: None }
    }

    /// True when an address lands in the MMIO window.
    pub(crate) fn in_window(&self, addr: u32) -> bool {
        addr.wrapping_sub(self.base) < WINDOW
    }

    pub(crate) fn load(&mut self, addr: u32, width: u32, host: &mut dyn Host) -> u64 {
        let off = addr.wrapping_sub(self.base);
        // Every receiver access polls the host first: this model has no
        // callback when a key arrives, so polling on access is what makes the
        // ready bit reflect reality.
        if off == reg::RECV_CTRL || off == reg::RECV_DATA {
            while let Some(c) = host.poll_input_char() {
                self.pending.push_back(c);
            }
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
            reg::XMIT_CTRL => self.xmit_ie = value & 0b10 != 0,
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
            // Receiver data and unmapped registers ignore writes.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ScriptHost;

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
}
