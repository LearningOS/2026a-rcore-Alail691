//! Implementation of [`PageTableEntry`] and [`PageTable`].

use super::{frame_alloc, FrameTracker, PhysPageNum, StepByOne, VirtAddr, VirtPageNum};
use crate::config::{PAGE_SIZE, PAGE_SIZE_BITS};
use alloc::vec;
use alloc::vec::Vec;
use bitflags::*;

bitflags! {
    /// page table entry flags
    pub struct PTEFlags: u8 {
        /// Valid
        const V = 1 << 0;
        /// Readable
        const R = 1 << 1;
        /// Writable
        const W = 1 << 2;
        /// eXecutable
        const X = 1 << 3;
        /// User
        const U = 1 << 4;
        /// Global
        const G = 1 << 5;
        /// Accessed
        const A = 1 << 6;
        /// Dirty
        const D = 1 << 7;
    }
}

#[derive(Copy, Clone)]
#[repr(C)]
/// page table entry structure
pub struct PageTableEntry {
    /// bits of page table entry
    pub bits: usize,
}

impl PageTableEntry {
    /// Create a new page table entry
    pub fn new(ppn: PhysPageNum, flags: PTEFlags) -> Self {
        PageTableEntry {
            bits: ppn.0 << 10 | flags.bits as usize,
        }
    }
    /// Create an empty page table entry
    pub fn empty() -> Self {
        PageTableEntry { bits: 0 }
    }
    /// Get the physical page number from the page table entry
    pub fn ppn(&self) -> PhysPageNum {
        (self.bits >> 10 & ((1usize << 44) - 1)).into()
    }
    /// Get the flags from the page table entry
    pub fn flags(&self) -> PTEFlags {
        PTEFlags::from_bits(self.bits as u8).unwrap()
    }
    /// The page pointered by page table entry is valid?
    pub fn is_valid(&self) -> bool {
        (self.flags() & PTEFlags::V) != PTEFlags::empty()
    }
    /// The page pointered by page table entry is readable?
    pub fn readable(&self) -> bool {
        (self.flags() & PTEFlags::R) != PTEFlags::empty()
    }
    /// The page pointered by page table entry is writable?
    pub fn writable(&self) -> bool {
        (self.flags() & PTEFlags::W) != PTEFlags::empty()
    }
    /// The page pointered by page table entry is executable?
    pub fn executable(&self) -> bool {
        (self.flags() & PTEFlags::X) != PTEFlags::empty()
    }
}

/// page table structure
pub struct PageTable {
    root_ppn: PhysPageNum,
    frames: Vec<FrameTracker>,
}

/// Assume that it won't oom when creating/mapping.
impl PageTable {
    /// Create a new page table
    pub fn new() -> Self {
        let frame = frame_alloc().unwrap();
        PageTable {
            root_ppn: frame.ppn,
            frames: vec![frame],
        }
    }
    /// Temporarily used to get arguments from user space.
    pub fn from_token(satp: usize) -> Self {
        Self {
            root_ppn: PhysPageNum::from(satp & ((1usize << 44) - 1)),
            frames: Vec::new(),
        }
    }
    /// Find PageTableEntry by VirtPageNum, create a frame for a 4KB page table if not exist
    fn find_pte_create(&mut self, vpn: VirtPageNum) -> Option<&mut PageTableEntry> {
        let idxs = vpn.indexes();
        let mut ppn = self.root_ppn;
        let mut result: Option<&mut PageTableEntry> = None;
        for (i, idx) in idxs.iter().enumerate() {
            let pte = &mut ppn.get_pte_array()[*idx];
            if i == 2 {
                result = Some(pte);
                break;
            }
            if !pte.is_valid() {
                let frame = frame_alloc().unwrap();
                *pte = PageTableEntry::new(frame.ppn, PTEFlags::V);
                self.frames.push(frame);
            }
            ppn = pte.ppn();
        }
        result
    }
    /// Find PageTableEntry by VirtPageNum
    fn find_pte(&self, vpn: VirtPageNum) -> Option<&mut PageTableEntry> {
        let idxs = vpn.indexes();
        let mut ppn = self.root_ppn;
        let mut result: Option<&mut PageTableEntry> = None;
        for (i, idx) in idxs.iter().enumerate() {
            let pte = &mut ppn.get_pte_array()[*idx];
            if i == 2 {
                result = Some(pte);
                break;
            }
            if !pte.is_valid() {
                return None;
            }
            ppn = pte.ppn();
        }
        result
    }
    /// set the map between virtual page number and physical page number
    #[allow(unused)]
    pub fn map(&mut self, vpn: VirtPageNum, ppn: PhysPageNum, flags: PTEFlags) {
        let pte = self.find_pte_create(vpn).unwrap();
        assert!(!pte.is_valid(), "vpn {:?} is mapped before mapping", vpn);
        *pte = PageTableEntry::new(ppn, flags | PTEFlags::V);
    }
    /// remove the map between virtual page number and physical page number
    #[allow(unused)]
    pub fn unmap(&mut self, vpn: VirtPageNum) {
        let pte = self.find_pte(vpn).unwrap();
        assert!(pte.is_valid(), "vpn {:?} is invalid before unmapping", vpn);
        *pte = PageTableEntry::empty();
    }
    /// get the page table entry from the virtual page number
    pub fn translate(&self, vpn: VirtPageNum) -> Option<PageTableEntry> {
        self.find_pte(vpn).map(|pte| *pte)
    }
    /// get the token from the page table
    pub fn token(&self) -> usize {
        8usize << 60 | self.root_ppn.0
    }
}

/// Translate&Copy a ptr[u8] array with LENGTH len to a mutable u8 Vec through page table
pub fn translated_byte_buffer(token: usize, ptr: *const u8, len: usize) -> Vec<&'static mut [u8]> {
    let page_table = PageTable::from_token(token);
    let mut start = ptr as usize;
    let end = start + len;
    let mut v = Vec::new();
    while start < end {
        let start_va = VirtAddr::from(start);
        let mut vpn = start_va.floor();
        let ppn = page_table.translate(vpn).unwrap().ppn();
        vpn.step();
        let mut end_va: VirtAddr = vpn.into();
        end_va = end_va.min(VirtAddr::from(end));
        if end_va.page_offset() == 0 {
            v.push(&mut ppn.get_bytes_array()[start_va.page_offset()..]);
        } else {
            v.push(&mut ppn.get_bytes_array()[start_va.page_offset()..end_va.page_offset()]);
        }
        start = end_va.into();
    }
    v
}

/// User virtual addresses must lie in the low canonical half of SV39.
///
/// Without this check, an address such as `isize::MAX` would be truncated to
/// 39 bits and could alias the trampoline page mapped at the top of the
/// address space.
fn is_user_va(ptr: usize) -> bool {
    ptr < (1usize << 38)
}

/// Translate and read a byte from user space.
///
/// Returns `None` if the address is not mapped or is not readable.
pub fn translated_read_byte(token: usize, ptr: usize) -> Option<u8> {
    if !is_user_va(ptr) {
        return None;
    }
    let page_table = PageTable::from_token(token);
    let va = VirtAddr::from(ptr);
    let pte = page_table.translate(va.floor())?;
    if !pte.is_valid() || !pte.readable() {
        return None;
    }
    Some(pte.ppn().get_bytes_array()[va.page_offset()])
}

/// Translate and write a byte into user space.
///
/// Returns `false` if the address is not mapped or is not writable.
pub fn translated_write_byte(token: usize, ptr: usize, data: u8) -> bool {
    if !is_user_va(ptr) {
        return false;
    }
    let page_table = PageTable::from_token(token);
    let va = VirtAddr::from(ptr);
    let pte = match page_table.translate(va.floor()) {
        Some(pte) if pte.is_valid() && pte.writable() => pte,
        _ => return false,
    };
    pte.ppn().get_bytes_array()[va.page_offset()] = data;
    true
}

/// Translate and copy a byte slice into user space.
///
/// All the destination pages are checked before copying, so the copy is
/// all-or-nothing. Returns `false` if any page is not writable.
pub fn translated_write_buffer(token: usize, ptr: usize, data: &[u8]) -> bool {
    if data.is_empty() {
        return true;
    }
    let end = match ptr.checked_add(data.len()) {
        Some(end) => end,
        None => return false,
    };
    if !is_user_va(ptr) || !is_user_va(end - 1) {
        return false;
    }
    let page_table = PageTable::from_token(token);
    // first check that every page in the range is mapped and writable
    let mut addr = ptr;
    while addr < end {
        let va = VirtAddr::from(addr);
        match page_table.translate(va.floor()) {
            Some(pte) if pte.is_valid() && pte.writable() => {}
            _ => return false,
        }
        addr = (va.floor().0 + 1) << PAGE_SIZE_BITS;
    }
    // then copy page by page
    let mut start = ptr;
    let mut copied = 0;
    while copied < data.len() {
        let va = VirtAddr::from(start);
        let pte = page_table.translate(va.floor()).unwrap();
        let offset = va.page_offset();
        let n = core::cmp::min(PAGE_SIZE - offset, data.len() - copied);
        pte.ppn().get_bytes_array()[offset..offset + n]
            .copy_from_slice(&data[copied..copied + n]);
        start += n;
        copied += n;
    }
    true
}

