//! Process management syscalls
use crate::mm::{
    translated_read_byte, translated_write_buffer, translated_write_byte, MapPermission,
};
use crate::task::{
    change_program_brk, current_user_token, exit_current_and_run_next, get_syscall_count,
    mmap_current, munmap_current, suspend_current_and_run_next,
};
use crate::timer::get_time_us;

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// task exits and submit an exit code
pub fn sys_exit(_exit_code: i32) -> ! {
    trace!("kernel: sys_exit");
    exit_current_and_run_next();
    panic!("Unreachable in sys_exit!");
}

/// current task gives up resources for other tasks
pub fn sys_yield() -> isize {
    trace!("kernel: sys_yield");
    suspend_current_and_run_next();
    0
}

/// get time with second and microsecond
///
/// The `TimeVal` is written through the page table, since it may be split by
/// two user pages and the user addresses cannot be dereferenced directly.
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    trace!("kernel: sys_get_time");
    let us = get_time_us();
    let time_val = TimeVal {
        sec: us / 1_000_000,
        usec: us % 1_000_000,
    };
    let bytes = unsafe {
        core::slice::from_raw_parts(
            &time_val as *const TimeVal as *const u8,
            core::mem::size_of::<TimeVal>(),
        )
    };
    if translated_write_buffer(current_user_token(), ts as usize, bytes) {
        0
    } else {
        -1
    }
}

/// trace the memory access or the number of system calls of the current task
///
/// * `trace_request == 0`: read a byte from the user address `id`
/// * `trace_request == 1`: write `data` to the user address `id`
/// * `trace_request == 2`: get the number of system calls `id` made by the task
pub fn sys_trace(trace_request: usize, id: usize, data: usize) -> isize {
    trace!("kernel: sys_trace");
    let token = current_user_token();
    match trace_request {
        0 => match translated_read_byte(token, id) {
            Some(byte) => byte as isize,
            None => -1,
        },
        1 => {
            if translated_write_byte(token, id, data as u8) {
                0
            } else {
                -1
            }
        }
        2 => get_syscall_count(id) as isize,
        _ => -1,
    }
}

/// implement mmap: map a new anonymous area of `len` bytes at `start`
pub fn sys_mmap(start: usize, len: usize, prot: usize) -> isize {
    trace!("kernel: sys_mmap");
    // `start` must be page aligned and the permission must be a non-empty
    // subset of R/W/X
    if start % crate::config::PAGE_SIZE != 0 || len == 0 || prot == 0 || prot > 0b111 {
        return -1;
    }
    let mut permission = MapPermission::U;
    if prot & 0b001 != 0 {
        permission |= MapPermission::R;
    }
    if prot & 0b010 != 0 {
        permission |= MapPermission::W;
    }
    if prot & 0b100 != 0 {
        permission |= MapPermission::X;
    }
    if mmap_current(start, start + len, permission) {
        0
    } else {
        -1
    }
}

/// implement munmap: unmap the area starting at `start` with `len` bytes
pub fn sys_munmap(start: usize, len: usize) -> isize {
    trace!("kernel: sys_munmap");
    if start % crate::config::PAGE_SIZE != 0
        || len == 0
        || len % crate::config::PAGE_SIZE != 0
    {
        return -1;
    }
    if munmap_current(start, start + len) {
        0
    } else {
        -1
    }
}
/// change data segment size
pub fn sys_sbrk(size: i32) -> isize {
    trace!("kernel: sys_sbrk");
    if let Some(old_brk) = change_program_brk(size) {
        old_brk as isize
    } else {
        -1
    }
}
