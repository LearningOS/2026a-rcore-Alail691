# 实验三（第 5 章）：进程管理

## 1. 实验目标

第 5 章把「任务」升级为「进程 + 线程」模型，引入 PID、内核栈分配、
父进程/子进程关系以及 `fork`、`exec`、`waitpid` 等系统调用。
本实验需要完成：

- `sys_spawn`：创建一个全新地址空间的子进程（区别于 `fork + exec`）；
- `sys_set_priority`：设置进程优先级；
- 将调度器从 FIFO 改为 **stride 调度**，使不同优先级的进程按比例获得 CPU 时间；
- 补全 `sys_get_time`、`sys_mmap`、`sys_munmap`（第 4 章功能在进程模型下继续可用）。

## 2. 实现思路

### 2.1 spawn

`fork` 会复制父进程的地址空间，而 `spawn` 应直接根据 ELF 创建新进程。
它仍然需要登记父子关系，否则父进程的 `wait`/`waitpid` 无法回收子进程：

```rust
pub fn sys_spawn(path: *const u8) -> isize {
    let token = current_user_token();
    let path = translated_str(token, path);
    if let Some(data) = get_app_data_by_name(path.as_str()) {
        let new_task = Arc::new(TaskControlBlock::new(data));
        let pid = new_task.pid.0;
        // let the caller be the parent of the new process
        let current = current_task().unwrap();
        new_task.inner_exclusive_access().parent = Some(Arc::downgrade(&current));
        current.inner_exclusive_access().children.push(new_task.clone());
        add_task(new_task);
        pid as isize
    } else {
        -1
    }
}
```

`TaskControlBlock::new` 会完成：建立地址空间、分配 PID、分配内核栈、
初始化 `TrapContext`，因此 `spawn` 得到的是一个可以立即调度的任务。

### 2.2 stride 调度

每个任务维护三个字段：

```rust
/// Priority used by the stride scheduler
pub priority: usize,
/// Stride value of the task, the smaller it is, the earlier it runs
pub stride: usize,
/// Value added to `stride` for every scheduling, `BIG_STRIDE / priority`
pub pass: usize,
```

新建任务时 `priority = DEFAULT_PRIORITY = 16`、`stride = 0`、
`pass = BIG_STRIDE / priority`。调度器每次从就绪队列中选出 `stride` 最小的任务，
并把它「推进」一个 `pass`：

```rust
pub fn fetch(&mut self) -> Option<Arc<TaskControlBlock>> {
    let mut selected: Option<usize> = None;
    let mut min_stride = usize::MAX;
    for (idx, task) in self.ready_queue.iter().enumerate() {
        let stride = task.inner_exclusive_access().stride;
        if stride < min_stride {
            min_stride = stride;
            selected = Some(idx);
        }
    }
    let idx = selected?;
    let task = self.ready_queue.remove(idx).unwrap();
    {
        let mut inner = task.inner_exclusive_access();
        inner.stride += inner.pass;
    }
    Some(task)
}
```

优先级越高，`pass` 越小，`stride` 增长越慢，因而被调度的次数越多，
长期来看 CPU 时间与优先级成正比。

`fork` 时子进程继承父进程的 `priority`、`stride` 和 `pass`。

### 2.3 set_priority

```rust
pub fn sys_set_priority(prio: isize) -> isize {
    if prio < 2 {
        return -1;
    }
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    inner.priority = prio as usize;
    inner.pass = BIG_STRIDE / inner.priority;
    prio
}
```

优先级小于 2（包括负数和 0、1）视为非法，返回 -1；合法时返回新的优先级。
测试同时要求 `set_priority(isize::MAX) == isize::MAX`，因此不设上界。

### 2.4 get_time / mmap / munmap

进程模型下每个进程有自己的页表，第 4 章的实现可以直接复用：

- `sys_get_time` 通过 `translated_write_buffer` 逐页检查并写入 `TimeVal`，
  以支持结构体跨越两个用户页；
- `sys_mmap` 校验页对齐、长度、权限（`prot ∈ 1..=7`），再向当前进程的
  `MemorySet` 插入 `Framed` 区域，并与已有区域做重叠检查；
- `sys_munmap` 要求两端页对齐且精确匹配一个已映射区域，取消映射时同时释放
  `data_frames`。

## 3. 测试结果

```sh
make -C ci-user test CHAPTER=5
```

`check/ch5.py` 共 15 个检查项全部通过：

```text
Test passed: 15/15
```

stride 调度的实测输出如下（6 个子进程，优先级 5~10，各运行 4 秒）：

```text
priority = 5, exitcode = 21265600, ratio = 4253120
priority = 6, exitcode = 25536800, ratio = 4256133
priority = 7, exitcode = 29903600, ratio = 4271942
priority = 8, exitcode = 32895200, ratio = 4111900
priority = 9, exitcode = 38100400, ratio = 4233377
priority = 10, exitcode = 43140400, ratio = 4314040
```

各进程的 `count / priority` 基本一致（约 4.2×10⁶），说明 CPU 时间按优先级
成比例分配，stride 调度实现正确。

其余测例 `ch5_spawn0`、`ch5_spawn1`、`ch5_setprio`、`ch5b_forktest` 等均通过，
最终输出 `ch5 Usertests passed!`。
