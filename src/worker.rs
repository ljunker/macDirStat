use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, TryRecvError, unbounded};

use crate::{
    scanner::{LoadOutcome, ScanOptions, ScanOutcome, calculate_size, load_children},
    tree::NodeId,
};

#[derive(Debug)]
pub enum ScanJob {
    LoadChildren {
        generation: u64,
        node_id: NodeId,
        path: PathBuf,
        options: ScanOptions,
    },
    CalculateSize {
        generation: u64,
        node_id: NodeId,
        scan_revision: u64,
        path: PathBuf,
        options: ScanOptions,
    },
}

#[derive(Debug)]
pub enum WorkerEvent {
    SizeStarted {
        generation: u64,
        node_id: NodeId,
        scan_revision: u64,
    },
    ChildrenLoaded {
        generation: u64,
        node_id: NodeId,
        outcome: LoadOutcome,
    },
    ChildrenLoadFailed {
        generation: u64,
        node_id: NodeId,
        message: String,
    },
    SizeCalculated {
        generation: u64,
        node_id: NodeId,
        scan_revision: u64,
        outcome: ScanOutcome,
    },
}

pub struct WorkerPool {
    job_tx: Option<Sender<ScanJob>>,
    result_rx: Receiver<WorkerEvent>,
    active_generation: Arc<AtomicU64>,
    handles: Vec<JoinHandle<()>>,
}

impl WorkerPool {
    pub fn new(worker_count: usize, generation: u64) -> Result<Self> {
        let (job_tx, job_rx) = unbounded();
        let (result_tx, result_rx) = unbounded();
        let active_generation = Arc::new(AtomicU64::new(generation));
        let mut handles = Vec::with_capacity(worker_count);

        for index in 0..worker_count {
            let jobs = job_rx.clone();
            let results = result_tx.clone();
            let active = Arc::clone(&active_generation);
            let handle = thread::Builder::new()
                .name(format!("macDirStat-scan-{index}"))
                .spawn(move || worker_loop(jobs, results, active))
                .with_context(|| format!("Could not start scan worker {index}"))?;
            handles.push(handle);
        }
        drop(result_tx);

        Ok(Self {
            job_tx: Some(job_tx),
            result_rx,
            active_generation,
            handles,
        })
    }

    pub fn send(&self, job: ScanJob) -> Result<()> {
        self.job_tx
            .as_ref()
            .context("Scan worker pool is shut down")?
            .send(job)
            .context("Scan worker pool stopped unexpectedly")
    }

    pub fn try_recv(&self) -> Result<WorkerEvent, TryRecvError> {
        self.result_rx.try_recv()
    }

    pub fn set_generation(&self, generation: u64) {
        self.active_generation.store(generation, Ordering::Release);
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        self.active_generation.store(u64::MAX, Ordering::Release);
        self.job_tx.take();
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

fn worker_loop(
    jobs: Receiver<ScanJob>,
    results: Sender<WorkerEvent>,
    active_generation: Arc<AtomicU64>,
) {
    for job in jobs {
        let generation = match &job {
            ScanJob::LoadChildren { generation, .. }
            | ScanJob::CalculateSize { generation, .. } => *generation,
        };
        if active_generation.load(Ordering::Acquire) != generation {
            continue;
        }

        match job {
            ScanJob::LoadChildren {
                generation,
                node_id,
                path,
                options,
            } => match load_children(&path, options, || {
                active_generation.load(Ordering::Relaxed) != generation
            }) {
                Ok(outcome) if !outcome.cancelled => {
                    if results
                        .send(WorkerEvent::ChildrenLoaded {
                            generation,
                            node_id,
                            outcome,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    if results
                        .send(WorkerEvent::ChildrenLoadFailed {
                            generation,
                            node_id,
                            message: error.to_string(),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            },
            ScanJob::CalculateSize {
                generation,
                node_id,
                scan_revision,
                path,
                options,
            } => {
                if results
                    .send(WorkerEvent::SizeStarted {
                        generation,
                        node_id,
                        scan_revision,
                    })
                    .is_err()
                {
                    break;
                }
                let outcome = calculate_size(&path, options, || {
                    active_generation.load(Ordering::Relaxed) != generation
                });
                if !outcome.cancelled
                    && results
                        .send(WorkerEvent::SizeCalculated {
                            generation,
                            node_id,
                            scan_revision,
                            outcome,
                        })
                        .is_err()
                {
                    break;
                }
            }
        }
    }
}
