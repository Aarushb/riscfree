//! Instruction Decode: paste or type a 32-bit instruction word and get the
//! per-field breakdown plus a natural-language reading, so machine code is
//! perceivable without decoding bit positions by hand.

use rvasm::DecodedFields;
use wxdragon::prelude::*;

pub struct DecodeTool {
    _frame: Frame,
}

fn reg_name(index: u32) -> String {
    const ABI: &[&str] = &[
        "zero", "ra", "sp", "gp", "tp", "t0", "t1", "t2", "s0", "s1", "a0", "a1", "a2", "a3", "a4",
        "a5", "a6", "a7", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11", "t3", "t4",
        "t5", "t6",
    ];
    format!(
        "x{index} ({})",
        ABI.get(index as usize).copied().unwrap_or("?")
    )
}

fn describe(fields: &DecodedFields) -> String {
    let mnemonic = rvasm::opcode_representative(fields.opcode)
        .map(|n| n.to_string())
        .unwrap_or_else(|| "unknown instruction".to_string());
    let imm = fields.immediate;
    match fields.format {
        "R" => format!(
            "{mnemonic}: rd {}, rs1 {}, rs2 {}, funct3 {funct3}, funct7 {funct7:#04x}",
            reg_name(fields.rd),
            reg_name(fields.rs1),
            reg_name(fields.rs2),
            funct3 = fields.funct3,
            funct7 = fields.funct7
        ),
        "I" => format!(
            "{mnemonic}: rd {}, rs1 {}, immediate {imm} (0x{imm:x}), funct3 {funct3}",
            reg_name(fields.rd),
            reg_name(fields.rs1),
            funct3 = fields.funct3
        ),
        "S" => format!(
            "{mnemonic}: store rs2 {} to {} plus rs1 {}, funct3 {funct3}",
            reg_name(fields.rs2),
            imm,
            reg_name(fields.rs1),
            funct3 = fields.funct3
        ),
        "B" => format!(
            "{mnemonic}: branch if rs1 {} versus rs2 {} matches funct3 {funct3}, jumping {imm} bytes",
            reg_name(fields.rs1),
            reg_name(fields.rs2),
            funct3 = fields.funct3
        ),
        "U" => format!(
            "{mnemonic}: rd {} gets upper immediate {imm:#010x}",
            reg_name(fields.rd)
        ),
        "J" => format!(
            "{mnemonic}: jump {imm} bytes, link into rd {}",
            reg_name(fields.rd)
        ),
        _ => format!("{mnemonic}: opcode 0x{:02x}", fields.opcode),
    }
}

impl DecodeTool {
    pub fn open() -> Self {
        let frame = Frame::builder()
            .with_title("Instruction Decode")
            .with_size(Size::new(680, 300))
            .build();
        frame.set_accessibility_label("Instruction Decode tool");

        let panel = Panel::builder(&frame).build();
        panel.set_accessibility_label("Instruction Decode fields");
        let sizer = BoxSizer::builder(Orientation::Vertical).build();

        let input_label = StaticText::builder(&panel)
            .with_label("Instruction word (8 hex digits):")
            .build();
        sizer.add(&input_label, 0, SizerFlag::All, 2);
        let input = TextCtrl::builder(&panel).build();
        input.set_value("00530533"); // add a2, a0, a1? actually add t2? decode and see
        input.set_accessibility_label("Instruction word");
        input.set_accessibility_description("Thirty-two bits as eight hexadecimal digits");
        sizer.add(&input, 0, SizerFlag::Expand | SizerFlag::All, 2);

        let decode_btn = Button::builder(&panel).with_label("Decode").build();
        decode_btn.set_accessibility_label("Decode instruction");
        sizer.add(&decode_btn, 0, SizerFlag::All, 4);

        let fields_label = StaticText::builder(&panel).with_label("Fields:").build();
        sizer.add(&fields_label, 0, SizerFlag::All, 2);
        let fields_text = StaticText::builder(&panel)
            .with_label("Choose Decode to break an instruction into its fields.")
            .build();
        fields_text.set_accessibility_label("Decoded fields");
        sizer.add(&fields_text, 0, SizerFlag::All, 2);

        let reading_label = StaticText::builder(&panel).with_label("Reading:").build();
        sizer.add(&reading_label, 0, SizerFlag::All, 2);
        let reading = StaticText::builder(&panel).with_label("").build();
        reading.set_accessibility_label("Instruction reading");
        reading.set_accessibility_description("The instruction read aloud in field order");
        sizer.add(&reading, 0, SizerFlag::All, 2);

        panel.set_sizer(sizer, true);

        let input_handle = input;
        let fields_handle = fields_text;
        let reading_handle = reading;
        decode_btn.on_click(move |_| {
            let text = input_handle.get_value().trim().trim_start_matches("0x").to_string();
            match u32::from_str_radix(&text, 16) {
                Ok(word) => {
                    let fields = rvasm::decode_fields(word);
                    let field_line = format!(
                        "opcode 0x{:02x} ({}), format {}, rd {}, funct3 {}, rs1 {}, rs2 {}, funct7 0x{:02x}, immediate {}",
                        fields.opcode,
                        rvasm::opcode_representative(fields.opcode).unwrap_or("unknown"),
                        fields.format,
                        reg_name(fields.rd),
                        fields.funct3,
                        reg_name(fields.rs1),
                        reg_name(fields.rs2),
                        fields.funct7,
                        fields.immediate
                    );
                    fields_handle.set_label(&field_line);
                    reading_handle.set_label(&describe(&fields));
                }
                Err(_) => {
                    fields_handle.set_label(&format!("'{text}' is not eight hexadecimal digits"));
                    reading_handle.set_label("");
                }
            }
        });

        frame.show(true);
        DecodeTool { _frame: frame }
    }
}
