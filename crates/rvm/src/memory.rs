//! Sparse paged memory with RARS's segment layout.
//!
//! Addresses are 32-bit even in RV64 mode by design: RARS's memory map
//! (text at 0x00400000, static data at 0x10010000, stack top at 0x7fffeff8,
//! MMIO at 0xffff0000) lives entirely in the low 4 GB in both of its modes,
//! so widening the address path would change nothing a teaching program can
//! reach. Effective addresses truncate to u32; only the *data* width grows
//! (8-byte `ld`/`sd`/`fld`/`fsd`).

use std::collections::HashMap;

pub const PAGE_SIZE: usize = 4096;

/// Segment boundaries, matching RARS's default configuration.
#[derive(Debug, Clone)]
pub struct MemLayout {
    pub text_base: u32,
    pub text_len: u32,
    pub data_base: u32,
    pub heap_base: u32,
    pub stack_top: u32,
    /// Highest valid stack address (exclusive grows-down limit).
    pub stack_limit: u32,
    pub kernel_base: u32,
    pub mmio_base: u32,
}

impl Default for MemLayout {
    fn default() -> Self {
        MemLayout {
            text_base: 0x0040_0000,
            text_len: 4 * 1024 * 1024,
            data_base: 0x1000_0000,
            heap_base: 0x1004_0000,
            stack_top: 0x7fff_eff8,
            stack_limit: 0x7ff0_0000,
            kernel_base: 0x8000_0000,
            mmio_base: 0xffff_0000,
        }
    }
}

impl MemLayout {
    /// The layout family this matches, for UI labels. Returns "Custom" when
    /// the bases were hand-edited away from a preset.
    pub fn name(&self) -> &'static str {
        match (self.text_base, self.data_base) {
            (0x0040_0000, 0x1000_0000) => "Default",
            (0x0040_0000, 0x0000_0000) => "CompactDataAtZero",
            (0x0000_0000, 0x1000_0000) => "CompactTextAtZero",
            _ => "Custom",
        }
    }

    /// RARS's `CompactDataAtZero` setting: the data segment starts at
    /// address 0 and text stays at 0x00400000. Static and heap data keep
    /// their Default offsets from the segment base (static = data_base +
    /// 0x10000, heap = data_base + 0x40000), so the matching assembler
    /// config is `AsmConfig { data_base: 0x0001_0000, .. }`.
    pub fn compact_data_at_zero() -> Self {
        MemLayout {
            data_base: 0x0000_0000,
            heap_base: 0x0004_0000,
            ..MemLayout::default()
        }
    }

    /// RARS's `CompactTextAtZero` setting: text at address 0 and the data
    /// segment at its Default base 0x10000000 (assembler `data_base` stays
    /// 0x10010000). Course programs that assume a zero-based text segment
    /// need this so fetches and breakpoint addresses line up.
    pub fn compact_text_at_zero() -> Self {
        MemLayout {
            text_base: 0x0000_0000,
            ..MemLayout::default()
        }
    }
}

#[derive(Debug)]
pub enum MemError {
    /// Address outside every mapped segment.
    AccessViolation { addr: u32 },
    /// Load/store not aligned to its width.
    Unaligned { addr: u32, width: u32 },
}

impl std::fmt::Display for MemError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemError::AccessViolation { addr } => write!(
                f,
                "address 0x{addr:08x} is outside the mapped memory segments"
            ),
            MemError::Unaligned { addr, width } => {
                write!(
                    f,
                    "address 0x{addr:08x} is not aligned to a {width}-byte access"
                )
            }
        }
    }
}

/// Sparse memory: 4 KiB pages allocated on first write, reads of untouched
/// pages return zeros. Little-endian throughout.
#[derive(Default)]
pub struct Memory {
    pages: HashMap<u32, Box<[u8; PAGE_SIZE]>>,
}

impl Memory {
    pub fn clear(&mut self) {
        self.pages.clear();
    }

    fn page_mut(&mut self, base: u32) -> &mut [u8; PAGE_SIZE] {
        self.pages
            .entry(base)
            .or_insert_with(|| Box::new([0; PAGE_SIZE]))
    }

    pub fn write_bytes(&mut self, mut addr: u32, bytes: &[u8]) {
        let mut rest = bytes;
        while !rest.is_empty() {
            let off = (addr as usize) % PAGE_SIZE;
            let n = (PAGE_SIZE - off).min(rest.len());
            let (chunk, tail) = rest.split_at(n);
            self.page_mut(addr - off as u32)[off..off + n].copy_from_slice(chunk);
            rest = tail;
            addr += n as u32;
        }
    }

    pub fn read_bytes(&self, mut addr: u32, out: &mut [u8]) -> Result<(), MemError> {
        let mut off = 0usize;
        while off < out.len() {
            let page_off = (addr as usize) % PAGE_SIZE;
            let n = (PAGE_SIZE - page_off).min(out.len() - off);
            match self.pages.get(&(addr - page_off as u32)) {
                Some(page) => out[off..off + n].copy_from_slice(&page[page_off..page_off + n]),
                None => out[off..off + n].fill(0),
            }
            off += n;
            addr += n as u32;
        }
        Ok(())
    }

    pub fn read_u8(&self, addr: u32) -> Result<u8, MemError> {
        let mut b = [0u8; 1];
        self.read_bytes(addr, &mut b)?;
        Ok(b[0])
    }

    pub fn read_u16(&self, addr: u32) -> Result<u16, MemError> {
        let mut b = [0u8; 2];
        self.read_bytes(addr, &mut b)?;
        Ok(u16::from_le_bytes(b))
    }

    pub fn read_u32(&self, addr: u32) -> Result<u32, MemError> {
        let mut b = [0u8; 4];
        self.read_bytes(addr, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    pub fn read_u64(&self, addr: u32) -> Result<u64, MemError> {
        let mut b = [0u8; 8];
        self.read_bytes(addr, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_roundtrip() {
        let mut m = Memory::default();
        m.write_bytes(0x1001_0000, &[1, 2, 3, 4]);
        assert_eq!(m.read_u32(0x1001_0000).unwrap(), 0x0403_0201);
        assert_eq!(m.read_u32(0x1001_0000 + 4096).unwrap(), 0); // untouched page
    }

    #[test]
    fn page_crossing_writes() {
        let mut m = Memory::default();
        let addr = 0x1000_0FFE; // last two bytes of a page
        m.write_bytes(addr, &[0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!(m.read_u16(0x1000_0FFE).unwrap(), 0xBBAA);
        assert_eq!(m.read_u16(0x1000_1000).unwrap(), 0xDDCC);
    }

    #[test]
    fn unwritten_reads_zero() {
        let m = Memory::default();
        assert_eq!(m.read_u64(0x7fff_ff00).unwrap(), 0);
    }

    #[test]
    fn default_preset_keeps_rars_layout() {
        let l = MemLayout::default();
        assert_eq!(l.name(), "Default");
        assert_eq!(l.text_base, 0x0040_0000);
        assert_eq!(l.text_len, 4 * 1024 * 1024);
        assert_eq!(l.data_base, 0x1000_0000);
        assert_eq!(l.heap_base, 0x1004_0000);
        assert_eq!(l.stack_top, 0x7fff_eff8);
        assert_eq!(l.mmio_base, 0xffff_0000);
    }

    #[test]
    fn compact_data_preset_moves_data_segment_to_zero() {
        let l = MemLayout::compact_data_at_zero();
        assert_eq!(l.name(), "CompactDataAtZero");
        // Text stays at its Default base; the data segment base moves to 0
        // and heap keeps its Default offset from the segment base.
        assert_eq!(l.text_base, 0x0040_0000);
        assert_eq!(l.data_base, 0x0000_0000);
        assert_eq!(l.heap_base, 0x0004_0000);
        // Everything outside the data segment is unchanged.
        assert_eq!(l.stack_top, MemLayout::default().stack_top);
        assert_eq!(l.mmio_base, MemLayout::default().mmio_base);
    }

    #[test]
    fn compact_text_preset_moves_text_to_zero() {
        let l = MemLayout::compact_text_at_zero();
        assert_eq!(l.name(), "CompactTextAtZero");
        assert_eq!(l.text_base, 0x0000_0000);
        // The data segment sits at the Default base.
        assert_eq!(l.data_base, 0x1000_0000);
        assert_eq!(l.heap_base, 0x1004_0000);
    }

    #[test]
    fn hand_edited_bases_report_custom() {
        let l = MemLayout {
            text_base: 0x0030_0000,
            ..MemLayout::default()
        };
        assert_eq!(l.name(), "Custom");
    }
}
