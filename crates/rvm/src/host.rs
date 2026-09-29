//! The host interface: everything the machine needs from the outside world.
//! The CLI and GUI provide different implementations; syscall behavior is
//! identical because the machine only sees this trait.

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

/// Host services the machine can call synchronously.
pub trait Host {
    /// Console output from print syscalls (called with complete chunks).
    fn write_output(&mut self, text: &str);
    /// Read one line (without the newline). None means end of input.
    fn read_line(&mut self) -> Option<String>;
    /// Read a single character of input. None means end of input.
    fn read_char(&mut self) -> Option<u8> {
        self.read_line().and_then(|l| l.bytes().next())
    }
    fn sleep_ms(&mut self, _ms: u64) {}
    fn time_ms(&mut self) -> u64 {
        0
    }
    fn get_cwd(&mut self) -> String {
        String::new()
    }
    /// Seeded random family; `idx` selects the stream (RARS supports 32).
    fn random_seed(&mut self, _idx: u32, _seed: u64) {}
    fn random_int(&mut self, _idx: u32) -> i32 {
        0
    }
    fn random_range(&mut self, _idx: u32, bound: u32) -> i32 {
        (bound.min(1).wrapping_sub(1)) as i32
    }
    fn random_float(&mut self, _idx: u32) -> f32 {
        0.0
    }
    fn random_double(&mut self, _idx: u32) -> f64 {
        0.0
    }
}

/// Real stdin/stdout host for the CLI.
#[derive(Default)]
pub struct StdHost {
    rng_streams: std::collections::HashMap<u32, u64>,
}

impl StdHost {
    fn next_stream(&mut self, idx: u32) -> u64 {
        let v = self.rng_streams.entry(idx).or_insert(0x2545_F491_4F6C_DD1D);
        // xorshift64*
        *v ^= *v >> 12;
        *v ^= *v << 25;
        *v ^= *v >> 27;
        v.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

impl Host for StdHost {
    fn write_output(&mut self, text: &str) {
        print!("{text}");
        let _ = std::io::stdout().flush();
    }

    fn read_line(&mut self) -> Option<String> {
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) => None,
            Ok(_) => Some(line.trim_end_matches(['\n', '\r']).to_string()),
            Err(_) => None,
        }
    }

    fn time_ms(&mut self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    fn get_cwd(&mut self) -> String {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    fn sleep_ms(&mut self, ms: u64) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }

    fn random_seed(&mut self, idx: u32, seed: u64) {
        self.rng_streams.insert(idx, seed);
    }

    fn random_int(&mut self, idx: u32) -> i32 {
        self.next_stream(idx) as i32
    }

    fn random_range(&mut self, idx: u32, bound: u32) -> i32 {
        (self.next_stream(idx) % bound.max(1) as u64) as i32
    }

    fn random_float(&mut self, idx: u32) -> f32 {
        (self.next_stream(idx) % (1 << 24)) as f32 / (1u32 << 24) as f32
    }

    fn random_double(&mut self, idx: u32) -> f64 {
        (self.next_stream(idx) % (1u64 << 53)) as f64 / (1u64 << 53) as f64
    }
}

/// Test/script host: scripted input lines, captured output. `Clone` so tests
/// can keep a handle while the machine owns the boxed host.
#[derive(Clone, Default)]
pub struct ScriptHost {
    input: Rc<RefCell<Vec<String>>>,
    output: Rc<RefCell<String>>,
    now_ms: Rc<RefCell<u64>>,
}

impl ScriptHost {
    pub fn with_input(lines: Vec<String>) -> Self {
        ScriptHost { input: Rc::new(RefCell::new(lines)), ..Default::default() }
    }

    pub fn take_output(&self) -> String {
        std::mem::take(&mut *self.output.borrow_mut())
    }

    pub fn output(&self) -> String {
        self.output.borrow().clone()
    }
}

impl Host for ScriptHost {
    fn write_output(&mut self, text: &str) {
        self.output.borrow_mut().push_str(text);
    }

    fn read_line(&mut self) -> Option<String> {
        self.input.borrow_mut().pop()
    }

    fn time_ms(&mut self) -> u64 {
        *self.now_ms.borrow()
    }
}
