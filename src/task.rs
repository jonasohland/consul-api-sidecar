use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::Poll,
};

use anyhow::Result;
use futures::{channel::oneshot, FutureExt};
use tokio::task::JoinHandle;

pub struct TaskWrapper<T> {
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<T>>,
    done: Arc<AtomicBool>,
}

impl<T> TaskWrapper<T> {
    pub async fn shutdown(&mut self) -> Result<T> {
        if let Some(shutdown) = self.shutdown.take() {
            shutdown.send(()).ok();
            if let Some(task) = self.task.take() {
                return Ok(task.await?);
            }
        }
        Err(anyhow::anyhow!("already shut down"))
    }

    pub async fn wait(&mut self) -> Result<Option<T>> {
        match &mut self.task {
            Some(task) => {
                let result = task.await?;
                drop(self.task.take());
                Ok(Some(result))
            }
            None => Ok(None),
        }
    }

    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }
}

impl<T> Drop for TaskWrapper<T> {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            shutdown.send(()).ok();
        }
    }
}

pub struct ShutdownHandle {
    done: Arc<AtomicBool>,
    shutdown: oneshot::Receiver<()>,
}

impl std::future::Future for ShutdownHandle {
    type Output = ();
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        match self.shutdown.poll_unpin(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(_) => Poll::Ready(()),
        }
    }
}

impl Drop for ShutdownHandle {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
    }
}

pub fn start_task<F, R, O>(fun: F) -> TaskWrapper<O>
where
    F: FnOnce(ShutdownHandle) -> R,
    R: std::future::Future<Output = O> + Send + 'static,
    O: Send + Sync + 'static,
{
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let done = Arc::new(AtomicBool::new(false));
    TaskWrapper {
        shutdown: Some(shutdown_tx),
        task: Some(tokio::task::spawn(fun(ShutdownHandle {
            done: done.clone(),
            shutdown: shutdown_rx,
        }))),
        done,
    }
}
