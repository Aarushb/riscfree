//! The teaching syscall layer (RARS-compatible numbers and register
//! conventions). File and dialog syscalls land in phase 2; unknown numbers
//! halt with the same kind of error RARS produces.

use crate::{Change, Event, Halt, Machine, StepOutcome};

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
            let c = m.host_mut().read_char().map(|b| b as u64).unwrap_or(u64::from(u32::MAX));
            m.write_reg(10, c, changes);
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
            m.fregs[10] = f.to_bits() as u64;
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
}
