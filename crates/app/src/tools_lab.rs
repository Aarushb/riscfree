//! Digital Lab Sim: two seven-segment displays and a hex keypad, driven
//! through the machine's MMIO registers. The displays read as lit-segment
//! letters plus the recognized digit when the pattern matches the standard
//! font, so the state is fully perceivable without seeing the segments.

use crate::bridge::Cmd;

use std::rc::Rc;
use std::sync::mpsc::Sender;
use wxdragon::prelude::*;

pub struct DigitalLabSim {
    _frame: Frame,
}

/// Shared listener type for per-opcode count responses.
pub type CountsListeners = Rc<std::cell::RefCell<Vec<(u32, Box<dyn Fn(&Vec<(u32, u64)>)>)>>>;

/// Map a 7-bit segment mask (bit 0 = a ... bit 6 = g) to a digit-like label.
fn segments_label(segments: u8) -> String {
    let names = ["a", "b", "c", "d", "e", "f", "g"];
    let lit: Vec<&str> = names
        .iter()
        .enumerate()
        .filter(|(bit, _)| segments & (1 << bit) != 0)
        .map(|(_, name)| *name)
        .collect();
    let digit = match segments & 0x7f {
        0x3f => "0",
        0x06 => "1",
        0x5b => "2",
        0x4f => "3",
        0x66 => "4",
        0x6d => "5",
        0x7d => "6",
        0x07 => "7",
        0x7f => "8",
        0x6f => "9",
        0x77 => "A",
        0x7c => "b",
        0x39 => "C",
        0x5e => "d",
        0x79 => "E",
        0x71 => "F",
        _ => "?",
    };
    if lit.is_empty() {
        "blank".to_string()
    } else {
        format!("segments {} (looks like {digit})", lit.join(","))
    }
}

/// Scan codes are 0xRN: row R (1-4), column N as a one-hot bit (1, 2, 4, 8).
/// Key labels follow the common Digital Lab Sim layout.
const KEYPAD: [[(&str, u8); 4]; 4] = [
    [("#1", 0x11), ("#2", 0x12), ("#3", 0x14), ("#A", 0x18)],
    [("#4", 0x21), ("#5", 0x22), ("#6", 0x24), ("#B", 0x28)],
    [("#7", 0x41), ("#8", 0x42), ("#9", 0x44), ("#C", 0x48)],
    [("#0", 0x81), ("#F", 0x82), ("#E", 0x84), ("#D", 0x88)],
];

impl DigitalLabSim {
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        cmd_tx: Sender<Cmd>,
        listeners: crate::tools::MemoryListeners,
        display_tag: u32,
    ) -> Self {
        let frame = Frame::builder()
            .with_title("Digital Lab Sim")
            .with_size(Size::new(560, 420))
            .build();
        frame.set_accessibility_label("Digital Lab Sim tool");

        let panel = Panel::builder(&frame).build();
        panel.set_accessibility_label("Digital Lab Sim displays and keypad");
        let sizer = BoxSizer::builder(Orientation::Vertical).build();

        let displays_label = StaticText::builder(&panel)
            .with_label("Seven segment displays (program-controlled)")
            .build();
        sizer.add(&displays_label, 0, SizerFlag::All, 2);

        let display1 = StaticText::builder(&panel)
            .with_label("Display 1: blank")
            .build();
        display1.set_accessibility_label("Display 1 state");
        sizer.add(&display1, 0, SizerFlag::All, 2);
        let display2 = StaticText::builder(&panel)
            .with_label("Display 2: blank")
            .build();
        display2.set_accessibility_label("Display 2 state");
        sizer.add(&display2, 0, SizerFlag::All, 2);

        let keypad_label = StaticText::builder(&panel)
            .with_label("Hex keypad (each press delivers its scan code to the program)")
            .build();
        sizer.add(&keypad_label, 0, SizerFlag::All, 4);

        // 4x4 keypad grid.
        let grid = GridSizer::builder(4, 4).with_gap(Size::new(4, 4)).build();
        for row in KEYPAD {
            for (label, scan) in row {
                let btn = Button::builder(&panel).with_label(label).build();
                btn.set_accessibility_label(&format!("Hex keypad key {label}"));
                btn.set_accessibility_description(&format!(
                    "Delivers scan code 0x{scan:02x} to the program at {label}"
                ));
                let tx = cmd_tx.clone();
                btn.on_click(move |_| {
                    tx.send(Cmd::PressHexKey(scan)).ok();
                });
                grid.add(&btn, 0, SizerFlag::Expand, 0);
            }
        }
        sizer.add_sizer(&grid, 0, SizerFlag::Expand, 0);

        let hint = StaticText::builder(&panel)
            .with_label("A press interrupts the program only when it enabled keypad interrupts (control register bit 7).")
            .build();
        sizer.add(&hint, 0, SizerFlag::All, 4);

        panel.set_sizer(sizer, true);

        // Display updates ride the periodic State snapshots.
        let d1 = display1;
        let d2 = display2;
        // DLS registers arrive via the tagged memory channel; the tool polls
        // the MMIO window (base 0xffff0000, 0x16 bytes) through its listener.
        listeners.borrow_mut().push((
            display_tag,
            Box::new(move |bytes: &[u8]| {
                let seg = |offset: usize| bytes.get(offset).copied().unwrap_or(0);
                d1.set_label(&format!("Display 1: {}", segments_label(seg(0x10))));
                d2.set_label(&format!("Display 2: {}", segments_label(seg(0x11))));
            }),
        ));

        frame.show(true);
        DigitalLabSim { _frame: frame }
    }
}

/// Instruction Counter: per-opcode execution counts, refreshed on demand.
pub struct InstructionCounter {
    _frame: Frame,
}

impl InstructionCounter {
    pub fn open(cmd_tx: Sender<Cmd>, counts_listeners: CountsListeners, tag: u32) -> Self {
        let frame = Frame::builder()
            .with_title("Instruction Counter")
            .with_size(Size::new(480, 520))
            .build();
        frame.set_accessibility_label("Instruction Counter tool");

        let panel = Panel::builder(&frame).build();
        panel.set_accessibility_label("Instruction Counter counts");
        let sizer = BoxSizer::builder(Orientation::Vertical).build();

        let refresh = Button::builder(&panel).with_label("Refresh counts").build();
        refresh.set_accessibility_label("Refresh counts");
        refresh.set_accessibility_description("Reads the current per-opcode execution counts");
        sizer.add(&refresh, 0, SizerFlag::All, 4);

        let list = ListCtrl::builder(&panel)
            .with_style(ListCtrlStyle::Report | ListCtrlStyle::Virtual | ListCtrlStyle::SingleSel)
            .build();
        list.insert_column(0, "Opcode", ListColumnFormat::Left, 100);
        list.insert_column(1, "Instruction", ListColumnFormat::Left, 140);
        list.insert_column(2, "Count", ListColumnFormat::Left, 140);
        list.set_item_count(0);
        let counts_store: Rc<std::cell::RefCell<Vec<(u32, u64)>>> =
            Rc::new(std::cell::RefCell::new(Vec::new()));
        {
            let store = counts_store.clone();
            assert!(list.set_virtual_text_callback(move |item, col| {
                let guard = store.borrow();
                let Some(entry) = guard.get(item as usize) else {
                    return String::new();
                };
                match col {
                    0 => format!("0x{:02x}", entry.0),
                    1 => rvasm::opcode_representative(entry.0)
                        .unwrap_or("?")
                        .to_string(),
                    2 => entry.1.to_string(),
                    _ => String::new(),
                }
            }));
        }
        list.set_accessibility_label("Instruction counts");
        list.set_accessibility_description("Executed instruction counts grouped by opcode");

        sizer.add(&list, 1, SizerFlag::Expand | SizerFlag::All, 2);
        panel.set_sizer(sizer, true);

        // Listener applies counts to the list.
        {
            let store = counts_store.clone();
            counts_listeners.borrow_mut().push((
                tag,
                Box::new(move |counts: &Vec<(u32, u64)>| {
                    *store.borrow_mut() = counts.clone();
                    list.set_item_count(counts.len() as i64);
                    if !counts.is_empty() {
                        list.refresh_items(0, counts.len() as i64 - 1);
                    }
                }),
            ));
        }
        // Initial fetch.
        cmd_tx.send(Cmd::GetCounts { tag }).ok();

        frame.show(true);
        InstructionCounter { _frame: frame }
    }
}
