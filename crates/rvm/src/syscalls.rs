//! The teaching syscall layer (RARS-compatible numbers and register
//! conventions). Console dialogs render inline: prompts and messages go
//! through `write_output` instead of OS windows, so the GUI can present them
//! its own way later; unknown numbers halt with the same kind of error RARS
//! produces.

use crate::{fp, Change, Event, Halt, Machine, StepOutcome};

pub(crate) fn dispatch(m: &mut Machine, events: &mut Vec<Event>, pc_before: u32) -> StepOutcome {
    let code = m.regs[17] as u32; // a7
    let mut changes = Vec::new();
    let halt = run_syscall(m, code, &mut changes);
    match halt {
        Ok(()) => StepOutcome {
            executed: true,
            pc_before,
            events: std::mem::take(events),
            changes,
        },
        Err(h) => {
            m.terminated = Some(h.clone());
            let mut evs = std::mem::take(events);
            evs.push(Event::Halted(h));
            StepOutcome { executed: false, pc_before, events: evs, changes: Vec::new() }
        }
    }
}

/// Runs the syscall; `Err` carries the halt (exit or error).
fn run_syscall(m: &mut Machine, code: u32, changes: &mut Vec<Change>) -> Result<(), Halt> {
    let a0 = m.regs[10];
    let a1 = m.regs[11];
    let a2 = m.regs[12];
    match code {
        1 => {
            // PrintInt
            let text = (a0 as u32 as i32).to_string();
            m.host_mut().write_output(&text);
        }
        4 => {
            // PrintString: NUL-terminated at a0
            let s = read_cstring(m, a0 as u32)?;
            m.host_mut().write_output(&s);
        }
        5 => {
            // ReadInt
            let line = m.host_mut().read_line().unwrap_or_default();
            let v = line.trim().parse::<i32>().unwrap_or(0);
            m.write_reg(10, v as i64 as u64, changes);
        }
        8 => {
            // ReadString (a0 buf, a1 maxlen): up to maxlen-1 bytes + NUL
            let line = m.host_mut().read_line().unwrap_or_default();
            let maxlen = a1 as u32 as usize;
            let bytes = line.as_bytes();
            let n = maxlen.saturating_sub(1).min(bytes.len());
            let mut out = bytes[..n].to_vec();
            out.push(0);
            m.mem.write_bytes(a0 as u32, &out);
        }
        10 => return Err(Halt::Exit { code: 0 }),
        11 => {
            // PrintChar
            let text = (a0 as u8 as char).to_string();
            m.host_mut().write_output(&text);
        }
        12 => {
            // ReadChar
            let c = m.host_mut().read_char().map(u64::from).unwrap_or(u64::from(u32::MAX));
            m.write_reg(10, c, changes);
        }
        50 => {
            // ConfirmDialog: a0 = message, a0 <- 1 yes / 0 no
            let msg = read_cstring(m, a0 as u32)?;
            let yes = m.host_mut().confirm(&msg);
            m.write_reg(10, u64::from(yes), changes);
        }
        51 => {
            // InputDialogInt: invalid input reads 0 (RARS re-prompts; we do not)
            let prompt = read_cstring(m, a0 as u32)?;
            m.host_mut().write_output(&prompt);
            let line = m.host_mut().read_line().unwrap_or_default();
            let v = line.trim().parse::<i32>().unwrap_or(0);
            m.write_reg(10, v as i64 as u64, changes);
        }
        52 => {
            // InputDialogFloat: result in fa0
            let prompt = read_cstring(m, a0 as u32)?;
            m.host_mut().write_output(&prompt);
            let line = m.host_mut().read_line().unwrap_or_default();
            let v = line.trim().parse::<f32>().unwrap_or(0.0);
            // NaN-box so FP instructions read the value back correctly.
            m.fregs[10] = fp::box_single(v.to_bits());
        }
        53 => {
            // InputDialogDouble: result in fa0
            let prompt = read_cstring(m, a0 as u32)?;
            m.host_mut().write_output(&prompt);
            let line = m.host_mut().read_line().unwrap_or_default();
            let v = line.trim().parse::<f64>().unwrap_or(0.0);
            m.fregs[10] = v.to_bits();
        }
        54 => {
            // InputDialogString (a0 prompt, a1 buf, a2 maxlen): writes the
            // line plus NUL like ReadString; a1 <- RARS's status (0 OK, -2
            // cancel/EOF, -3 empty, -4 too long, still storing a truncation).
            let prompt = read_cstring(m, a0 as u32)?;
            m.host_mut().write_output(&prompt);
            let maxlen = a2 as u32 as usize;
            let keep = maxlen.saturating_sub(1);
            let (mut out, status): (Vec<u8>, i64) = match m.host_mut().read_line() {
                None => (Vec::new(), -2),
                Some(line) if line.is_empty() => (Vec::new(), -3),
                Some(line) => {
                    let bytes = line.as_bytes();
                    if bytes.len() > keep {
                        (bytes[..keep].to_vec(), -4)
                    } else {
                        (bytes.to_vec(), 0)
                    }
                }
            };
            out.push(0);
            m.mem.write_bytes(a1 as u32, &out);
            m.write_reg(11, status as u64, changes);
        }
        55 => {
            // MessageDialog (a1 type = icon choice; inline console ignores it)
            let msg = read_cstring(m, a0 as u32)?;
            m.host_mut().write_output(&format!("{msg}\n"));
        }
        56 => {
            // MessageDialogInt
            m.host_mut().write_output(&format!("{}\n", a0 as u32 as i32));
        }
        58 => {
            // MessageDialogDouble
            let d = f64::from_bits(m.fregs[10]);
            m.host_mut().write_output(&format!("{d}\n"));
        }
        59 => {
            // MessageDialogString
            let msg = read_cstring(m, a0 as u32)?;
            m.host_mut().write_output(&format!("{msg}\n"));
        }
        60 => {
            // MessageDialogFloat
            let f = f32::from_bits(m.fregs[10] as u32);
            m.host_mut().write_output(&format!("{f}\n"));
        }
        57 => {
            // Close: RARS puts 0 in a0 on success, -1 on a bad fd
            let n = m.host_mut().file_close(a0 as u32 as i32);
            m.write_reg(10, n as i64 as u64, changes);
        }
        62 => {
            // LSeek (whence 0 = start, 1 = current, 2 = end)
            let n = m.host_mut().file_seek(a0 as u32 as i32, a1 as u32 as i32, a2 as u32 as i32);
            m.write_reg(10, n as i64 as u64, changes);
        }
        63 => {
            // Read (a0 fd, a1 buf, a2 len) -> bytes read or -1
            let mut buf = vec![0u8; a2 as u32 as usize];
            let n = m.host_mut().file_read(a0 as u32 as i32, &mut buf);
            if n > 0 {
                m.mem.write_bytes(a1 as u32, &buf[..n as usize]);
            }
            m.write_reg(10, n as i64 as u64, changes);
        }
        64 => {
            // Write (a0 fd, a1 buf, a2 len) -> bytes written
            let mut buf = vec![0u8; a2 as u32 as usize];
            let _ = m.mem.read_bytes(a1 as u32, &mut buf);
            let n = m.host_mut().file_write(a0 as u32 as i32, &buf);
            m.write_reg(10, n as i64 as u64, changes);
        }
        1024 => {
            // Open (a0 path, a1 flags: 0 read, 1 write-create-truncate,
            // 9 write-append) -> fd or -1
            let path = read_cstring(m, a0 as u32)?;
            let flags = a1 as u32;
            let fd = m.host_mut().file_open(&path, flags != 0, flags == 9);
            m.write_reg(10, fd as i64 as u64, changes);
        }
        17 => {
            // GetCWD (a0 buf, a1 len)
            let cwd = m.host_mut().get_cwd();
            let bytes = cwd.as_bytes();
            let n = (a1 as u32 as usize).saturating_sub(1).min(bytes.len());
            let mut out = bytes[..n].to_vec();
            out.push(0);
            m.mem.write_bytes(a0 as u32, &out);
            m.write_reg(10, 0, changes);
        }
        30 => {
            // Time: a0 = low 32, a1 = high 32 of ms since epoch
            let ms = m.host_mut().time_ms();
            m.write_reg(10, (ms as u32) as u64, changes);
            m.write_reg(11, ms >> 32, changes);
        }
        32 => {
            // Sleep
            m.host_mut().sleep_ms(a0 as u32 as u64);
        }
        34 => {
            // PrintIntHex
            let text = format!("0x{:08x}", a0 as u32);
            m.host_mut().write_output(&text);
        }
        35 => {
            // PrintIntBinary
            let text = format!("0b{:032b}", a0 as u32);
            m.host_mut().write_output(&text);
        }
        36 => {
            // PrintIntUnsigned
            let text = (a0 as u32).to_string();
            m.host_mut().write_output(&text);
        }
        40 => {
            // RandSeed (a0 idx, a1 seed)
            m.host_mut().random_seed(a0 as u32, a1);
        }
        41 => {
            // RandInt
            let v = m.host_mut().random_int(a0 as u32);
            m.write_reg(10, v as i64 as u64, changes);
        }
        42 => {
            // RandIntRange (a0 idx, a1 bound)
            let v = m.host_mut().random_range(a0 as u32, a1 as u32);
            m.write_reg(10, v as i64 as u64, changes);
        }
        43 => {
            // RandFloat → fa0
            let f = m.host_mut().random_float(a0 as u32);
            m.fregs[10] = fp::box_single(f.to_bits());
        }
        44 => {
            // RandDouble → fa0
            let d = m.host_mut().random_double(a0 as u32);
            m.fregs[10] = d.to_bits();
        }
        93 => return Err(Halt::Exit { code: a0 as u32 as i32 }),
        other => {
            return Err(Halt::Error {
                message: format!("service {other} is not available in this build"),
            })
        }
    }
    Ok(())
}

fn read_cstring(m: &Machine, addr: u32) -> Result<String, Halt> {
    let mut bytes = Vec::new();
    let mut a = addr;
    // RARS strings are NUL-terminated; bound the scan so a missing NUL
    // cannot spin forever.
    let limit = a + 1_048_576;
    let mut buf = [0u8; 1];
    while a < limit {
        if m.peek_bytes(a, &mut buf).is_err() {
            break;
        }
        if buf[0] == 0 {
            return Ok(String::from_utf8_lossy(&bytes).into_owned());
        }
        bytes.push(buf[0]);
        a += 1;
    }
    Err(Halt::Error {
        message: format!("string at 0x{addr:08x} is missing its NUL terminator"),
    })
}

#[cfg(test)]
mod tests {
    use crate::testutil::*;
    use crate::ScriptHost;
    use crate::{Event, Halt};

    #[test]
    fn print_int_hex_binary_unsigned() {
        let src = "\
    li a0, 255
    li a7, 1
    ecall
    li a0, 255
    li a7, 34
    ecall
    li a0, -1
    li a7, 36
    ecall
    li a7, 10
    ecall
";
        let host = ScriptHost::default();
        let mut m = machine_with(src, Box::new(host.clone()));
        m.run(None);
        assert_eq!(host.take_output(), "2550x000000ff4294967295");
    }

    #[test]
    fn read_int() {
        let src = "\
    li a7, 5
    ecall
    addi a1, a0, 1
    li a7, 1
    mv a0, a1
    ecall
    li a7, 10
    ecall
";
        let host = ScriptHost::with_input(vec!["41".into()]);
        let mut m = machine_with(src, Box::new(host.clone()));
        m.run(None);
        assert_eq!(host.take_output(), "42");
    }

    #[test]
    fn exit_code() {
        let mut m = machine("    li a0, 3\n    li a7, 93\n    ecall\n");
        m.run(None);
        assert_eq!(m.exit_code(), Some(3));
    }

    #[test]
    fn unknown_service_errors() {
        let mut m = machine("    li a7, 999\n    ecall\n");
        let events = m.run(None);
        assert_eq!(
            events.last(),
            Some(&Event::Halted(Halt::Error { message: "service 999 is not available in this build".into() }))
        );
    }

    #[test]
    fn file_write_then_read_back() {
        let src = "\
.data
path: .asciz \"rvm-test.txt\"
msg: .asciz \"hello\"
buffer: .space 16
.text
    la a0, path
    li a1, 1
    li a7, 1024
    ecall
    mv s0, a0            # write-create fd
    mv a0, s0
    la a1, msg
    li a2, 5
    li a7, 64
    ecall
    mv s1, a0            # bytes written
    mv a0, s0
    li a7, 57
    ecall
    mv s2, a0            # close -> 0
    la a0, path
    li a1, 0
    li a7, 1024
    ecall
    mv s3, a0            # read fd
    mv a0, s3
    la a1, buffer
    li a2, 5
    li a7, 63
    ecall
    mv s4, a0            # bytes read
    la a0, buffer
    li a7, 4
    ecall
    li a7, 10
    ecall
";
        let host = ScriptHost::default();
        let mut m = machine_with(src, Box::new(host.clone()));
        m.run(None);
        assert_eq!(m.reg(8) as u32 as i32, 3); // s0: fds start at 3
        assert_eq!(m.reg(9), 5); // s1: 5 bytes written
        assert_eq!(m.reg(18), 0); // s2: close succeeded
        assert_eq!(m.reg(19) as u32 as i32, 4); // s3: next fd
        assert_eq!(m.reg(20), 5); // s4: 5 bytes read
        assert_eq!(host.take_output(), "hello");
        assert_eq!(host.file_contents("rvm-test.txt").as_deref(), Some(b"hello".as_slice()));
    }

    #[test]
    fn open_missing_file_fails() {
        let src = "\
.data
path: .asciz \"no-such-file\"
.text
    la a0, path
    li a1, 0
    li a7, 1024
    ecall
    li a7, 10
    ecall
";
        let mut m = machine_with(src, Box::new(ScriptHost::default()));
        m.run(None);
        assert_eq!(m.reg(10) as u32 as i32, -1);
    }

    #[test]
    fn lseek_repositions() {
        let src = "\
.data
path: .asciz \"s.txt\"
.text
    la a0, path
    li a1, 0
    li a7, 1024
    ecall
    mv s0, a0            # fd
    mv a0, s0
    mv a1, sp
    li a2, 2
    li a7, 63
    ecall                # read \"he\"
    mv a0, s0
    li a1, -1
    li a2, 2
    li a7, 62
    ecall                # seek to end-1
    mv s1, a0            # new offset 4
    mv a0, s0
    mv a1, sp
    li a2, 1
    li a7, 63
    ecall
    lbu a0, 0(sp)
    li a7, 11
    ecall                # prints the last byte
    li a7, 10
    ecall
";
        let host = ScriptHost::default();
        host.set_file("s.txt", b"hello");
        let mut m = machine_with(src, Box::new(host.clone()));
        m.run(None);
        assert_eq!(m.reg(9), 4); // s1: end-1 from a 5-byte file
        assert_eq!(host.take_output(), "o");
    }

    #[test]
    fn close_then_read_fails() {
        let src = "\
.data
path: .asciz \"s.txt\"
.text
    la a0, path
    li a1, 0
    li a7, 1024
    ecall
    mv s0, a0            # fd
    mv a0, s0
    li a7, 57
    ecall
    mv s1, a0            # close -> 0
    mv a0, s0
    mv a1, sp
    li a2, 4
    li a7, 63
    ecall                # read on a closed fd
    li a7, 10
    ecall
";
        let host = ScriptHost::default();
        host.set_file("s.txt", b"data");
        let mut m = machine_with(src, Box::new(host));
        m.run(None);
        assert_eq!(m.reg(9), 0); // close succeeded
        assert_eq!(m.reg(10) as u32 as i32, -1); // read after close fails
    }

    #[test]
    fn confirm_yes_and_no() {
        let src = "\
.data
q: .asciz \"Proceed?\"
.text
    la a0, q
    li a7, 50
    ecall
    li a7, 1
    ecall
    li a7, 10
    ecall
";
        // Input lines are consumed from the back, so "y" is served first.
        let host = ScriptHost::with_input(vec!["n".into(), "y".into()]);
        let mut m = machine_with(src, Box::new(host.clone()));
        m.run(None);
        assert_eq!(host.take_output(), "1");
        let host = ScriptHost::with_input(vec!["n".into()]);
        let mut m = machine_with(src, Box::new(host.clone()));
        m.run(None);
        assert_eq!(host.take_output(), "0");
    }

    #[test]
    fn message_dialogs_print_inline() {
        let src = "\
.data
m: .asciz \"All done\"
.text
    la a0, m
    li a1, 1
    li a7, 55
    ecall
    li a0, 42
    li a7, 56
    ecall
    la a0, m
    li a7, 59
    ecall
    li a7, 10
    ecall
";
        let host = ScriptHost::default();
        let mut m = machine_with(src, Box::new(host.clone()));
        m.run(None);
        assert_eq!(host.take_output(), "All done\n42\nAll done\n");
    }

    #[test]
    fn input_dialog_string_and_int() {
        let src = "\
.data
prompt: .asciz \"Name? \"
buf: .space 32
.text
    la a0, prompt
    la a1, buf
    li a2, 32
    li a7, 54
    ecall
    mv s0, a1            # status
    la a0, buf
    li a7, 4
    ecall                # echo the string back
    la a0, prompt
    li a7, 51
    ecall
    mv s1, a0
    li a7, 10
    ecall
";
        // Lines are consumed from the back: "Ada" first, then "-7".
        let host = ScriptHost::with_input(vec!["-7".into(), "Ada".into()]);
        let mut m = machine_with(src, Box::new(host.clone()));
        m.run(None);
        assert_eq!(m.reg(8), 0); // s0: status OK
        assert_eq!(m.reg(9) as u32 as i32, -7); // s1: parsed int
        assert_eq!(host.take_output(), "Name? AdaName? ");
    }
}
