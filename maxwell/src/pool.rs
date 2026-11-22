//! Threads that wait for work: a draw is too short to start threads for.
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

/// One call of `run`.
struct Job {
    /// (valid until `run` returns, which waits for `left`)
    f: *const (dyn Fn(usize) + Sync),
    n: usize,
    next: AtomicUsize,
    left: AtomicUsize,
    panicked: AtomicBool,
    done: (Mutex<bool>, Condvar),
}

// SAFETY: `f` points to something Sync that outlives the job's calls (see `run`)
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

impl Job {
    fn work(&self) {
        loop {
            let k = self.next.fetch_add(1, Ordering::Relaxed);
            if k >= self.n {
                return;
            }
            // SAFETY: an index below n was claimed, so `left` is not 0 and `run` still waits
            let f = unsafe { &*self.f };
            if std::panic::catch_unwind(AssertUnwindSafe(|| f(k))).is_err() {
                self.panicked.store(true, Ordering::Relaxed);
            }
            if self.left.fetch_sub(1, Ordering::AcqRel) == 1 {
                *self.done.0.lock().unwrap() = true;
                self.done.1.notify_all();
            }
        }
    }
}

struct Pool {
    queue: Mutex<Vec<Arc<Job>>>,
    wake: Condvar,
}

fn pool() -> &'static Pool {
    static POOL: OnceLock<&'static Pool> = OnceLock::new();
    POOL.get_or_init(|| {
        let p: &'static Pool = Box::leak(Box::new(Pool { queue: Mutex::new(Vec::new()), wake: Condvar::new() }));
        for k in 1..crate::pipe::threads() {
            let _ = std::thread::Builder::new().name(format!("maxwell-{k}")).spawn(move || loop {
                let job = {
                    let mut q = p.queue.lock().unwrap();
                    loop {
                        q.retain(|j| j.next.load(Ordering::Relaxed) < j.n);
                        match q.first() {
                            Some(j) => break j.clone(),
                            None => q = p.wake.wait(q).unwrap(),
                        }
                    }
                };
                job.work();
            });
        }
        p
    })
}

/// Call `f` with every number below `n`, on up to that many threads, and wait for all.
pub fn run(n: usize, f: &(dyn Fn(usize) + Sync)) {
    if n <= 1 || crate::pipe::threads() == 1 {
        (0..n).for_each(f);
        return;
    }
    let p = pool();
    // SAFETY: only the lifetime changes; the wait below keeps `f` alive while it is called
    let f: *const (dyn Fn(usize) + Sync) = unsafe { std::mem::transmute::<*const (dyn Fn(usize) + Sync + '_), *const (dyn Fn(usize) + Sync + 'static)>(f) };
    let job = Arc::new(Job { f, n, next: AtomicUsize::new(0), left: AtomicUsize::new(n), panicked: AtomicBool::new(false), done: (Mutex::new(false), Condvar::new()) });
    p.queue.lock().unwrap().push(job.clone());
    if n > 2 {
        p.wake.notify_all();
    } else {
        p.wake.notify_one();
    }
    job.work();
    let mut done = job.done.0.lock().unwrap();
    while !*done {
        done = job.done.1.wait(done).unwrap();
    }
    if job.panicked.load(Ordering::Relaxed) {
        panic!("a GPU worker panicked");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_is_called_once() {
        for n in [0, 1, 2, 7, 100] {
            let counts: Vec<AtomicUsize> = (0..n).map(|_| AtomicUsize::new(0)).collect();
            run(n, &|k| {
                counts[k].fetch_add(1, Ordering::Relaxed);
            });
            assert!(counts.iter().all(|c| c.load(Ordering::Relaxed) == 1));
        }
        // from several threads at once
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    for _ in 0..50 {
                        let sum = AtomicUsize::new(0);
                        run(20, &|k| {
                            sum.fetch_add(k, Ordering::Relaxed);
                        });
                        assert_eq!(sum.load(Ordering::Relaxed), 190);
                    }
                });
            }
        });
        assert!(std::panic::catch_unwind(|| run(3, &|k| assert!(k != 1))).is_err() || crate::pipe::threads() == 1);
    }
}
