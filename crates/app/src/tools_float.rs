//! Float Representation: RARS's bit-level float inspector, rebuilt with
//! accessible, labeled controls. Converts between raw bits and float values
//! in both directions; no machine state involved.

use std::cell::RefCell;
use std::rc::Rc;
use wxdragon::prelude::*;

pub struct FloatRepTool {
    _frame: Frame,
}

/// Shared so the two conversion directions can read each other's inputs.
struct Fields {
    bits_input: RefCell<Option<TextCtrl>>,
    float_input: RefCell<Option<TextCtrl>>,
    result: RefCell<Option<StaticText>>,
}

impl FloatRepTool {
    pub fn open() -> Self {
        let frame = Frame::builder()
            .with_title("Float Representation")
            .with_size(Size::new(640, 280))
            .build();
        frame.set_accessibility_label("Float Representation tool");

        let panel = Panel::builder(&frame).build();
        panel.set_accessibility_label("Float Representation settings");
        let sizer = BoxSizer::builder(Orientation::Vertical).build();

        let fields = Rc::new(Fields {
            bits_input: RefCell::new(None),
            float_input: RefCell::new(None),
            result: RefCell::new(None),
        });

        // Bits -> float.
        let bits_label = StaticText::builder(&panel)
            .with_label("Bits (8 hex digits):")
            .build();
        sizer.add(&bits_label, 0, SizerFlag::All, 2);
        let bits_input = TextCtrl::builder(&panel).build();
        bits_input.set_value("41200000");
        bits_input.set_accessibility_label("Bits, eight hexadecimal digits");
        bits_input.set_accessibility_description("The raw 32-bit pattern to interpret as a float");
        sizer.add(&bits_input, 0, SizerFlag::Expand | SizerFlag::All, 2);

        // Float -> bits.
        let float_label = StaticText::builder(&panel)
            .with_label("Float value:")
            .build();
        sizer.add(&float_label, 0, SizerFlag::All, 2);
        let float_input = TextCtrl::builder(&panel).build();
        float_input.set_value("10.0");
        float_input.set_accessibility_label("Float value");
        float_input.set_accessibility_description("A decimal float value to convert to bits");
        sizer.add(&float_input, 0, SizerFlag::Expand | SizerFlag::All, 2);

        let convert = Button::builder(&panel).with_label("Convert").build();
        convert.set_accessibility_label("Convert");
        convert.set_accessibility_description("Converts both fields in both directions");
        sizer.add(&convert, 0, SizerFlag::All, 4);

        let result_label = StaticText::builder(&panel).with_label("Result:").build();
        sizer.add(&result_label, 0, SizerFlag::All, 2);
        let result = StaticText::builder(&panel)
            .with_label("enter a value and choose Convert")
            .build();
        result.set_accessibility_label("Conversion result");
        sizer.add(&result, 0, SizerFlag::All, 2);

        // Wire the conversion with shared access to the fields.
        *fields.bits_input.borrow_mut() = Some(bits_input);
        *fields.float_input.borrow_mut() = Some(float_input);
        *fields.result.borrow_mut() = Some(result);
        let f = fields.clone();
        convert.on_click(move |_| {
            let bits_text = f
                .bits_input
                .borrow()
                .as_ref()
                .map(|t| t.get_value())
                .unwrap_or_default();
            let float_text = f
                .float_input
                .borrow()
                .as_ref()
                .map(|t| t.get_value())
                .unwrap_or_default();

            let mut lines: Vec<String> = Vec::new();
            let bits_clean = bits_text.trim().trim_start_matches("0x");
            if let Ok(bits) = u32::from_str_radix(bits_clean, 16) {
                let value = f32::from_bits(bits);
                lines.push(format!(
                    "Bits {bits_clean} as float: {value} (double precision: {})",
                    value as f64
                ));
                lines.push(format!(
                    "Sign {}, exponent {}, mantissa 0x{:07x}",
                    bits >> 31,
                    (bits >> 23) & 0xff,
                    bits & 0x7f_ffff
                ));
            } else if !bits_clean.is_empty() {
                lines.push(format!("'{bits_clean}' is not eight hexadecimal digits"));
            }
            if let Ok(value) = float_text.trim().parse::<f32>() {
                let bits = value.to_bits();
                lines.push(format!("Float {value} as bits: 0x{bits:08x}"));
            } else if !float_text.trim().is_empty() {
                lines.push(format!("'{}' is not a float value", float_text.trim()));
            }

            if let Some(result) = f.result.borrow().as_ref() {
                result.set_label(&if lines.is_empty() {
                    "enter a value and choose Convert".to_string()
                } else {
                    lines.join("; ")
                });
            }
        });

        panel.set_sizer(sizer, true);
        frame.show(true);
        FloatRepTool { _frame: frame }
    }
}
