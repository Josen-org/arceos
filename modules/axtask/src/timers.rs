use alloc::sync::Arc;
use core::{
    cmp::Reverse,
    hash::{Hash, Hasher},
};

use axhal::time::{TimeValue, wall_time};
use foldhash::fast::FixedState;
use kernel_guard::NoOp;
use priority_queue::PriorityQueue;

use crate::{AxTaskRef, select_run_queue};

struct TaskPtr(AxTaskRef);

impl TaskPtr {
    fn new(task: &AxTaskRef) -> Self {
        TaskPtr(task.clone())
    }
}

impl PartialEq for TaskPtr {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for TaskPtr {}

impl Hash for TaskPtr {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.0).hash(state);
    }
}

percpu_static! {
    TIMER_LIST: PriorityQueue<TaskPtr, Reverse<TimeValue>, FixedState> = PriorityQueue::with_hasher(FixedState::with_seed(0)),
}

pub fn set_alarm_wakeup(deadline: TimeValue, task: &AxTaskRef) {
    TIMER_LIST.with_current(|list| {
        list.push(TaskPtr::new(task), Reverse(deadline));
    });
}

pub fn clear_alarm_wakeup(task: &AxTaskRef) {
    TIMER_LIST.with_current(|list| {
        list.remove(&TaskPtr::new(task));
    });
}

pub fn check_events() {
    // Safety: IRQs are disabled at this time.
    let timer_list = unsafe { TIMER_LIST.current_ref_mut_raw() };
    while let Some((TaskPtr(task), _)) =
        timer_list.pop_if(|_, Reverse(deadline)| *deadline < wall_time())
    {
        select_run_queue::<NoOp>(&task).unblock_task(task, true);
    }
}
