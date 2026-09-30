//! Timer Tool: arms and clears the machine's instruction-count timer, the
//! driving source for interrupt-handler coursework, and shows the wait state.

use crate::bridge::Cmd;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::Sender;
use wxdragon::prelude::*;

pub struct TimerTool {
    _frame: Frame,
}

impl TimerTool {
    pub fn open(cmd_tx: Sender<Cmd>, waiting: Rc<RefCell<bool>>) -> Self {
        let frame = Frame::builder()
            .with_title("Timer Tool")
            .with_size(Size::new(560, 260))
            .build();
        frame.set_accessibility_label("Timer Tool");

        let panel = Panel::builder(&frame).build();
        panel.set_accessibility_label("Timer settings");
        let sizer = BoxSizer::builder(Orientation::Vertical).build();

        let interval_label = StaticText::builder(&panel)
            .with_label("Interrupt interval (instructions):")
            .build();
        sizer.add(&interval_label, 0, SizerFlag::All, 2);
        let interval = TextCtrl::builder(&panel).build();
        interval.set_value("100000");
        interval.set_accessibility_label("Timer interval in instructions");
        interval.set_accessibility_description(
            "How many executed instructions pass between timer interrupts",
        );
        sizer.add(&interval, 0, SizerFlag::Expand | SizerFlag::All, 2);

        let buttons = BoxSizer::builder(Orientation::Horizontal).build();
        let arm = Button::builder(&panel).with_label("Arm timer").build();
        arm.set_accessibility_label("Arm timer");
        arm.set_accessibility_description("Raises a timer interrupt every interval instructions");
        buttons.add(&arm, 0, SizerFlag::All, 4);
        let clear = Button::builder(&panel).with_label("Clear timer").build();
        clear.set_accessibility_label("Clear timer");
        clear.set_accessibility_description("Disarms the timer");
        buttons.add(&clear, 0, SizerFlag::All, 4);
        sizer.add_sizer(&buttons, 0, SizerFlag::Expand, 0);

        let status = StaticText::builder(&panel)
            .with_label("Timer not armed.")
            .build();
        status.set_accessibility_label("Timer status");
        sizer.add(&status, 0, SizerFlag::All, 2);

        let hint = StaticText::builder(&panel)
            .with_label("Your program enables interrupts (set ustatus bit 0, point utvec at a handler), then waits; each tick raises cause 16.")
            .build();
        sizer.add(&hint, 0, SizerFlag::All, 4);

        panel.set_sizer(sizer, true);

        let status_for_arm = status;
        let interval_for_arm = interval;
        let cmd_for_arm = cmd_tx.clone();
        arm.on_click(move |_| {
            let value: u64 = interval_for_arm.get_value().trim().parse().unwrap_or(0);
            if value == 0 {
                status_for_arm.set_label("Enter a nonzero instruction count.");
            } else {
                cmd_for_arm.send(Cmd::ArmTimer(value)).ok();
                status_for_arm.set_label(&format!(
                    "Timer armed: an interrupt every {value} instructions."
                ));
            }
        });
        let status_for_clear = status;
        clear.on_click(move |_| {
            cmd_tx.send(Cmd::ClearTimer).ok();
            status_for_clear.set_label("Timer not armed.");
        });

        // Reflect the machine's wfi wait state so a parked program is visible.
        {
            let status_for_wait = status_for_clear;
            frame.on_idle(move |idle| {
                if let WindowEventData::Idle(idle) = idle {
                    idle.request_more(true);
                }
                let waiting = *waiting.borrow();
                let label = status_for_wait.get_label();
                if waiting && !label.contains("waiting") {
                    status_for_wait.set_label("Program is waiting for an interrupt (wfi).");
                }
            });
        }

        frame.show(true);
        TimerTool { _frame: frame }
    }
}
