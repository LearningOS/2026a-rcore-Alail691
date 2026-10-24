# 实验一（第 3 章）：多道程序与分时多任务 —— trace 系统调用

## 1. 实验目标

第 3 章将内核从「批处理」推进到「多道程序 + 分时多任务」，需要完成：

- 利用时钟中断实现任务抢占式切换；
- 实现 `sys_get_time`，让用户程序能够获取当前时间；
- 实现 `sys_trace`，支持读取/写入用户内存以及统计当前任务的系统调用次数。

本章实验（lab1）的核心是 `sys_trace`，其接口定义如下：

```rust
sys_trace(trace_request: usize, id: usize, data: usize) -> isize
```

- `trace_request == 0`（Read）：读取用户地址 `id` 处的一个字节并返回；
- `trace_request == 1`（Write）：把 `data` 的低 8 位写入用户地址 `id`，成功返回 0；
- `trace_request == 2`（Syscall）：返回当前任务调用系统调用 `id` 的次数。

## 2. 实现思路

### 2.1 系统调用计数器

`sys_trace` 需要按「任务」统计每类系统调用的次数，因此内核必须为每个任务维护一个计数数组。第 3 章的任务数量固定为 `MAX_APP_NUM = 16`，系统调用号上限按 500 预留：

```rust
/// the max number of syscalls (used by the trace syscall)
pub const MAX_SYSCALL_NUM: usize = 500;
```

计数数组放在 TCB 之外。若直接放进 `TaskControlBlock`，`[TaskControlBlock; MAX_APP_NUM]`
的初始化会在只有 64 KiB 的内核启动栈上产生约 64 KiB 的临时对象，容易造成栈溢出。
因此使用一个位于 `.bss` 的二维静态数组，以任务 id 为第一维：

```rust
static mut SYSCALL_TIMES: [[usize; MAX_SYSCALL_NUM]; MAX_APP_NUM] =
    [[0; MAX_SYSCALL_NUM]; MAX_APP_NUM];
```

并通过 `core::ptr::addr_of_mut!` 访问，避免产生 `static mut` 引用（Rust 2024 语义下不安全）：

```rust
pub fn add_syscall_count(syscall_id: usize) {
    if syscall_id >= MAX_SYSCALL_NUM {
        return;
    }
    let task_id = current_task_id();
    unsafe {
        let times = core::ptr::addr_of_mut!(SYSCALL_TIMES);
        (*times)[task_id][syscall_id] += 1;
    }
}
```

### 2.2 在分发前计数

计数必须发生在系统调用真正执行之前，否则 `sys_trace` 查询自身时看不到「正在执行」的这次调用。
测试代码中有如下断言：

```rust
assert_eq!(2, count_syscall(SYSCALL_TRACE));
```

第一次 `count_syscall(GETTIMEOFDAY)` 使 TRACE 计数变为 1，第二次 `count_syscall(TRACE)` 在返回前
使计数变为 2，因此 `syscall()` 入口处先计数、再分发：

```rust
pub fn syscall(syscall_id: usize, args: [usize; 3]) -> isize {
    add_syscall_count(syscall_id);
    match syscall_id {
        SYSCALL_WRITE => sys_write(args[0], args[1] as *const u8, args[2]),
        SYSCALL_EXIT => sys_exit(args[0] as i32),
        SYSCALL_YIELD => sys_yield(),
        SYSCALL_GET_TIME => sys_get_time(args[0] as *mut TimeVal, args[1]),
        SYSCALL_TRACE => sys_trace(args[0], args[1], args[2]),
        _ => panic!("Unsupported syscall_id: {}", syscall_id),
    }
}
```

### 2.3 sys_trace 的实现

第 3 章尚未开启分页，内核与用户程序共享同一物理地址空间，因此用户指针可以直接解引用：

```rust
pub fn sys_trace(trace_request: usize, id: usize, data: usize) -> isize {
    match trace_request {
        0 => unsafe { core::ptr::read_volatile(id as *const u8) as isize },
        1 => {
            unsafe { core::ptr::write_volatile(id as *mut u8, data as u8) };
            0
        }
        2 => get_syscall_count(id) as isize,
        _ => -1,
    }
}
```

其中 `get_syscall_count` 按当前任务 id 取出对应计数：

```rust
pub fn get_syscall_count(syscall_id: usize) -> usize {
    if syscall_id >= MAX_SYSCALL_NUM {
        return 0;
    }
    let task_id = current_task_id();
    unsafe {
        let times = core::ptr::addr_of!(SYSCALL_TIMES);
        (*times)[task_id][syscall_id]
    }
}
```

## 3. 测试结果

使用官方检查器运行：

```sh
make -C ci-user test CHAPTER=3
```

输出中与 lab1 相关的部分如下（节选）：

```text
string from task trace test

Test trace OK!
Test sleep OK!
```

`check/ch3.py` 的全部 7 个检查项均通过：

```text
7/7 Test passed
```

其中 `ch3_sleep`、`ch3_sleep1` 验证 `get_time` 与时钟中断，
`ch3_trace` 验证 `sys_trace` 的读、写、计数三种请求，二者均通过，
说明第 3 章的抢占式调度、时间获取与任务级系统调用统计工作正常。
