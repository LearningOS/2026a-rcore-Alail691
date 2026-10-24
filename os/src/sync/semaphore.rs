//! Semaphore

use crate::sync::UPSafeCell;
use crate::task::{
    block_current_and_run_next, current_task, deadlock_check_on_sem, record_sem_alloc,
    release_sem_alloc, take_deadlock_abort, wakeup_task, TaskControlBlock,
};
use alloc::{collections::VecDeque, sync::Arc};

/// semaphore structure
pub struct Semaphore {
    /// the id of this semaphore in the process
    pub id: usize,
    /// semaphore inner
    pub inner: UPSafeCell<SemaphoreInner>,
}

pub struct SemaphoreInner {
    pub count: isize,
    pub wait_queue: VecDeque<Arc<TaskControlBlock>>,
}

impl Semaphore {
    /// Create a new semaphore
    pub fn new(id: usize, res_count: usize) -> Self {
        trace!("kernel: Semaphore::new");
        Self {
            id,
            inner: unsafe {
                UPSafeCell::new(SemaphoreInner {
                    count: res_count as isize,
                    wait_queue: VecDeque::new(),
                })
            },
        }
    }

    /// up operation of semaphore
    pub fn up(&self) {
        trace!("kernel: Semaphore::up");
        release_sem_alloc(self.id);
        let mut inner = self.inner.exclusive_access();
        inner.count += 1;
        if inner.count <= 0 {
            if let Some(task) = inner.wait_queue.pop_front() {
                wakeup_task(task);
            }
        }
    }

    /// down operation of semaphore
    ///
    /// Returns -0xdead if the operation would cause a deadlock and deadlock
    /// detection is enabled.
    pub fn down(&self) -> isize {
        trace!("kernel: Semaphore::down");
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
}
