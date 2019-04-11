use crate::interrupt;
use crate::thread_pool::*;
use alloc::boxed::Box;
use alloc::sync::Arc;
use core::cell::UnsafeCell;
use log::*;

/// Thread executor
///
/// Per-CPU struct. Defined at global.
/// Only accessed by associated CPU with interrupt disabled.
#[derive(Default)]
pub struct Processor {
    inner: UnsafeCell<Option<ProcessorInner>>,
}

unsafe impl Sync for Processor {}

struct ProcessorInner {
    id: usize,
    proc: Option<(Tid, Box<Context>)>,
    loop_context: Box<Context>,
    manager: Arc<ThreadPool>,
}

impl Processor {
    pub const fn new() -> Self {
        Processor {
            inner: UnsafeCell::new(None),
        }
    }

    pub unsafe fn init(&self, id: usize, context: Box<Context>, manager: Arc<ThreadPool>) {
        *self.inner.get() = Some(ProcessorInner {
            id,
            proc: None,
            loop_context: context,
            manager,
        });
    }

    fn inner(&self) -> &mut ProcessorInner {
        unsafe { &mut *self.inner.get() }
            .as_mut()
            .expect("Processor is not initialized")
    }

    /// Begin running processes after CPU setup.
    ///
    /// This function never returns. It loops, doing:
    /// - choose a process to run
    /// - switch to start running that process
    /// - eventually that process transfers control
    ///   via switch back to the scheduler.
    pub fn run(&self) -> ! {
        let inner = self.inner();
        unsafe {
            interrupt::disable_and_store();
        }
        loop {
            if let Some(proc) = inner.manager.run(inner.id) {
                trace!("CPU{} begin running thread {}", inner.id, proc.0);
                inner.proc = Some(proc);
                unsafe {
                    inner
                        .loop_context
                        .switch_to(&mut *inner.proc.as_mut().unwrap().1);
                }
                let (tid, context) = inner.proc.take().unwrap();
                trace!("CPU{} stop running thread {}", inner.id, tid);
                inner.manager.stop(tid, context);
            } else {
                trace!("CPU{} idle", inner.id);
                unsafe {
                    interrupt::enable_and_wfi();
                }
                // wait for a timer interrupt
                unsafe {
                    interrupt::disable_and_store();
                }
            }
        }
    }

    /// Called by process running on this Processor.
    /// Yield and reschedule.
    ///
    /// The interrupt may be enabled.
    pub fn yield_now(&self) {
        let inner = self.inner();
        unsafe {
            let flags = interrupt::disable_and_store();
            inner
                .proc
                .as_mut()
                .unwrap()
                .1
                .switch_to(&mut *inner.loop_context);
            interrupt::restore(flags);
        }
    }

    pub fn tid(&self) -> Tid {
        self.inner().proc.as_ref().unwrap().0
    }

    pub fn context(&self) -> &Context {
        &*self.inner().proc.as_ref().unwrap().1
    }

    pub fn manager(&self) -> &ThreadPool {
        &*self.inner().manager
    }

    /// Called by timer interrupt handler.
    ///
    /// The interrupt should be disabled in the handler.
    pub fn tick(&self) {
        // If I'm idle, tid == None, need_reschedule == false.
        // Will go back to `run()` after interrupt return.
        let tid = self.inner().proc.as_ref().map(|p| p.0);
        let need_reschedule = self.manager().tick(self.inner().id, tid);
        if need_reschedule {
            self.yield_now();
        }
    }
}
