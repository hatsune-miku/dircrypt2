use std::{
    collections::VecDeque,
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

#[derive(Default)]
struct State {
    phase: String,
    done: u64,
    total: u64,
    current: String,
    stop: bool,
    generation: u64,
    enabled: bool,
    began: Option<Instant>,
}

#[derive(Clone)]
pub struct Progress(Arc<(Mutex<State>, Condvar)>);
pub struct Reporter {
    progress: Progress,
    thread: Option<thread::JoinHandle<()>>,
}

impl Reporter {
    pub fn new(quiet: bool) -> Self {
        let progress = Progress(Arc::new((
            Mutex::new(State {
                enabled: !quiet,
                ..Default::default()
            }),
            Condvar::new(),
        )));
        let copy = progress.clone();
        let thread = (!quiet).then(|| {
            thread::spawn(move || {
                let (lock, event) = &*copy.0;
                let mut generation = u64::MAX;
                let mut started = Instant::now();
                let mut samples = VecDeque::new();
                loop {
                    let state = lock.lock().unwrap();
                    if state.stop {
                        break;
                    }
                    let now = Instant::now();
                    if state.generation != generation {
                        generation = state.generation;
                        started = now;
                        samples.clear();
                        samples.push_back((now, 0u64));
                    }
                    if !state.phase.is_empty() && now.duration_since(started).as_secs_f64() >= 0.9 {
                        samples.push_back((now, state.done));
                        while samples.len() > 2
                            && now.duration_since(samples[1].0).as_secs_f64() >= 5.0
                        {
                            samples.pop_front();
                        }
                        let (when, count) = samples[0];
                        let elapsed = now.duration_since(started).as_secs_f64();
                        let span = now.duration_since(when).as_secs_f64();
                        let speed = if span >= 1.0 {
                            format!(
                                "recent {:.0}/s | avg {:.0}/s",
                                state.done.saturating_sub(count) as f64 / span,
                                state.done as f64 / elapsed
                            )
                        } else {
                            "rate warming up".into()
                        };
                        eprintln!(
                            "[INFO] {} | {}/{}{} | {} | {:.1}s | {}",
                            state.phase,
                            state.done,
                            if state.total == 0 {
                                "?".into()
                            } else {
                                state.total.to_string()
                            },
                            if state.total > 0 {
                                format!(" ({:.1}%)", state.done as f64 * 100.0 / state.total as f64)
                            } else {
                                String::new()
                            },
                            speed,
                            elapsed,
                            state.current
                        );
                    }
                    let _ = event.wait_timeout(state, Duration::from_secs(1)).unwrap();
                }
            })
        });
        Self { progress, thread }
    }
    pub fn progress(&self) -> Progress {
        self.progress.clone()
    }
}

impl Drop for Reporter {
    fn drop(&mut self) {
        self.progress.0.0.lock().unwrap().stop = true;
        self.progress.0.1.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Progress {
    pub fn phase(&self, phase: &str, total: u64) {
        let mut state = self.0.0.lock().unwrap();
        if state.enabled {
            if let Some(started) = state.began {
                eprintln!(
                    "[INFO] {} | processed {} | {:.2}s",
                    state.phase,
                    state.done,
                    started.elapsed().as_secs_f64()
                );
            }
            eprintln!(
                "[INFO] {phase} | 0/{}",
                if total > 0 {
                    total.to_string()
                } else {
                    "?".into()
                }
            );
        }
        state.phase = phase.into();
        state.done = 0;
        state.total = total;
        state.current.clear();
        state.generation += 1;
        state.began = Some(Instant::now());
        self.0.1.notify_all();
    }
    pub fn advance(&self, count: u64) {
        self.0.0.lock().unwrap().done += count;
    }
    pub fn current(&self, name: &str) {
        self.0.0.lock().unwrap().current = name
            .chars()
            .take(160)
            .map(|c| if c.is_control() { '�' } else { c })
            .collect();
    }
}
