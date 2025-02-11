use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use tracing::instrument;
use tracing::info;
use tracing::error;
use metrics::counter;

#[derive(Debug)]
pub struct ThreadPool {
    workers: Vec<Worker>,
    sender: Option<mpsc::Sender<Job>>,
}

type Job = Box<dyn FnOnce() + Send + 'static>;

impl ThreadPool {
    #[instrument]
    pub fn new(size: usize) -> ThreadPool {
        assert!(size > 0);
        info!("Creating thread pool with {} workers", size);

        let (sender, receiver) = mpsc::channel();
        let receiver = Arc::new(Mutex::new(receiver));
        let mut workers = Vec::with_capacity(size);

        for id in 0..size {
            info!("Creating worker {}", id);
            workers.push(Worker::new(id, Arc::clone(&receiver)));
        }

        ThreadPool {
            workers,
            sender: Some(sender),
        }
    }

    #[instrument(skip(f))]
    pub fn execute<F>(&self, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        let job = Box::new(f);
        if let Err(e) = self.sender.as_ref().unwrap().send(job) {
            error!("Failed to send job to worker: {}", e);
            counter!("job_send_errors_total", 1);
        }
    }
}

impl Drop for ThreadPool {
    fn drop(&mut self) {
        info!("Shutting down thread pool");
        drop(self.sender.take());

        for worker in &mut self.workers {
            info!("Shutting down worker {}", worker.id);
            if let Some(thread) = worker.thread.take() {
                thread.join().unwrap();
            }
        }
    }
}

#[derive(Debug)]
struct Worker {
    id: usize,
    thread: Option<thread::JoinHandle<()>>,
}

impl Worker {
    #[instrument]
    fn new(id: usize, receiver: Arc<Mutex<mpsc::Receiver<Job>>>) -> Worker {
        let thread = thread::spawn(move || loop {
            let message = receiver.lock().unwrap().recv();

            match message {
                Ok(job) => {
                    info!("Worker {id} processing job");
                    counter!("worker_jobs_total", 1, "worker_id" => id.to_string());
                    job();
                }
                Err(_) => {
                    info!("Worker {id} shutting down");
                    break;
                }
            }
        });

        Worker {
            id,
            thread: Some(thread),
        }
    }
}