//! The host interface: everything the machine needs from the outside world.
//! The CLI and GUI provide different implementations; syscall behavior is
//! identical because the machine only sees this trait.

use std::cell::RefCell;
use std::collections::HashMap;
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
    /// Non-blocking poll for one keystroke feeding the MMIO receiver port
    /// (0xffff0004). None means no key is waiting; it never blocks. The
    /// default None suits hosts that cannot poll stdin without blocking.
    fn poll_input_char(&mut self) -> Option<u8> {
        None
    }
    /// Yes/no question (RARS ConfirmDialog). The host decides how to ask.
    fn confirm(&mut self, _message: &str) -> bool {
        false
    }
    /// Open a host file: read-only, or write with create+truncate (append
    /// keeps existing contents instead). Returns a host fd or -1.
    fn file_open(&mut self, _path: &str, _write: bool, _append: bool) -> i32 {
        -1
    }
    /// Read up to `buf.len()` bytes; returns the count, 0 at EOF, -1 on error.
    fn file_read(&mut self, _fd: i32, _buf: &mut [u8]) -> i32 {
        -1
    }
    /// Write all bytes; returns the count or -1.
    fn file_write(&mut self, _fd: i32, _bytes: &[u8]) -> i32 {
        -1
    }
    /// Reposition: whence 0 = start, 1 = current, 2 = end. Returns the new
    /// offset (low 32 bits) or -1.
    fn file_seek(&mut self, _fd: i32, _offset: i32, _whence: i32) -> i32 {
        -1
    }
    /// Close a file: 0 on success, -1 on an unknown fd.
    fn file_close(&mut self, _fd: i32) -> i32 {
        -1
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

/// Real stdin/stdout host for the CLI. File syscalls hit the real filesystem;
/// `poll_input_char` keeps the trait default because stdin has no portable
/// non-blocking read — MMIO keyboard programs under the CLI see no keys.
pub struct StdHost {
    rng_streams: HashMap<u32, u64>,
    files: HashMap<i32, std::fs::File>,
    next_fd: i32,
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

impl Default for StdHost {
    // fds 0-2 stand for stdin/out/err, so program files start at 3.
    fn default() -> Self {
        StdHost {
            rng_streams: HashMap::new(),
            files: HashMap::new(),
            next_fd: 3,
        }
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

    fn confirm(&mut self, message: &str) -> bool {
        self.write_output(&format!("{message} [y/n] "));
        self.read_line()
            .is_some_and(|l| l.trim().starts_with(['y', 'Y']))
    }

    fn file_open(&mut self, path: &str, write: bool, append: bool) -> i32 {
        let mut opts = std::fs::OpenOptions::new();
        if write {
            opts.write(true).create(true);
            if append {
                opts.append(true);
            } else {
                opts.truncate(true);
            }
        } else {
            opts.read(true);
        }
        match opts.open(path) {
            Ok(f) => {
                let fd = self.next_fd;
                self.next_fd += 1;
                self.files.insert(fd, f);
                fd
            }
            Err(_) => -1,
        }
    }

    fn file_read(&mut self, fd: i32, buf: &mut [u8]) -> i32 {
        use std::io::Read;
        match self.files.get_mut(&fd).map(|f| f.read(buf)) {
            Some(Ok(n)) => n as i32,
            _ => -1,
        }
    }

    fn file_write(&mut self, fd: i32, bytes: &[u8]) -> i32 {
        use std::io::Write;
        match self.files.get_mut(&fd).map(|f| f.write_all(bytes)) {
            Some(Ok(())) => bytes.len() as i32,
            _ => -1,
        }
    }

    fn file_seek(&mut self, fd: i32, offset: i32, whence: i32) -> i32 {
        use std::io::{Seek, SeekFrom};
        let from = match whence {
            0 => SeekFrom::Start(u64::from(offset as u32)),
            1 => SeekFrom::Current(offset as i64),
            _ => SeekFrom::End(offset as i64),
        };
        match self.files.get_mut(&fd).map(|f| f.seek(from)) {
            Some(Ok(pos)) => pos as u32 as i32,
            _ => -1,
        }
    }

    fn file_close(&mut self, fd: i32) -> i32 {
        if self.files.remove(&fd).is_some() {
            0
        } else {
            -1
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

/// One host-side open file over the in-memory image store.
struct OpenScriptFile {
    name: String,
    pos: usize,
    /// Opened for writing (reads fail on write fds and vice versa, like the OS).
    write: bool,
}

/// Test/script host: scripted input lines, captured output, and an in-memory
/// file store. `Clone` so tests can keep a handle while the machine owns the
/// boxed host.
#[derive(Clone)]
pub struct ScriptHost {
    input: Rc<RefCell<Vec<String>>>,
    output: Rc<RefCell<String>>,
    now_ms: Rc<RefCell<u64>>,
    /// File images by path; `set_file` seeds reads, writes land back here.
    files: Rc<RefCell<HashMap<String, Vec<u8>>>>,
    open: Rc<RefCell<HashMap<i32, OpenScriptFile>>>,
    next_fd: Rc<RefCell<i32>>,
}

impl Default for ScriptHost {
    // fds 0-2 stand for stdin/out/err, so program files start at 3.
    fn default() -> Self {
        ScriptHost {
            input: Rc::new(RefCell::new(Vec::new())),
            output: Rc::new(RefCell::new(String::new())),
            now_ms: Rc::new(RefCell::new(0)),
            files: Rc::new(RefCell::new(HashMap::new())),
            open: Rc::new(RefCell::new(HashMap::new())),
            next_fd: Rc::new(RefCell::new(3)),
        }
    }
}

impl ScriptHost {
    pub fn with_input(lines: Vec<String>) -> Self {
        ScriptHost {
            input: Rc::new(RefCell::new(lines)),
            ..Default::default()
        }
    }

    /// Seed a file image so the program can open and read it by name.
    pub fn set_file(&self, name: &str, contents: &[u8]) {
        self.files
            .borrow_mut()
            .insert(name.to_string(), contents.to_vec());
    }

    /// The current bytes stored under `name` (what a program wrote, or the
    /// seeded image when untouched).
    pub fn file_contents(&self, name: &str) -> Option<Vec<u8>> {
        self.files.borrow().get(name).cloned()
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

    fn poll_input_char(&mut self) -> Option<u8> {
        let mut input = self.input.borrow_mut();
        loop {
            // Lines are consumed from the back (matching read_line); hand out
            // their bytes one at a time for the MMIO keyboard.
            let line = input.last_mut()?;
            if line.is_empty() {
                input.pop();
                continue;
            }
            let b = line.remove(0) as u32 as u8;
            return Some(b);
        }
    }

    fn confirm(&mut self, _message: &str) -> bool {
        self.read_line()
            .is_some_and(|l| l.trim().starts_with(['y', 'Y']))
    }

    fn file_open(&mut self, path: &str, write: bool, append: bool) -> i32 {
        let mut files = self.files.borrow_mut();
        if write {
            // create; truncate unless appending
            let image = files.entry(path.to_string()).or_default();
            if !append {
                image.clear();
            }
        } else if !files.contains_key(path) {
            return -1; // nothing seeded under this name
        }
        let pos = if append {
            files.get(path).map_or(0, |v| v.len())
        } else {
            0
        };
        let fd = *self.next_fd.borrow();
        *self.next_fd.borrow_mut() += 1;
        self.open.borrow_mut().insert(
            fd,
            OpenScriptFile {
                name: path.to_string(),
                pos,
                write,
            },
        );
        fd
    }

    fn file_read(&mut self, fd: i32, buf: &mut [u8]) -> i32 {
        let mut open = self.open.borrow_mut();
        let files = self.files.borrow();
        let Some(f) = open.get_mut(&fd) else {
            return -1;
        };
        if f.write {
            return -1;
        }
        let Some(image) = files.get(&f.name) else {
            return -1;
        };
        let n = image.len().saturating_sub(f.pos).min(buf.len());
        buf[..n].copy_from_slice(&image[f.pos..f.pos + n]);
        f.pos += n;
        n as i32
    }

    fn file_write(&mut self, fd: i32, bytes: &[u8]) -> i32 {
        let mut open = self.open.borrow_mut();
        let mut files = self.files.borrow_mut();
        let Some(f) = open.get_mut(&fd) else {
            return -1;
        };
        if !f.write {
            return -1;
        }
        let Some(image) = files.get_mut(&f.name) else {
            return -1;
        };
        let end = f.pos + bytes.len();
        if image.len() < end {
            image.resize(end, 0);
        }
        image[f.pos..end].copy_from_slice(bytes);
        f.pos = end;
        bytes.len() as i32
    }

    fn file_seek(&mut self, fd: i32, offset: i32, whence: i32) -> i32 {
        let mut open = self.open.borrow_mut();
        let files = self.files.borrow();
        let Some(f) = open.get_mut(&fd) else {
            return -1;
        };
        let base: i64 = match whence {
            0 => 0,
            1 => f.pos as i64,
            _ => files.get(&f.name).map_or(0, |v| v.len() as i64),
        };
        let target = base + offset as i64;
        if target < 0 {
            return -1;
        }
        f.pos = target as usize;
        target as i32
    }

    fn file_close(&mut self, fd: i32) -> i32 {
        if self.open.borrow_mut().remove(&fd).is_some() {
            0
        } else {
            -1
        }
    }

    fn time_ms(&mut self) -> u64 {
        *self.now_ms.borrow()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdhost_file_roundtrip_seek_and_append() {
        // A scratch directory outside the repo; cleaned up at both ends so a
        // prior failed run cannot poison this one.
        let dir = std::env::temp_dir().join("rvm-stdhost-file-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("roundtrip.txt");
        let path_str = path.to_str().unwrap().to_string();

        let mut h = StdHost::default();
        let wfd = h.file_open(&path_str, true, false);
        assert_eq!(wfd, 3); // fds start after stdin/out/err
        assert_eq!(h.file_write(wfd, b"hello world"), 11);
        assert_eq!(h.file_close(wfd), 0);

        let mut h = StdHost::default();
        let rfd = h.file_open(&path_str, false, false);
        assert_eq!(rfd, 3);
        let mut buf = [0u8; 5];
        assert_eq!(h.file_read(rfd, &mut buf), 5);
        assert_eq!(&buf, b"hello");
        assert_eq!(h.file_seek(rfd, -1, 2), 10); // whence end: offset 10 of 11
        assert_eq!(h.file_read(rfd, &mut buf), 1);
        assert_eq!(buf[0], b'd');
        assert_eq!(h.file_read(rfd, &mut buf), 0); // EOF
        assert_eq!(h.file_close(rfd), 0);
        assert_eq!(h.file_read(rfd, &mut buf), -1); // closed fd

        // Append reopens without truncating; plain write would truncate.
        let mut h = StdHost::default();
        let afd = h.file_open(&path_str, true, true);
        assert_eq!(h.file_write(afd, b"!"), 1);
        assert_eq!(h.file_close(afd), 0);
        assert_eq!(std::fs::read(&path).unwrap().len(), 12);

        assert_eq!(h.file_open("no/such/path/x", false, false), -1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
