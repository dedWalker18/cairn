//! Supervision of worker processes: spawn on demand, enforce per-request
//! deadlines with a watchdog (SIGKILL), respawn after any failure, reap when
//! idle so models do not sit in RAM.

use std::io::{BufReader, BufWriter};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use cairn_core::ipc;
use serde::de::DeserializeOwned;
use serde::Serialize;

pub struct Worker {
    kind: &'static str,
    child: Option<Child>,
    stdin: Option<BufWriter<ChildStdin>>,
    stdout: Option<BufReader<ChildStdout>>,
    deadline: Arc<Mutex<Option<Instant>>>,
    /// Bumped on every spawn and kill; a watchdog exits when it changes.
    generation: Arc<AtomicU64>,
    /// pidfd of the running worker, -1 when none.
    pid: Arc<AtomicI32>,
    last_used: Instant,
    threads: usize,
}

impl Worker {
    pub fn new(kind: &'static str, threads: usize) -> Worker {
        Worker {
            kind,
            child: None,
            stdin: None,
            stdout: None,
            deadline: Arc::new(Mutex::new(None)),
            generation: Arc::new(AtomicU64::new(0)),
            pid: Arc::new(AtomicI32::new(-1)),
            last_used: Instant::now(),
            threads,
        }
    }

    fn spawn(&mut self) -> Result<()> {
        // /proc/self/exe: the worker is always the same build as the daemon,
        // even if the binary on disk was replaced by an upgrade.
        let mut child = Command::new("/proc/self/exe")
            .args(["worker", self.kind])
            .env("RAYON_NUM_THREADS", self.threads.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("spawning {} worker", self.kind))?;
        self.stdin = Some(BufWriter::new(child.stdin.take().context("stdin")?));
        self.stdout = Some(BufReader::new(child.stdout.take().context("stdout")?));
        let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id() as libc::pid_t, 0) } as i32;
        if pidfd < 0 {
            let _ = child.kill();
            let _ = child.wait();
            bail!("pidfd_open failed (Linux 5.3+ required)");
        }
        self.close_pidfd();
        self.pid.store(pidfd, Ordering::SeqCst);
        let my_gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.child = Some(child);
        let (deadline, generation, pid) = (self.deadline.clone(), self.generation.clone(), self.pid.clone());
        std::thread::Builder::new().name(format!("watchdog-{}", self.kind)).spawn(move || {
            while generation.load(Ordering::SeqCst) == my_gen {
                std::thread::sleep(Duration::from_millis(200));
                let expired = deadline.lock().map(|d| d.is_some_and(|t| Instant::now() > t)).unwrap_or(false);
                if expired {
                    // Signal through a pidfd: it names this exact process, so a
                    // recycled pid can never be hit.
                    let fd = pid.load(Ordering::SeqCst);
                    if fd >= 0 {
                        unsafe {
                            libc::syscall(libc::SYS_pidfd_send_signal, fd, libc::SIGKILL, std::ptr::null::<libc::siginfo_t>(), 0);
                        }
                    }
                    break;
                }
            }
        })?;
        tracing::debug!(kind = self.kind, "worker started");
        Ok(())
    }

    /// Start the process if needed. Must be called from a long-lived thread:
    /// PR_SET_PDEATHSIG fires when the *spawning thread* exits, not the
    /// process, so a worker spawned from a short-lived thread is killed with it.
    pub fn ensure_started(&mut self) -> Result<()> {
        if self.child.is_none() {
            self.spawn()?;
        }
        Ok(())
    }

    pub fn kill(&mut self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut d) = self.deadline.lock() {
            *d = None;
        }
        self.stdin = None;
        self.stdout = None;
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.close_pidfd();
    }

    fn close_pidfd(&mut self) {
        let fd = self.pid.swap(-1, Ordering::SeqCst);
        if fd >= 0 {
            // The watchdog may still hold the number; it only uses it while the
            // generation matches, and kill() bumped the generation first.
            unsafe {
                libc::close(fd);
            }
        }
    }

    pub fn running(&self) -> bool {
        self.child.is_some()
    }

    pub fn idle_for(&self) -> Duration {
        self.last_used.elapsed()
    }

    /// One request/response round trip. Any failure (timeout, crash,
    /// protocol error) kills the worker; the next call starts a fresh one.
    pub fn call<Q: Serialize, R: DeserializeOwned>(&mut self, req: &Q, payload: &[u8], timeout: Duration) -> Result<(R, Vec<u8>)> {
        if self.child.is_none() {
            bail!("{} worker not running (ensure_started was not called)", self.kind);
        }
        self.last_used = Instant::now();
        *self.deadline.lock().unwrap() = Some(Instant::now() + timeout);
        let r = (|| {
            ipc::write_frame(self.stdin.as_mut().unwrap(), req, payload)?;
            match ipc::read_frame::<_, R>(self.stdout.as_mut().unwrap())? {
                Some(v) => Ok(v),
                None => bail!("{} worker exited (crash, timeout or memory limit)", self.kind),
            }
        })();
        *self.deadline.lock().unwrap() = None;
        self.last_used = Instant::now();
        if r.is_err() {
            self.kill();
        }
        r
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.kill();
    }
}
