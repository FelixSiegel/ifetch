use crate::server::helpers::lock_mutex;
use log::error;
use std::{
    panic::{self, catch_unwind},
    sync::{
        Arc, Mutex,
        mpsc::{Receiver, Sender, channel},
    },
    thread::{Builder, JoinHandle},
};

pub type Job = Box<dyn FnOnce() + Send + 'static>;

/// Thread pool specifically dedicated to executing background chapter downloads.
pub struct DownloadPool {
    sender: Sender<Job>,
    _workers: Vec<JoinHandle<()>>,
}

impl DownloadPool {
    /// Spawns a new download pool with `threads` worker threads.
    pub fn new(threads: usize) -> Self {
        let (sender, receiver) = channel::<Job>();
        let receiver = Arc::new(Mutex::new(receiver));

        let workers = (0..threads.max(1))
            .map(|id| {
                let rx = Arc::clone(&receiver);
                Builder::new()
                    .name(format!("ifetch-dl-{}", id))
                    .spawn(move || run_worker(id, rx))
                    .expect("Failed to spawn download worker thread")
            })
            .collect();

        Self {
            sender,
            _workers: workers,
        }
    }

    /// Queues a closure to be executed by the download worker pool.
    pub fn spawn<F>(&self, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        if let Err(e) = self.sender.send(Box::new(f)) {
            error!("Failed to queue download task: {}", e);
        }
    }
}

/// Receives the next available job, releasing the mutex lock immediately.
fn next_job(receiver: &Mutex<Receiver<Job>>) -> Option<Job> {
    lock_mutex(receiver).recv().ok()
}

/// Continuous worker thread execution loop.
fn run_worker(id: usize, receiver: Arc<Mutex<Receiver<Job>>>) {
    while let Some(job) = next_job(&receiver) {
        if let Err(panic_err) = catch_unwind(panic::AssertUnwindSafe(job)) {
            error!("Download worker {} caught panic: {:?}", id, panic_err);
        }
    }
}
