//! Tools: the RARS tool family rebuilt accessibility-first. Each visual tool
//! ships with a textual twin that carries the same information, per the PRD's
//! requirement that no tool be usable only by eye.
//!
//! Tool windows receive memory refreshes through tagged `Evt::Memory`
//! responses routed by the main event pump.

use crate::bridge::{Cmd, Evt};
use std::rc::Rc;
use std::sync::mpsc::Sender;
use wxdragon::prelude::*;

/// Registry the main pump consults for tagged memory responses.
pub type MemoryListeners = Rc<std::cell::RefCell<Vec<(u32, Box<dyn Fn(&[u8])>)>>>;

/// Allocate the next listener tag.
pub fn next_tag(tags: &Rc<std::cell::RefCell<u32>>) -> u32 {
    let mut t = tags.borrow_mut();
    let tag = *t;
    *t += 1;
    tag
}

/// Bitmap Display: a word-per-pixel RGB grid at a configurable base address.
/// The textual twin renders every row as hex pixels in a read-only control,
/// so a screen reader user navigates the same grid a sighted user watches.
pub struct BitmapTool {
    _frame: Frame,
}

impl BitmapTool {
    pub fn open(
        cmd_tx: Sender<Cmd>,
        listeners: MemoryListeners,
        tag: u32,
        request_refresh: Rc<dyn Fn()>,
    ) -> Self {
        let frame = Frame::builder()
            .with_title("Bitmap Display")
            .with_size(Size::new(720, 520))
            .build();
        frame.set_accessibility_label("Bitmap Display tool");

        let panel = Panel::builder(&frame).build();
        panel.set_accessibility_label("Bitmap Display settings and grid");
        let sizer = BoxSizer::builder(Orientation::Vertical).build();

        // Settings row: base address, width and height in pixels (one byte
        // per channel, one word per pixel, like RARS's defaults).
        let settings = BoxSizer::builder(Orientation::Horizontal).build();
        let make_label = |text: &str| {
            let l = StaticText::builder(&panel).with_label(text).build();
            l
        };
        let base_label = make_label("Base address (hex):");
        settings.add(&base_label, 0, SizerFlag::AlignCenterVertical | SizerFlag::All, 2);
        let base_input = TextCtrl::builder(&panel).build();
        base_input.set_value("10010000");
        base_input.set_accessibility_label("Bitmap base address");
        settings.add(&base_input, 1, SizerFlag::Expand | SizerFlag::All, 2);

        let width_label = make_label("Width:");
        settings.add(&width_label, 0, SizerFlag::AlignCenterVertical | SizerFlag::All, 2);
        let width_input = TextCtrl::builder(&panel).build();
        width_input.set_value("16");
        width_input.set_accessibility_label("Bitmap width in pixels");
        settings.add(&width_input, 0, SizerFlag::All, 2);

        let height_label = make_label("Height:");
        settings.add(&height_label, 0, SizerFlag::AlignCenterVertical | SizerFlag::All, 2);
        let height_input = TextCtrl::builder(&panel).build();
        height_input.set_value("16");
        height_input.set_accessibility_label("Bitmap height in pixels");
        settings.add(&height_input, 0, SizerFlag::All, 2);

        let go = Button::builder(&panel).with_label("Apply").build();
        go.set_accessibility_label("Apply bitmap settings");
        settings.add(&go, 0, SizerFlag::All, 2);
        sizer.add_sizer(&settings, 0, SizerFlag::Expand, 0);

        let grid_label = StaticText::builder(&panel)
            .with_label("Pixel grid (each pixel is RRGGBB, one row per line)")
            .build();
        sizer.add(&grid_label, 0, SizerFlag::All, 2);

        let grid = TextCtrl::builder(&panel)
            .with_style(TextCtrlStyle::MultiLine | TextCtrlStyle::ReadOnly)
            .build();
        grid.set_value("No program running.");
        grid.set_accessibility_label("Bitmap pixel grid");
        grid.set_accessibility_description(
            "The bitmap as text: one image row per line, each pixel as six hexadecimal color digits",
        );
        if let Some(font) = Font::builder()
            .with_point_size(9)
            .with_family(FontFamily::Teletype)
            .build()
        {
            grid.set_font(&font);
        }
        sizer.add(&grid, 1, SizerFlag::Expand | SizerFlag::All, 2);

        panel.set_sizer(sizer, true);

        // Settings state shared with the listener closure.
        struct Settings {
            base: std::cell::Cell<u32>,
            width: std::cell::Cell<u32>,
            height: std::cell::Cell<u32>,
        }
        let settings_state = Rc::new(Settings {
            base: std::cell::Cell::new(0x1001_0000),
            width: std::cell::Cell::new(16),
            height: std::cell::Cell::new(16),
        });

        // Listener: rebuild the grid text from a memory response.
        let grid_for_listener = grid;
        let state_for_listener = settings_state.clone();
        listeners.borrow_mut().push((
            tag,
            Box::new(move |bytes: &[u8]| {
                let width = state_for_listener.width.get().max(1) as usize;
                let height = state_for_listener.height.get().max(1) as usize;
                let mut lines = Vec::with_capacity(height);
                for row in 0..height {
                    let mut line = String::new();
                    for col in 0..width {
                        let index = row * width + col;
                        let start = index * 4;
                        if start + 4 <= bytes.len() {
                            let word = u32::from_le_bytes([
                                bytes[start],
                                bytes[start + 1],
                                bytes[start + 2],
                                bytes[start + 3],
                            ]);
                            line.push_str(&format!("{:06x} ", word & 0x00ff_ffff));
                        }
                    }
                    lines.push(line.trim_end().to_string());
                }
                grid_for_listener.set_value(&lines.join("\n"));
            }),
        ));

        // Apply: read the settings, then ask for a fresh window.
        let state_for_apply = settings_state.clone();
        let base_for_apply = base_input;
        let width_for_apply = width_input;
        let height_for_apply = height_input;
        let tx_for_apply = cmd_tx.clone();
        let request_for_apply = request_refresh.clone();
        go.on_click(move |_| {
            let base = u32::from_str_radix(
                base_for_apply.get_value().trim().trim_start_matches("0x"),
                16,
            )
            .unwrap_or(0x1001_0000);
            let width = width_for_apply.get_value().trim().parse().unwrap_or(16).clamp(1, 256);
            let height = height_for_apply.get_value().trim().parse().unwrap_or(16).clamp(1, 256);
            state_for_apply.base.set(base);
            state_for_apply.width.set(width);
            state_for_apply.height.set(height);
            tx_for_apply
                .send(Cmd::ReadMemory { addr: base, len: width * height * 4, tag })
                .ok();
            request_for_apply();
        });

        // First paint: request the default window.
        cmd_tx
            .send(Cmd::ReadMemory {
                addr: settings_state.base.get(),
                len: settings_state.width.get() * settings_state.height.get() * 4,
                tag,
            })
            .ok();

        frame.show(true);
        BitmapTool { _frame: frame }
    }
}

/// Route a tagged memory event to its listener; returns true when a listener
/// matched (so the caller can skip default handling).
pub fn route_memory_event(listeners: &MemoryListeners, evt: &Evt) -> bool {
    if let Evt::Memory { bytes, tag, .. } = evt {
        for (listener_tag, listener) in listeners.borrow().iter() {
            if listener_tag == tag {
                listener(bytes);
                return true;
            }
        }
    }
    false
}
