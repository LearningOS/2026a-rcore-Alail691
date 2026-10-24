# 实验二（第 4 章）：地址空间

## 1. 实验目标

第 4 章为每个应用建立了独立的页表，内核与用户程序运行在不同的地址空间中。
本实验需要在这一前提下重新实现与「用户指针」相关的系统调用，并补全内存管理接口：

- `sys_get_time`：通过页表把 `TimeVal` 写回用户空间，需考虑结构体跨页的情况；
- `sys_trace`：通过页表读写用户内存，拒绝非法地址，并继续支持系统调用计数；
- `sys_mmap` / `sys_munmap`：建立/取消一段匿名映射；
- `sys_sbrk`：复用已经提供的 `change_program_brk`，验证堆区的动态扩展。

## 2. 实现思路

### 2.1 用户指针不能直接解引用

开启分页后，内核若直接 `*ptr` 访问用户指针，会以内核页表去解释用户虚拟地址，
轻则读到错误数据，重则触发内核页错误。因此在 `mm/page_table.rs` 中提供了一组
「带检查」的翻译函数：

```rust
/// User virtual addresses must lie in the low canonical half of SV39.
fn is_user_va(ptr: usize) -> bool {
    ptr < (1usize << 38)
}

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
```

这里有一个必须处理的细节：`VirtAddr::from(usize)` 只保留低 39 位。
若不做低半区检查，`isize::MAX` 之类的地址会被截断成 `0x7F_FFFF_FFFF`，
恰好与映射在地址空间顶端的 trampoline 页重合，从而被错误地判定为「可读」。
`ch4_trace1` 中 `trace_read(isize::MAX) == None` 的断言正是针对这一点。

写操作同理，额外检查 PTE 的 `W` 位：

```rust
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
```

### 2.2 sys_get_time 与跨页写入

`TimeVal` 由两个 `usize` 组成，共 16 字节。如果用户传入的地址位于页尾，
结构体会被拆到两个物理页上，因此不能只翻译一次。实现 `translated_write_buffer`
先检查整个区间内每一页都可写，再逐页拷贝，保证「要么全部写入、要么返回 -1」：

```rust
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
    // 检查每一页都可写，然后逐页拷贝
    ...
}
```

```rust
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    let us = get_time_us();
    let time_val = TimeVal { sec: us / 1_000_000, usec: us % 1_000_000 };
    let bytes = unsafe { core::slice::from_raw_parts(
        &time_val as *const TimeVal as *const u8,
        core::mem::size_of::<TimeVal>()) };
    if translated_write_buffer(current_user_token(), ts as usize, bytes) { 0 } else { -1 }
}
```

### 2.3 sys_trace

`sys_trace` 的三种请求分别对应读、写与计数：

```rust
pub fn sys_trace(trace_request: usize, id: usize, data: usize) -> isize {
    let token = current_user_token();
    match trace_request {
        0 => match translated_read_byte(token, id) {
            Some(byte) => byte as isize,
            None => -1,
        },
        1 => if translated_write_byte(token, id, data as u8) { 0 } else { -1 },
        2 => get_syscall_count(id) as isize,
        _ => -1,
    }
}
```

系统调用计数在第 4 章放入 `TaskControlBlockInner`：

```rust
/// Number of system calls made by this task, indexed by syscall id
pub syscall_times: [usize; MAX_SYSCALL_NUM],
```

并在 `syscall()` 分发前 `add_syscall_count(syscall_id)`，保证 `sys_trace` 查询自身时
能统计到当前这次调用。

### 2.4 mmap / munmap

`MemorySet` 增加两个方法。`mmap` 要求 `start` 页对齐、长度非零、权限合法，
且新区域不能与已有区域重叠；`end` 向上取整到页边界（`ch4_unmap` 中
`mmap(start-len, len+1)` 需要成功，说明长度允许不是页的整数倍）：

```rust
pub fn mmap(&mut self, start_va: VirtAddr, end_va: VirtAddr,
            permission: MapPermission) -> bool {
    let start_vpn = start_va.floor();
    let end_vpn = end_va.ceil();
    if start_vpn >= end_vpn || self.overlaps(start_vpn, end_vpn) {
        return false;
    }
    self.insert_framed_area(start_va, end_va, permission);
    true
}
```

`munmap` 要求两端都页对齐，并且恰好匹配一个已映射区域；这样
`munmap(start, len + 1)` 与 `munmap(start + 1, len - 1)` 都会返回 -1：

```rust
pub fn munmap(&mut self, start_va: VirtAddr, end_va: VirtAddr) -> bool {
    if !start_va.aligned() || !end_va.aligned() || start_va >= end_va {
        return false;
    }
    ...
    area.unmap(&mut self.page_table);   // 同时释放 data_frames
    true
}
```

系统调用层把 `prot` 的 `R/W/X` 位映射为 `MapPermission`，并拒绝空权限与越界权限：

```rust
if start % PAGE_SIZE != 0 || len == 0 || prot == 0 || prot > 0b111 {
    return -1;
}
```

### 2.5 sbrk

`sbrk` 直接复用模板提供的 `change_program_brk`：向 `heap_bottom` 处的零长度
`MapArea` 追加页，或把该区域收缩回 `heap_bottom`。当新的 break 低于堆底时返回 -1，
因此 `ch4b_sbrk` 中「释放超过已分配页数」的请求会失败，符合预期。

## 3. 测试结果

```sh
make -C ci-user test CHAPTER=4
```

`check/ch4.py` 共 16 个检查项全部通过：

```text
Test passed: 16/16
```

其中：

- `ch4_mmap0` 验证可写映射；
- `ch4_mmap1/2` 分别验证写只读页、读只写页会触发页错误并被内核杀死；
- `ch4_mmap3` 验证非法参数（重叠、未对齐、非法权限）均返回 -1；
- `ch4_unmap`、`ch4_unmap2` 验证取消映射与参数校验；
- `ch4_trace1` 验证用户内存读写与非法地址拒绝；
- `ch4b_sbrk` 验证堆的扩展与收缩。

日志中可见预期的页错误处理：

```text
[kernel] PageFault in application, bad addr = 0x10000000, ... kernel killed it.
```

并且没有出现 `FAIL: T.T` 或 `Should cause error`，说明所有测例行为正确。
