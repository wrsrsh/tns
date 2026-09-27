//! State shared between the interactive client and the probe threads.

use std::collections::{HashSet, VecDeque};
use std::fs::File;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::cache::Cache;
use crate::remote::Shell;

#[derive(Default, Clone, Copy)]
pub struct Stats {
    pub keys: u64,
    pub predicted: u64,
    pub hit: u64,
    pub miss: u64,
    pub unpredicted: u64,
    pub learned_live: u64,
    pub learned_probe: u64,
    pub probed: u64,
}

#[derive(Default)]
pub struct ProbeQueue {
    pub queue: VecDeque<String>,
    pub seen: HashSet<String>,
}

pub struct Shared {
    pub host: String,
    pub session_id: String,
    pub shell: Shell,
    pub cache: Mutex<Cache>,
    pub stats: Mutex<Stats>,
    pub stopping: AtomicBool,
    pub probes: Mutex<ProbeQueue>,
    pub probe_cv: Condvar,
    pub size: Mutex<(usize, usize)>,
    pub cwd: Mutex<Option<String>>,
    pub log: Mutex<Option<File>>,
    pub t0: Instant,
}

impl Shared {
    pub fn log(&self, msg: &str) {
        if let Ok(mut g) = self.log.lock() {
            if let Some(f) = g.as_mut() {
                let _ = writeln!(f, "{:.3} {}", self.t0.elapsed().as_secs_f64(), msg);
            }
        }
    }

    pub fn stopping(&self) -> bool {
        self.stopping.load(Ordering::Relaxed)
    }

    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Relaxed);
        let _g = self.probes.lock().unwrap();
        self.probe_cv.notify_all();
    }

    pub fn enqueue(&self, cmd: &str, front: bool) {
        let cmd = cmd.trim();
        if cmd.is_empty() || cmd.len() > 120 || cmd.chars().any(|c| (c as u32) < 32) {
            return;
        }
        let mut q = self.probes.lock().unwrap();
        if front {
            q.queue.push_front(cmd.to_string());
        } else if !q.seen.contains(cmd) {
            q.queue.push_back(cmd.to_string());
        }
        q.seen.insert(cmd.to_string());
        self.probe_cv.notify_one();
    }

    /// Next command to probe; waits up to 0.5 s when `block`.
    pub fn next_probe(&self, block: bool) -> Option<String> {
        let mut q = self.probes.lock().unwrap();
        while q.queue.is_empty() && !self.stopping() {
            if !block {
                return None;
            }
            q = self.probe_cv.wait_timeout(q, Duration::from_millis(500)).unwrap().0;
            if q.queue.is_empty() {
                return None;
            }
        }
        q.queue.pop_front()
    }

    pub fn with_stats(&self, f: impl FnOnce(&mut Stats)) {
        if let Ok(mut s) = self.stats.lock() {
            f(&mut s);
        }
    }
}
