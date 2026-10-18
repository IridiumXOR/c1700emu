//! Background compilation (docs ch. 17.17). Compiling a region takes
//! 0.1-20 ms; done on the spot, it stalls the guest, and when a lot of code
//! gets hot at once (IOS's IP stack under the first pings) the stalls add
//! up to hundreds of milliseconds. A worker thread owns the Cranelift
//! module instead: the dispatcher scans the region and watches its code as
//! before, hands it over, keeps interpreting it meanwhile, and installs the
//! result at the start of a later batch.

use super::codegen::{Compiler, Link};
use super::scan::Block as GuestBlock;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

/// A region to compile. `links` is the address of its link table, which
/// the dispatcher owns (the generated code embeds it, nothing else uses it).
pub(super) struct Job {
    pub id: usize,
    pub epoch: u64,
    pub region: Vec<GuestBlock>,
    pub links: usize,
}

/// A compiled region: its code and the target of each link it uses (see
/// `Compiler::compile`), or why it couldn't be compiled.
pub(super) struct Done {
    pub id: usize,
    pub epoch: u64,
    pub result: Result<(usize, Vec<Option<u32>>), String>,
    /// Host code in the worker's module since its last reset.
    pub code_bytes: usize,
    pub time: Duration,
}

enum Request {
    Compile(Job),
    /// Free all code (the dispatcher dropped every pointer into it); jobs
    /// of older epochs are skipped from now on.
    Reset { epoch: u64 },
}

pub(super) struct Worker {
    requests: Sender<Request>,
    results: Receiver<Done>,
}

impl Worker {
    pub fn spawn() -> Self {
        let (requests, jobs) = channel::<Request>();
        let (done, results) = channel();
        std::thread::Builder::new()
            .name("jit compiler".into())
            .spawn(move || {
                // Created here: the module and its memory belong to this thread.
                let mut compiler = Compiler::new();
                let mut epoch = 0;
                for request in jobs {
                    match request {
                        Request::Reset { epoch: e } => {
                            compiler.reset();
                            epoch = e;
                        }
                        Request::Compile(job) if job.epoch < epoch => {} // made before a reset
                        Request::Compile(job) => {
                            let start = Instant::now();
                            let result = compiler.compile(&job.region, job.links as *mut Link).map(|(code, targets)| (code as usize, targets));
                            let reply = Done { id: job.id, epoch: job.epoch, result, code_bytes: compiler.code_bytes, time: start.elapsed() };
                            if done.send(reply).is_err() {
                                return; // the JIT is gone
                            }
                        }
                    }
                }
            })
            .expect("cannot start the JIT compiler thread");
        Worker { requests, results }
    }

    pub fn submit(&self, job: Job) {
        let _ = self.requests.send(Request::Compile(job));
    }

    pub fn reset(&self, epoch: u64) {
        let _ = self.requests.send(Request::Reset { epoch });
    }

    /// A finished job: if any (`wait` = false), or the next one.
    pub fn finished(&self, wait: bool) -> Option<Done> {
        if wait { self.results.recv().ok() } else { self.results.try_recv().ok() }
    }
}
