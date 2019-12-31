use std::cell::Cell;
use std::io;
use std::iter;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, Thread};
use std::time::Duration;

use crossbeam_deque::{Injector, Steal, Stealer, Worker};
use crossbeam_utils::thread::scope;
use once_cell::unsync::OnceCell;

use crate::rt::Reactor;
use crate::task::Runnable;
use crate::utils::{abort_on_panic, random, Spinlock};

thread_local! {
    /// A reference to the current machine, if the current thread runs tasks.
    static MACHINE: OnceCell<Arc<Machine>> = OnceCell::new();

    /// This flag is set to true whenever `task::yield_now()` is invoked.
    static YIELD_NOW: Cell<bool> = Cell::new(false);
}

/// Scheduler state.
struct Scheduler {
    /// Set to `true` every time before a machine blocks polling the reactor.
    progress: bool,

    /// Set to `true` while a machine is polling the reactor.
    polling: bool,

    /// Idle processors.
    processors: Vec<Processor>,

    /// Running machines.
    machines: Vec<Arc<Machine>>,
}

/// An async runtime.
pub struct Runtime {
    /// The reactor.
    reactor: Reactor,

    /// The global queue of tasks.
    injector: Injector<Runnable>,

    /// Handles to local queues for stealing work.
    stealers: Vec<Stealer<Runnable>>,

    /// The scheduler state.
    sched: Mutex<Scheduler>,

    /// The thread ID of the runtime.
    // Why a spinlock? There's only one place we set this (the `run()` preamble),
    // and one place we used this (the `notify()` function), so we really don't
    // get that much contention.
    thread: Spinlock<Thread>,
}

impl Runtime {
    /// Creates a new runtime.
    pub fn new() -> Runtime {
        let cpus = num_cpus::get().max(1);
        let processors: Vec<_> = (0..cpus).map(|_| Processor::new()).collect();
        let stealers = processors.iter().map(|p| p.worker.stealer()).collect();

        Runtime {
            reactor: Reactor::new().unwrap(),
            injector: Injector::new(),
            thread: Spinlock::new(thread::current()),
            stealers,
            sched: Mutex::new(Scheduler {
                processors,
                machines: Vec::new(),
                progress: false,
                polling: false,
            }),
        }
    }

    /// Returns a reference to the reactor.
    pub fn reactor(&self) -> &Reactor {
        &self.reactor
    }

    /// Flushes the task slot so that tasks get run more fairly.
    pub fn yield_now(&self) {
        YIELD_NOW.with(|flag| flag.set(true));
    }

    /// Schedules a task.
    pub fn schedule(&self, task: Runnable) {
        MACHINE.with(|machine| {
            // If the current thread is a worker thread, schedule it onto the current machine.
            // Otherwise, push it into the global task queue.
            match machine.get() {
                None => {
                    self.injector.push(task);
                    self.notify();
                }
                Some(m) => m.schedule(&self, task),
            }
        });
    }

    /// Runs the runtime on the current thread.
    pub fn run(&self) {
        scope(|s| {
            const DELAY_MIN: u64 = 1_250;
            const DELAY_MAX: u64 = 10_000;
            let mut delay = 0;

            *self.thread.lock() = thread::current();

            loop {
                // Get a list of new machines to start, if any need to be started.
                for m in self.make_machines() {
                    delay = DELAY_MIN;

                    s.builder()
                        .name("async-std/machine".to_string())
                        .spawn(move |_| {
                            abort_on_panic(|| {
                                let _ = MACHINE.with(|machine| machine.set(m.clone()));
                                m.run(self);
                            })
                        })
                        .expect("cannot start a machine thread");
                }

                // Sleep for a bit longer if the scheduler state hasn't changed in a while.
                delay = (delay * 2).min(DELAY_MAX);

                thread::sleep(Duration::from_micros(delay));

                // If no new work has been scheduled since the last this process ran a tick,
                // then the whole system is sleeping. In the interest of saving battery, sleep indefinitely.
                //
                // # Soundness
                //
                // The goal of this design is to ensure that blocked machines do not cause tasks to starve.
                //
                // This parker is unparked at the same time as the progress flag is set. This should be sound,
                // because if the system goes from setting the flag to not setting it, we detect that.
                // We also need to unpark whenever a notification comes in, so that if there is no machine polling the reactor,
                // we can get around to spawning a new machine.
                //
                // * If work is added to the backlog while we're parked and the is no machine polling, we gets unparked,
                //   spin for 10_000 + 5_000 + 2_500 + 1_250 = 18_750 (~20K) microseconds, and will spawn a
                //   machine because all the existing machines are blocked while doing so, then we park again.
                // * If work is added before we park and everybody is blocked, then the token will be set as described
                //   in crossbeam's docs, and we'll finish the old iteration, then claim the token, then go through
                //   scenario 1 again. The old ramp-up, plus the new ramp-up, adds up to ~40_000 microseconds before we sleep.
                // * If work is added to the blacklog while a machine is still healthy, but then the machine turns
                //   to blocking afterward, then at the last point in which the machine claimed a job, it would have
                //   unparked us, and we'll spin enough times to detect the change.
                //
                // The largest possible sleep-induced delay is adding a task between the 10ms and 5ms spots, making a 15ms delay.
                // This design also means that, as long as we either get a notification or complete a job every 20ms,
                // this design will only perform atomics ops, no locking.
                //
                // This assumes, of course, that we go through a sufficient number of iterations before parking,
                // where "sufficient" means "if it doesn't pop anything off the queue while we ramp, we mark it unhealthy."
                // Since it currently uses a bool, that means our ramp-up must be at least three iterations long:
                // once for the still-healthy machine to set it to true, once for us to set it to false, and once for us to
                // verify that it's still false.
                assert!(DELAY_MAX / DELAY_MIN > 2);
                if delay == DELAY_MAX {
                    thread::park();
                    delay = DELAY_MIN;
                }
            }
        })
        .unwrap();
    }

    /// Returns a list of machines that need to be started.
    fn make_machines(&self) -> Vec<Arc<Machine>> {
        let mut sched = self.sched.lock().unwrap();
        let mut to_start = Vec::new();

        let thread = thread::current();

        // If there is a machine that is stuck on a task and not making any progress, steal its
        // processor and set up a new machine to take over.
        for m in &mut sched.machines {
            if !m.progress.swap(false, Ordering::SeqCst) {
                let opt_p = m.processor.try_lock().and_then(|mut p| p.take());

                if let Some(p) = opt_p {
                    *m = Arc::new(Machine::new(p, thread.clone()));
                    to_start.push(m.clone());
                }
            }
        }

        // If no machine has been polling the reactor in a while, that means the runtime is
        // overloaded with work and we need to start another machine.
        if !sched.polling {
            if !sched.progress {
                if let Some(p) = sched.processors.pop() {
                    let m = Arc::new(Machine::new(p, thread.clone()));
                    to_start.push(m.clone());
                    sched.machines.push(m);
                }
            }

            sched.progress = false;
        }

        to_start
    }

    /// Unparks a thread polling the reactor.
    fn notify(&self) {
        // In case there isn't anyone polling the reactor.
        if let Some(thread) = self.thread.try_lock() {
            thread.unpark();
        }
        // In case there is someone polling the reactor.
        self.reactor.notify().unwrap();
    }

    /// Attempts to poll the reactor without blocking on it.
    ///
    /// Returns `Ok(true)` if at least one new task was woken.
    ///
    /// This function might not poll the reactor at all so do not rely on it doing anything. Only
    /// use for optimization.
    fn quick_poll(&self) -> io::Result<bool> {
        if let Ok(sched) = self.sched.try_lock() {
            if !sched.polling {
                return self.reactor.poll(Some(Duration::from_secs(0)));
            }
        }
        Ok(false)
    }
}

/// A thread running a processor.
struct Machine {
    /// Holds the processor until it gets stolen.
    processor: Spinlock<Option<Processor>>,

    /// Gets set to `true` before running every task to indicate the machine is not stuck.
    progress: AtomicBool,

    /// The thread handle of the runtime.
    runtime: Thread,
}

impl Machine {
    /// Creates a new machine running a processor.
    fn new(p: Processor, thread: Thread) -> Machine {
        Machine {
            processor: Spinlock::new(Some(p)),
            progress: AtomicBool::new(true),
            runtime: thread,
        }
    }

    /// Schedules a task onto the machine.
    fn schedule(&self, rt: &Runtime, task: Runnable) {
        match self.processor.lock().as_mut() {
            None => {
                rt.injector.push(task);
                rt.notify();
            }
            Some(p) => p.schedule(rt, task),
        }
    }

    /// Finds the next runnable task.
    fn find_task(&self, rt: &Runtime) -> Steal<Runnable> {
        let mut retry = false;

        // First try finding a task in the local queue or in the global queue.
        if let Some(p) = self.processor.lock().as_mut() {
            if let Some(task) = p.pop_task() {
                return Steal::Success(task);
            }

            match p.steal_from_global(rt) {
                Steal::Empty => {}
                Steal::Retry => retry = true,
                Steal::Success(task) => return Steal::Success(task),
            }
        }

        // Try polling the reactor, but don't block on it.
        let progress = rt.quick_poll().unwrap();

        // Try finding a task in the local queue, which might hold tasks woken by the reactor. If
        // the local queue is still empty, try stealing from other processors.
        if let Some(p) = self.processor.lock().as_mut() {
            if progress {
                if let Some(task) = p.pop_task() {
                    return Steal::Success(task);
                }
            }

            match p.steal_from_others(rt) {
                Steal::Empty => {}
                Steal::Retry => retry = true,
                Steal::Success(task) => return Steal::Success(task),
            }
        }

        if retry { Steal::Retry } else { Steal::Empty }
    }

    /// Runs the machine on the current thread.
    fn run(&self, rt: &Runtime) {
        /// Number of yields when no runnable task is found.
        const YIELDS: u32 = 3;
        /// Number of short sleeps when no runnable task in found.
        const SLEEPS: u32 = 10;
        /// Number of runs in a row before the global queue is inspected.
        const RUNS: u32 = 64;

        // The number of times the thread found work in a row.
        let mut runs = 0;
        // The number of times the thread didn't find work in a row.
        let mut fails = 0;

        loop {
            // let the scheduler know this machine is making progress.
            self.progress.store(true, Ordering::SeqCst);
            // Notify the runtime to keep track of how long this takes,
            // in case it blocks.
            self.runtime.unpark();

            // Check if `task::yield_now()` was invoked and flush the slot if so.
            YIELD_NOW.with(|flag| {
                if flag.replace(false) {
                    if let Some(p) = self.processor.lock().as_mut() {
                        p.flush_slot(rt);
                    }
                }
            });

            // After a number of runs in a row, do some work to ensure no task is left behind
            // indefinitely. Poll the reactor, steal tasks from the global queue, and flush the
            // task slot.
            if runs >= RUNS {
                runs = 0;
                rt.quick_poll().unwrap();

                if let Some(p) = self.processor.lock().as_mut() {
                    if let Steal::Success(task) = p.steal_from_global(rt) {
                        p.schedule(rt, task);
                    }

                    p.flush_slot(rt);
                }
            }

            // Try to find a runnable task.
            if let Steal::Success(task) = self.find_task(rt) {
                task.run();
                runs += 1;
                fails = 0;
                continue;
            }

            fails += 1;

            // Check if the processor was stolen.
            if self.processor.lock().is_none() {
                break;
            }

            // Yield the current thread a few times.
            if fails <= YIELDS {
                thread::yield_now();
                continue;
            }

            // Put the current thread to sleep a few times.
            if fails <= YIELDS + SLEEPS {
                let opt_p = self.processor.lock().take();
                thread::sleep(Duration::from_micros(10));
                *self.processor.lock() = opt_p;
                continue;
            }

            let mut sched = rt.sched.lock().unwrap();

            // One final check for available tasks while the scheduler is locked.
            if let Some(task) = iter::repeat_with(|| self.find_task(rt))
                .find(|s| !s.is_retry())
                .and_then(|s| s.success())
            {
                self.schedule(rt, task);
                continue;
            }

            // If another thread is already blocked on the reactor, there is no point in keeping
            // the current thread around since there is too little work to do.
            if sched.polling {
                break;
            }

            // Take out the machine associated with the current thread.
            let m = match sched
                .machines
                .iter()
                .position(|elem| ptr::eq(&**elem, self))
            {
                None => break, // The processor was stolen.
                Some(pos) => sched.machines.swap_remove(pos),
            };

            // Unlock the schedule poll the reactor until new I/O events arrive.
            sched.polling = true;
            drop(sched);
            rt.reactor.poll(None).unwrap();

            // Lock the scheduler again and re-register the machine.
            sched = rt.sched.lock().unwrap();
            sched.polling = false;
            sched.machines.push(m);
            sched.progress = true;

            runs = 0;
            fails = 0;
        }

        // When shutting down the thread, take the processor out if still available.
        let opt_p = self.processor.lock().take();

        // Return the processor to the scheduler and remove the machine.
        if let Some(p) = opt_p {
            let mut sched = rt.sched.lock().unwrap();
            sched.processors.push(p);
            sched.machines.retain(|elem| !ptr::eq(&**elem, self));
        }
    }
}

struct Processor {
    /// The local task queue.
    worker: Worker<Runnable>,

    /// Contains the next task to run as an optimization that skips the queue.
    slot: Option<Runnable>,
}

impl Processor {
    /// Creates a new processor.
    fn new() -> Processor {
        Processor {
            worker: Worker::new_fifo(),
            slot: None,
        }
    }

    /// Schedules a task to run on this processor.
    fn schedule(&mut self, rt: &Runtime, task: Runnable) {
        match self.slot.replace(task) {
            None => {}
            Some(task) => {
                self.worker.push(task);
                rt.notify();
            }
        }
    }

    /// Flushes a task from the slot into the local queue.
    fn flush_slot(&mut self, rt: &Runtime) {
        if let Some(task) = self.slot.take() {
            self.worker.push(task);
            rt.notify();
        }
    }

    /// Pops a task from this processor.
    fn pop_task(&mut self) -> Option<Runnable> {
        self.slot.take().or_else(|| self.worker.pop())
    }

    /// Steals a task from the global queue.
    fn steal_from_global(&mut self, rt: &Runtime) -> Steal<Runnable> {
        rt.injector.steal_batch_and_pop(&self.worker)
    }

    /// Steals a task from other processors.
    fn steal_from_others(&mut self, rt: &Runtime) -> Steal<Runnable> {
        // Pick a random starting point in the list of queues.
        let len = rt.stealers.len();
        let start = random(len as u32) as usize;

        // Create an iterator over stealers that starts from the chosen point.
        let (l, r) = rt.stealers.split_at(start);
        let stealers = r.iter().chain(l.iter());

        // Try stealing a batch of tasks from each queue.
        stealers
            .map(|s| s.steal_batch_and_pop(&self.worker))
            .collect()
    }
}
