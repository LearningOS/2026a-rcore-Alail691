//! Implementation of process [`ProcessControlBlock`] and task(thread) [`TaskControlBlock`] management mechanism
//!
//! Here is the entry for task scheduling required by other modules
//! (such as syscall or clock interrupt).
//! By suspending or exiting the current task, you can
//! modify the task state, manage the task queue through TASK_MANAGER (in task/manager.rs) ,
//! and switch the control flow through PROCESSOR (in task/processor.rs) .
//!
//! Be careful when you see [`__switch`]. Control flow around this function
//! might not be what you expect.

mod context;
mod id;
mod manager;
mod process;
mod processor;
mod signal;
mod switch;
#[allow(clippy::module_inception)]
mod task;

use self::id::TaskUserRes;
use crate::fs::{open_file, OpenFlags};
use crate::task::manager::add_stopping_task;
use crate::timer::remove_timer;
use alloc::{sync::Arc, vec, vec::Vec};
use lazy_static::*;
use manager::fetch_task;
use process::ProcessControlBlock;
use switch::__switch;

pub use context::TaskContext;
pub use id::{kstack_alloc, pid_alloc, KernelStack, PidHandle, IDLE_PID};
pub use manager::{add_task, pid2process, remove_from_pid2process, remove_task, wakeup_task};
pub use processor::{
    current_kstack_top, current_process, current_task, current_trap_cx, current_trap_cx_user_va,
    current_user_token, run_tasks, schedule, take_current_task,
};
pub use signal::SignalFlags;
pub use task::{TaskControlBlock, TaskStatus};

/// Make current task suspended and switch to the next task
pub fn suspend_current_and_run_next() {
    // There must be an application running.
    let task = take_current_task().unwrap();

    // ---- access current TCB exclusively
    let mut task_inner = task.inner_exclusive_access();
    let task_cx_ptr = &mut task_inner.task_cx as *mut TaskContext;
    // Change status to Ready
    task_inner.task_status = TaskStatus::Ready;
    drop(task_inner);
    // ---- release current TCB

    // push back to ready queue.
    add_task(task);
    // jump to scheduling cycle
    schedule(task_cx_ptr);
}

/// Make current task blocked and switch to the next task.
pub fn block_current_and_run_next() {
    let task = take_current_task().unwrap();
    let mut task_inner = task.inner_exclusive_access();
    let task_cx_ptr = &mut task_inner.task_cx as *mut TaskContext;
    task_inner.task_status = TaskStatus::Blocked;
    drop(task_inner);
    schedule(task_cx_ptr);
}

use crate::board::QEMUExit;

/// Exit the current 'Running' task and run the next task in task list.
pub fn exit_current_and_run_next(exit_code: i32) {
    trace!(
        "kernel: pid[{}] exit_current_and_run_next",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    // take from Processor
    let task = take_current_task().unwrap();
    let mut task_inner = task.inner_exclusive_access();
    let process = task.process.upgrade().unwrap();
    let tid = task_inner.res.as_ref().unwrap().tid;
    // record exit code
    task_inner.exit_code = Some(exit_code);
    task_inner.res = None;
    // here we do not remove the thread since we are still using the kstack
    // it will be deallocated when sys_waittid is called
    drop(task_inner);

    // Move the task to stop-wait status, to avoid kernel stack from being freed
    if tid == 0 {
        add_stopping_task(task);
    } else {
        drop(task);
    }
    // however, if this is the main thread of current process
    // the process should terminate at once
    if tid == 0 {
        let pid = process.getpid();
        if pid == IDLE_PID {
            println!(
                "[kernel] Idle process exit with exit_code {} ...",
                exit_code
            );
            if exit_code != 0 {
                //crate::sbi::shutdown(255); //255 == -1 for err hint
                crate::board::QEMU_EXIT_HANDLE.exit_failure();
            } else {
                //crate::sbi::shutdown(0); //0 for success hint
                crate::board::QEMU_EXIT_HANDLE.exit_success();
            }
        }
        remove_from_pid2process(pid);
        let mut process_inner = process.inner_exclusive_access();
        // mark this process as a zombie process
        process_inner.is_zombie = true;
        // record exit code of main process
        process_inner.exit_code = exit_code;

        {
            // move all child processes under init process
            let mut initproc_inner = INITPROC.inner_exclusive_access();
            for child in process_inner.children.iter() {
                child.inner_exclusive_access().parent = Some(Arc::downgrade(&INITPROC));
                initproc_inner.children.push(child.clone());
            }
        }

        // deallocate user res (including tid/trap_cx/ustack) of all threads
        // it has to be done before we dealloc the whole memory_set
        // otherwise they will be deallocated twice
        let mut recycle_res = Vec::<TaskUserRes>::new();
        for task in process_inner.tasks.iter().filter(|t| t.is_some()) {
            let task = task.as_ref().unwrap();
            // if other tasks are Ready in TaskManager or waiting for a timer to be
            // expired, we should remove them.
            //
            // Mention that we do not need to consider Mutex/Semaphore since they
            // are limited in a single process. Therefore, the blocked tasks are
            // removed when the PCB is deallocated.
            trace!("kernel: exit_current_and_run_next .. remove_inactive_task");
            remove_inactive_task(Arc::clone(&task));
            let mut task_inner = task.inner_exclusive_access();
            if let Some(res) = task_inner.res.take() {
                recycle_res.push(res);
            }
        }
        // dealloc_tid and dealloc_user_res require access to PCB inner, so we
        // need to collect those user res first, then release process_inner
        // for now to avoid deadlock/double borrow problem.
        drop(process_inner);
        recycle_res.clear();

        let mut process_inner = process.inner_exclusive_access();
        process_inner.children.clear();
        // deallocate other data in user space i.e. program code/data section
        process_inner.memory_set.recycle_data_pages();
        // drop file descriptors
        process_inner.fd_table.clear();
        // remove all tasks
        process_inner.tasks.clear();
    }
    drop(process);
    // we do not have to save task context
    let mut _unused = TaskContext::zero_init();
    schedule(&mut _unused as *mut _);
}

lazy_static! {
    /// Creation of initial process
    ///
    /// the name "initproc" may be changed to any other app name like "usertests",
    /// but we have user_shell, so we don't need to change it.
    pub static ref INITPROC: Arc<ProcessControlBlock> = {
        let inode = open_file("ch8b_initproc", OpenFlags::RDONLY).unwrap();
        let v = inode.read_all();
        ProcessControlBlock::new(v.as_slice())
    };
}

///Add init process to the manager
pub fn add_initproc() {
    let _initproc = INITPROC.clone();
}

/// Check if the current task has any signal to handle
pub fn check_signals_of_current() -> Option<(i32, &'static str)> {
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    process_inner.signals.check_error()
}

/// Add signal to the current task
pub fn current_add_signal(signal: SignalFlags) {
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    process_inner.signals |= signal;
}

/// the inactive(blocked) tasks are removed when the PCB is deallocated.(called by exit_current_and_run_next)
pub fn remove_inactive_task(task: Arc<TaskControlBlock>) {
    remove_task(Arc::clone(&task));
    trace!("kernel: remove_inactive_task .. remove_timer");
    remove_timer(Arc::clone(&task));
}

/// The tid of the current task.
pub fn current_tid() -> usize {
    current_task()
        .unwrap()
        .inner_exclusive_access()
        .res
        .as_ref()
        .unwrap()
        .tid
}

/// Whether deadlock detection is enabled in the current process.
pub fn deadlock_detect_enabled() -> bool {
    current_process()
        .inner_exclusive_access()
        .deadlock_detect
}

/// Record that the current thread acquired one unit of semaphore `sem_id`.
pub fn record_sem_alloc(sem_id: usize) {
    let tid = current_tid();
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    inner.ensure_sem_alloc();
    if tid < inner.sem_alloc.len() && sem_id < inner.sem_alloc[tid].len() {
        inner.sem_alloc[tid][sem_id] += 1;
    }
}

/// Record that the current thread released one unit of semaphore `sem_id`.
pub fn release_sem_alloc(sem_id: usize) {
    let tid = current_tid();
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    inner.ensure_sem_alloc();
    if tid < inner.sem_alloc.len() && sem_id < inner.sem_alloc[tid].len() {
        inner.sem_alloc[tid][sem_id] = inner.sem_alloc[tid][sem_id].saturating_sub(1);
    }
}

/// Take and clear the deadlock-abort flag of the current task.
pub fn take_deadlock_abort() -> bool {
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    let flag = inner.deadlock_aborted;
    inner.deadlock_aborted = false;
    flag
}

/// Check whether blocking the current thread on semaphore `sem_id` would
/// lead to a deadlock (banker's algorithm).
///
/// If a deadlock is detected, all the threads involved are woken up with
/// their abort flag set and `true` is returned; the caller should return
/// -0xdead to the user program.
pub fn deadlock_check_on_sem(sem_id: usize) -> bool {
    let task = match current_task() {
        Some(task) => task,
        None => return false,
    };
    let tid = match task.inner_exclusive_access().res.as_ref() {
        Some(res) => res.tid,
        None => return false,
    };
    let process = task.process.upgrade().unwrap();
    let mut inner = process.inner_exclusive_access();
    if !inner.deadlock_detect {
        return false;
    }
    inner.ensure_sem_alloc();
    let ntask = inner.tasks.len();
    let nsem = inner.semaphore_list.len();
    // currently available units of every semaphore
    let mut work = vec![0usize; nsem];
    // the semaphore every thread is waiting for
    let mut request: Vec<Option<usize>> = vec![None; ntask];
    for (sid, sem) in inner.semaphore_list.iter().enumerate() {
        if let Some(sem) = sem {
            let sem_inner = sem.inner.exclusive_access();
            if sem_inner.count > 0 {
                work[sid] = sem_inner.count as usize;
            }
            for waiting in sem_inner.wait_queue.iter() {
                if let Some(waiting_tid) = waiting
                    .inner_exclusive_access()
                    .res
                    .as_ref()
                    .map(|res| res.tid)
                {
                    if waiting_tid < ntask {
                        request[waiting_tid] = Some(sid);
                    }
                }
            }
        }
    }
    // the calling thread is waiting for `sem_id`
    if tid < ntask {
        request[tid] = Some(sem_id);
    }
    // banker's algorithm: a thread with no pending request, or whose request
    // can be satisfied, will eventually release everything it holds
    let mut finish = vec![false; ntask];
    loop {
        let mut progress = false;
        for t in 0..ntask {
            if finish[t] || inner.tasks[t].is_none() {
                finish[t] = true;
                continue;
            }
            let can_finish = match request[t] {
                None => true,
                Some(s) => work[s] > 0,
            };
            if can_finish {
                finish[t] = true;
                for s in 0..nsem {
                    work[s] += inner.sem_alloc[t][s];
                }
                progress = true;
            }
        }
        if !progress {
            break;
        }
    }
    if finish[tid] {
        return false;
    }
    // deadlock detected: abort all threads that cannot finish
    let victims: Vec<usize> = (0..ntask)
        .filter(|&t| inner.tasks[t].is_some() && !finish[t])
        .collect();
    // remove them from all the wait queues so that a later `up` will not
    // wake them up a second time
    for sem in inner.semaphore_list.iter().flatten() {
        sem.inner.exclusive_access().wait_queue.retain(|waiting| {
            match waiting.inner_exclusive_access().res.as_ref() {
                Some(res) => !victims.contains(&res.tid),
                None => true,
            }
        });
    }
    for &victim_tid in victims.iter() {
        if victim_tid == tid {
            continue;
        }
        let victim = inner.tasks[victim_tid].as_ref().unwrap().clone();
        {
            let mut victim_inner = victim.inner_exclusive_access();
            victim_inner.deadlock_aborted = true;
            victim_inner.task_status = TaskStatus::Ready;
        }
        add_task(victim);
    }
    true
}
