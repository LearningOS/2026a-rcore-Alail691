# 实验五（第 8 章）：并发与同步

## 1. 实验目标

第 8 章把进程进一步拆分为「进程 + 线程」：同一进程内的多个线程共享地址空间和
文件描述符表，但各自拥有 tid、用户栈和内核栈。内核提供线程创建/等待以及
互斥锁、信号量、条件变量等同步原语。本实验（lab5）的核心是 **死锁检测**：

- `sys_enable_deadlock_detect(enabled)` 打开/关闭死锁检测；
- 阻塞互斥锁在同一线程重复加锁时必须返回 `-0xdead`；
- 多线程争用多个信号量时，若请求会导致死锁，则返回 `-0xdead` 并唤醒所有
  被卷入死锁的线程；对于不会死锁的请求必须正常放行。

## 2. 实现思路

### 2.1 互斥锁的自死锁检测

`Mutex` trait 的 `lock` 改为返回 `isize`，成功返回 0，检测到死锁返回 `-0xdead`。
`MutexBlockingInner` 增加 `owner: Option<usize>` 记录持有者 tid：

```rust
fn lock(&self) -> isize {
    let tid = crate::task::current_tid();
    let detect = crate::task::deadlock_detect_enabled();
    let mut mutex_inner = self.inner.exclusive_access();
    if mutex_inner.locked {
        // locking a mutex twice in the same thread deadlocks
        if detect && mutex_inner.owner == Some(tid) {
            return -0xdead;
        }
        mutex_inner.wait_queue.push_back(current_task().unwrap());
        drop(mutex_inner);
        block_current_and_run_next();
        // the ownership is transferred to this thread by unlock
        self.inner.exclusive_access().owner = Some(tid);
        0
    } else {
        mutex_inner.locked = true;
        mutex_inner.owner = Some(tid);
        0
    }
}
```

`unlock` 清空 owner；若有等待者，则把锁的所有权移交给被唤醒的线程
（`locked` 保持 true，由被唤醒者在 `lock` 返回前写入 `owner`）。

### 2.2 基于银行家算法的信号量死锁检测

简单地在「等待图」中找环并不正确：例如 4 个线程、2 类资源各 2 份的场景中，
虽然等待关系存在环，但环外的线程会释放资源，最终都能推进。因此采用
**银行家算法的安全性检查**：若按当前请求把线程置为等待后，仍存在一个安全序列，
则允许等待；否则判定为死锁。

每个进程维护：

```rust
/// whether deadlock detection is enabled
pub deadlock_detect: bool,
/// number of semaphore units held by each thread: `sem_alloc[tid][sem_id]`
pub sem_alloc: Vec<Vec<usize>>,
```

线程在 `down` 成功（或阻塞后被唤醒）时记录分配，在 `up` 时减少分配。
检查流程位于 `task/mod.rs` 的 `deadlock_check_on_sem`：

1. 统计每个信号量的可用量 `work[sem]`（当前 `count` 的正数部分）；
2. 从各信号量的等待队列构造每个线程的待满足请求 `request[tid]`；
3. 反复扫描：没有待满足请求、或请求的资源当前可用的线程可以「完成」，
   完成时把它持有的所有资源归还到 `work`；
4. 若请求线程本身能完成，说明无死锁，返回 `false`；
5. 否则收集所有无法完成的线程，把它们从各等待队列中移除，给除当前线程外的
   受害者置 `deadlock_aborted` 标志并重新加入就绪队列，当前线程直接返回
   `-0xdead`。

线程被强制唤醒后，`Semaphore::down` 通过读取 `deadlock_aborted` 标志返回
`-0xdead`，用户程序据此退出：

```rust
pub fn down(&self) -> isize {
    let mut inner = self.inner.exclusive_access();
    inner.count -= 1;
    if inner.count < 0 {
        inner.wait_queue.push_back(current_task().unwrap());
        drop(inner);
        if deadlock_check_on_sem(self.id) {
            // the checker has removed this task from the wait queue
            return -0xdead;
        }
        block_current_and_run_next();
        if take_deadlock_abort() {
            return -0xdead;
        }
    }
    record_sem_alloc(self.id);
    0
}
```

如果只让请求者返回 `-0xdead` 而不唤醒环中的其他线程，它们将永远阻塞，
父进程的 `waittid` 也就无法结束，因此必须把整个「无法完成集合」一起唤醒。

### 2.3 打开检测

```rust
pub fn sys_enable_deadlock_detect(enabled: usize) -> isize {
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    process_inner.deadlock_detect = enabled != 0;
    process_inner.ensure_sem_alloc();
    0
}
```

## 3. 测试结果

```sh
make -C ci-user test CHAPTER=8
```

与死锁检测相关的三个测例：

- `ch8_deadlock_mutex1`：同一线程对阻塞互斥锁重复加锁，第二次返回
  `-0xdead`，输出 `deadlock test mutex 1 OK!`；
- `ch8_deadlock_sem1`：3 个线程各持有一份资源后再请求其他资源，形成环，
  内核检测到死锁并唤醒所有相关线程，至少一个线程以非 0 退出，
  输出 `deadlock test semaphore 1 OK!`；
- `ch8_deadlock_sem2`：4 个线程、两类资源各 2 份，虽然等待关系成环，
  但资源最终可以释放，**不应**误报死锁，所有线程正常退出，
  输出 `deadlock test semaphore 2 OK!`。

此外 `ch7b_pipetest`、`ch8b_mpsc_sem`、`ch8b_phil_din_mutex`、
`ch8b_race_adder_mutex_spin`、`ch8b_sync_sem`、`ch8b_test_condvar`、
`ch8b_threads`、`ch8b_threads_arg` 等并发测例全部通过，最终输出
`ch8 Usertests passed!`。
