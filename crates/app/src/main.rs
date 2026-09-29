//! AsAccess GUI skeleton (wxDragon accessibility spike, Phase 0 / Spike 1).
//!
//! Builds one main frame exercising every widget class the IDE is expected to
//! need, each with an explicit accessible name, so a UIA probe can measure what
//! screen readers will actually see:
//! - menu bar (File / Run / Help) with accelerators
//! - button row: Assemble, Run, Step, Stop
//! - horizontal splitter: StyledTextCtrl editor | vertical splitter with
//!   register table (wxGrid tab + virtual wxListCtrl tab) over a Run I/O pane
//! - status bar with two fields
//! - Settings dialog (labels associated via sizers) and a Keyboard Shortcuts
//!   dialog (F1 accelerator) listing shortcuts in a report-mode list control.

#[cfg(target_os = "windows")]
use wxdragon::accessible::AccRole;
use wxdragon::prelude::*;

// Menu item ids.
const ID_NEW: Id = 1001;
const ID_OPEN: Id = 1002;
const ID_SAVE: Id = 1003;
const ID_SETTINGS: Id = 1004;
const ID_EXIT: Id = 1005;
const ID_RUN_ASSEMBLE: Id = 2001;
const ID_RUN_RUN: Id = 2002;
const ID_RUN_STEP: Id = 2003;
const ID_RUN_STOP: Id = 2004;
const ID_SHORTCUTS: Id = 3001;
const ID_ABOUT: Id = 3002;

/// Register rows shared by the grid and the virtual list tabs.
const REGISTER_ROWS: [(&str, &str, &str, &str); 8] = [
    ("x0", "zero", "0x00000000", "no"),
    ("x1", "ra", "0x00000000", "no"),
    ("x2", "sp", "0x7ffffc00", "no"),
    ("x3", "gp", "0x00000000", "no"),
    ("x4", "tp", "0x00000000", "no"),
    ("x5", "t0", "0x00000005", "yes"),
    ("x6", "t1", "0x00000007", "yes"),
    ("x7", "t2", "0x0000000c", "yes"),
];

const SAMPLE_RISCV: &str = "\
# add.s - add two numbers
    .text
    .globl main
main:
    li   t0, 5        # load immediate 5
    li   t1, 7        # load immediate 7
    add  t2, t0, t1   # t2 = t0 + t1
    ecall             # exit to host
";

fn main() {
    // Avoid manifest-check warnings when running a plain debug exe on Windows.
    SystemOptions::set_option_by_int("msw.no-manifest-check", 1);

    let _ = wxdragon::main(|_| {
        let frame = Frame::builder()
            .with_title("AsAccess - RISC-V Assembly IDE")
            .with_size(Size::new(1200, 800))
            .build();
        frame.set_accessibility_label("AsAccess main window");
        #[cfg(target_os = "windows")]
        frame.set_accessibility_role(AccRole::Application);

        // --- Status bar (two fields) ---
        let status_bar = StatusBar::builder(&frame)
            .with_fields_count(2)
            .with_status_widths(vec![-1, 260])
            .add_initial_text(0, "Ready")
            .add_initial_text(1, "No program loaded")
            .build();
        status_bar.set_accessibility_label("Status bar");
        frame.set_existing_status_bar(Some(&status_bar));

        // --- Menu bar ---
        frame.set_menu_bar(build_menu_bar());

        // --- Main panel with button row + splitter ---
        let panel = Panel::builder(&frame).build();
        let main_sizer = BoxSizer::builder(Orientation::Vertical).build();

        let button_row = BoxSizer::builder(Orientation::Horizontal).build();
        for (label, help) in [
            ("Assemble", "Assemble the current source file"),
            ("Run", "Run the assembled program"),
            ("Step", "Execute one instruction"),
            ("Stop", "Stop the running program"),
        ] {
            let btn = Button::builder(&panel).with_label(label).build();
            btn.set_accessibility_label(label);
            btn.set_accessibility_description(help);
            #[cfg(target_os = "windows")]
            btn.set_accessibility_role(AccRole::PushButton);
            let sb = status_bar;
            let text = format!("{label} pressed");
            btn.on_click(move |_| {
                sb.set_status_text(&text, 0);
            });
            button_row.add(&btn, 0, SizerFlag::All, 4);
        }
        main_sizer.add_sizer(&button_row, 0, SizerFlag::Expand, 0);

        // Horizontal splitter: editor | right side.
        let h_splitter = SplitterWindow::builder(&panel)
            .with_style(SplitterWindowStyle::LiveUpdate | SplitterWindowStyle::Default)
            .build();

        let editor_panel = Panel::builder(&h_splitter).build();
        let editor_sizer = BoxSizer::builder(Orientation::Vertical).build();
        let editor = build_editor(&editor_panel);
        editor_sizer.add(&editor, 1, SizerFlag::Expand | SizerFlag::All, 2);
        editor_panel.set_sizer(editor_sizer, true);

        let right_splitter = SplitterWindow::builder(&h_splitter)
            .with_style(SplitterWindowStyle::LiveUpdate | SplitterWindowStyle::Default)
            .build();

        let notebook = build_register_views(&right_splitter);

        let io_panel = build_io_panel(&right_splitter);

        let _ = right_splitter.split_horizontally(&notebook, &io_panel, 380);
        let _ = h_splitter.split_vertically(&editor_panel, &right_splitter, 620);

        main_sizer.add(&h_splitter, 1, SizerFlag::Expand, 0);
        panel.set_sizer(main_sizer, true);

        // --- Menu events (dialogs opened from menu items) ---
        bind_menu_events(&frame, &status_bar);

        frame.show(true);
        frame.centre();
    });
}

fn build_menu_bar() -> MenuBar {
    let file_menu = Menu::builder()
        .append_item(ID_NEW, "&New\tCtrl+N", "New source file")
        .append_item(ID_OPEN, "&Open...\tCtrl+O", "Open a source file")
        .append_item(ID_SAVE, "&Save\tCtrl+S", "Save the current source file")
        .append_separator()
        .append_item(ID_SETTINGS, "S&ettings...", "Open the settings dialog")
        .append_separator()
        .append_item(ID_EXIT, "E&xit\tAlt+F4", "Exit AsAccess")
        .build();

    let run_menu = Menu::builder()
        .append_item(ID_RUN_ASSEMBLE, "&Assemble\tF7", "Assemble the current source file")
        .append_item(ID_RUN_RUN, "&Run\tF5", "Run the assembled program")
        .append_item(ID_RUN_STEP, "St&ep\tF10", "Execute one instruction")
        .append_item(ID_RUN_STOP, "Sto&p\tShift+F5", "Stop the running program")
        .build();

    let help_menu = Menu::builder()
        .append_item(ID_SHORTCUTS, "&Keyboard Shortcuts\tF1", "Show keyboard shortcuts")
        .append_separator()
        .append_item(ID_ABOUT, "&About AsAccess", "About this application")
        .build();

    MenuBar::builder()
        .append(file_menu, "&File")
        .append(run_menu, "&Run")
        .append(help_menu, "&Help")
        .build()
}

fn build_editor(parent: &Panel) -> StyledTextCtrl {
    let editor = StyledTextCtrl::builder(parent).build();
    editor.set_text(SAMPLE_RISCV);
    // Line numbers in margin 1.
    editor.set_margin_type(1, MarginType::Number);
    editor.set_margin_width(1, 40);
    editor.set_read_only(false);
    editor.set_accessibility_label("Source editor");
    editor.set_accessibility_description("RISC-V assembly source editor");
    editor.set_accessibility_value(SAMPLE_RISCV);
    #[cfg(target_os = "windows")]
    editor.set_accessibility_role(AccRole::Document);
    editor
}

fn build_register_views(parent: &SplitterWindow) -> Notebook {
    let notebook = Notebook::builder(parent).build();
    notebook.set_accessibility_label("Register views");
    #[cfg(target_os = "windows")]
    notebook.set_accessibility_role(AccRole::PageTabList);

    // Tab 1: wxGrid register table.
    let grid_panel = Panel::builder(&notebook).build();
    let grid = Grid::builder(&grid_panel).build();
    grid.create_grid(REGISTER_ROWS.len() as i32, 4, GridSelectionMode::Cells);
    for (col, name) in ["Reg", "Name", "Value", "Changed"].iter().enumerate() {
        grid.set_col_label_value(col as i32, name);
    }
    for (row, (reg, name, value, changed)) in REGISTER_ROWS.iter().enumerate() {
        grid.set_row_label_value(row as i32, &(row + 1).to_string());
        grid.set_cell_value(row as i32, 0, reg);
        grid.set_cell_value(row as i32, 1, name);
        grid.set_cell_value(row as i32, 2, value);
        grid.set_cell_value(row as i32, 3, changed);
    }
    grid.set_accessibility_label("Register values grid table");
    #[cfg(target_os = "windows")]
    grid.set_accessibility_role(AccRole::Table);
    let grid_sizer = BoxSizer::builder(Orientation::Vertical).build();
    grid_sizer.add(&grid, 1, SizerFlag::Expand | SizerFlag::All, 2);
    grid_panel.set_sizer(grid_sizer, true);
    notebook.add_page(&grid_panel, "Registers (Grid)", true, None);

    // Tab 2: virtual wxListCtrl in report mode with the same columns.
    let list_panel = Panel::builder(&notebook).build();
    let list = ListCtrl::builder(&list_panel)
        .with_style(
            ListCtrlStyle::Report | ListCtrlStyle::Virtual | ListCtrlStyle::SingleSel,
        )
        .build();
    list.insert_column(0, "Reg", ListColumnFormat::Left, 70);
    list.insert_column(1, "Name", ListColumnFormat::Left, 110);
    list.insert_column(2, "Value", ListColumnFormat::Left, 140);
    list.insert_column(3, "Changed", ListColumnFormat::Left, 90);
    list.set_item_count(REGISTER_ROWS.len() as i64);
    let rows: Vec<[String; 4]> = REGISTER_ROWS
        .iter()
        .map(|(r, n, v, c)| [r.to_string(), n.to_string(), v.to_string(), c.to_string()])
        .collect();
    assert!(list.set_virtual_text_callback(move |item, col| {
        rows.get(item as usize)
            .and_then(|row| row.get(col as usize))
            .cloned()
            .unwrap_or_default()
    }));
    list.set_accessibility_label("Register values list table");
    #[cfg(target_os = "windows")]
    list.set_accessibility_role(AccRole::List);
    let list_sizer = BoxSizer::builder(Orientation::Vertical).build();
    list_sizer.add(&list, 1, SizerFlag::Expand | SizerFlag::All, 2);
    list_panel.set_sizer(list_sizer, true);
    notebook.add_page(&list_panel, "Registers (List)", false, None);

    notebook
}

fn build_io_panel(parent: &SplitterWindow) -> Panel {
    let io_panel = Panel::builder(parent).build();
    let sizer = BoxSizer::builder(Orientation::Vertical).build();

    let io_label = StaticText::builder(&io_panel).with_label("Run I/O").build();
    sizer.add(&io_label, 0, SizerFlag::All, 2);

    let io_output = TextCtrl::builder(&io_panel)
        .with_style(TextCtrlStyle::MultiLine | TextCtrlStyle::ReadOnly)
        .build();
    io_output.set_value("program output appears here\n");
    io_output.set_accessibility_label("Run I/O output");
    io_output.set_accessibility_description("Read-only output of the running program");
    #[cfg(target_os = "windows")]
    io_output.set_accessibility_role(AccRole::StaticText);
    sizer.add(&io_output, 1, SizerFlag::Expand | SizerFlag::All, 2);

    let input_label = StaticText::builder(&io_panel)
        .with_label("Program input")
        .build();
    sizer.add(&input_label, 0, SizerFlag::All, 2);

    let input = TextCtrl::builder(&io_panel).build();
    input.set_accessibility_label("Program input");
    input.set_accessibility_description("Text forwarded to the program on each read");
    #[cfg(target_os = "windows")]
    input.set_accessibility_role(AccRole::Text);
    sizer.add(&input, 0, SizerFlag::Expand | SizerFlag::All, 2);

    io_panel.set_sizer(sizer, true);
    io_panel
}

fn bind_menu_events(frame: &Frame, status_bar: &StatusBar) {
    let sb = *status_bar;
    let fr = *frame;
    frame.on_menu(move |event| {
        match event.get_id() {
            ID_SETTINGS => show_settings_dialog(&fr),
            ID_SHORTCUTS => show_shortcuts_dialog(&fr),
            ID_ABOUT => sb.set_status_text("AsAccess accessibility spike build", 0),
            id => {
                let text = match id {
                    ID_NEW => "New file",
                    ID_OPEN => "Open file",
                    ID_SAVE => "Save file",
                    ID_EXIT => "Exit",
                    ID_RUN_ASSEMBLE => "Assemble",
                    ID_RUN_RUN => "Run",
                    ID_RUN_STEP => "Step",
                    ID_RUN_STOP => "Stop",
                    _ => "",
                };
                if !text.is_empty() {
                    sb.set_status_text(text, 0);
                }
            }
        }
    });
}

// --- Dialogs ---------------------------------------------------------------

/// Settings dialog: three labeled controls with static-text labels laid out in
/// sizers next to the inputs, and matching accessible names on the inputs.
fn show_settings_dialog(frame: &Frame) {
    let dialog = Dialog::builder(frame, "Settings")
        .with_style(DialogStyle::DefaultDialogStyle | DialogStyle::ResizeBorder)
        .with_size(420, 240)
        .build();
    dialog.set_accessibility_label("Settings dialog");

    let panel = Panel::builder(&dialog).build();
    let sizer = BoxSizer::builder(Orientation::Vertical).build();

    // Verbosity choice.
    let verbosity_label = StaticText::builder(&panel).with_label("Verbosity:").build();
    let verbosity = Choice::builder(&panel)
        .with_choices(vec![
            "Quiet".to_string(),
            "Normal".to_string(),
            "Verbose".to_string(),
        ])
        .with_selection(Some(1))
        .build();
    verbosity.set_accessibility_label("Verbosity");
    verbosity.set_accessibility_description("How much narration detail to speak");
    let verbosity_row = BoxSizer::builder(Orientation::Horizontal).build();
    verbosity_row.add(&verbosity_label, 0, SizerFlag::AlignCenterVertical | SizerFlag::All, 4);
    verbosity_row.add(&verbosity, 1, SizerFlag::Expand | SizerFlag::All, 4);
    sizer.add_sizer(&verbosity_row, 0, SizerFlag::Expand, 0);

    // Editor fallback checkbox.
    let fallback = CheckBox::builder(&panel)
        .with_label("Use editor fallback when narration is unavailable")
        .with_value(true)
        .build();
    fallback.set_accessibility_label("Use editor fallback");
    fallback.set_accessibility_description(
        "Announce editor state through the editor control if speech is unavailable",
    );
    sizer.add(&fallback, 0, SizerFlag::All, 4);

    // Theme choice.
    let theme_label = StaticText::builder(&panel).with_label("Theme:").build();
    let theme = Choice::builder(&panel)
        .with_choices(vec![
            "System".to_string(),
            "Light".to_string(),
            "Dark".to_string(),
        ])
        .with_selection(Some(0))
        .build();
    theme.set_accessibility_label("Theme");
    theme.set_accessibility_description("Interface color theme");
    let theme_row = BoxSizer::builder(Orientation::Horizontal).build();
    theme_row.add(&theme_label, 0, SizerFlag::AlignCenterVertical | SizerFlag::All, 4);
    theme_row.add(&theme, 1, SizerFlag::Expand | SizerFlag::All, 4);
    sizer.add_sizer(&theme_row, 0, SizerFlag::Expand, 0);

    // Close button.
    let close_btn = Button::builder(&panel).with_label("Close").build();
    close_btn.set_accessibility_label("Close settings");
    let dlg = dialog;
    close_btn.on_click(move |_| {
        dlg.end_modal(ID_OK);
    });
    sizer.add(&close_btn, 0, SizerFlag::AlignCenterHorizontal | SizerFlag::All, 8);

    panel.set_sizer(sizer, true);
    let dialog_sizer = BoxSizer::builder(Orientation::Vertical).build();
    dialog_sizer.add(&panel, 1, SizerFlag::Expand, 0);
    dialog.set_sizer(dialog_sizer, true);

    let _ = dialog.show_modal();
    dialog.destroy();
}

/// Keyboard Shortcuts dialog: report-mode list control of action + shortcut.
fn show_shortcuts_dialog(frame: &Frame) {
    let dialog = Dialog::builder(frame, "Keyboard Shortcuts")
        .with_style(DialogStyle::DefaultDialogStyle | DialogStyle::ResizeBorder)
        .with_size(460, 320)
        .build();
    dialog.set_accessibility_label("Keyboard shortcuts dialog");

    let panel = Panel::builder(&dialog).build();
    let sizer = BoxSizer::builder(Orientation::Vertical).build();

    let shortcuts: [(&str, &str); 7] = [
        ("Assemble", "F7"),
        ("Run program", "F5"),
        ("Step instruction", "F10"),
        ("Stop program", "Shift+F5"),
        ("Open file", "Ctrl+O"),
        ("Save file", "Ctrl+S"),
        ("Show this dialog", "F1"),
    ];

    let list = ListCtrl::builder(&panel)
        .with_style(ListCtrlStyle::Report | ListCtrlStyle::SingleSel)
        .build();
    list.insert_column(0, "Action", ListColumnFormat::Left, 240);
    list.insert_column(1, "Shortcut", ListColumnFormat::Left, 140);
    for (i, (action, key)) in shortcuts.iter().enumerate() {
        let idx = list.insert_item(i as i64, action, None);
        list.set_item_text_by_column(idx as i64, 1, key);
    }
    list.set_accessibility_label("Keyboard shortcuts list");
    let sizer_list = &list;
    sizer.add(sizer_list, 1, SizerFlag::Expand | SizerFlag::All, 4);

    let close_btn = Button::builder(&panel).with_label("Close").build();
    close_btn.set_accessibility_label("Close keyboard shortcuts");
    let dlg = dialog;
    close_btn.on_click(move |_| {
        dlg.end_modal(ID_OK);
    });
    sizer.add(&close_btn, 0, SizerFlag::AlignCenterHorizontal | SizerFlag::All, 6);

    panel.set_sizer(sizer, true);
    let dialog_sizer = BoxSizer::builder(Orientation::Vertical).build();
    dialog_sizer.add(&panel, 1, SizerFlag::Expand, 0);
    dialog.set_sizer(dialog_sizer, true);

    let _ = dialog.show_modal();
    dialog.destroy();
}
