//! Executor for processing pending effects
//!
//! This module provides effect processing with automatic debouncing and background execution:
//!
//! - **Effect Loop**: A background thread that processes effects with debouncing.
//!   When signals emit, the loop is notified and waits for a debounce period before processing.
//!   New notifications during debounce reset the timer (up to a maximum wait time).
//!
//! - **Background Worker**: Skippable effects that exceed the time budget are deferred
//!   to a background worker thread, allowing the main effect loop to stay responsive.
//!
//! ## Architecture
//!
//! The executor uses channels and condition variables for efficient event-driven scheduling:
//! 1. When signals emit, `schedule_effect_processing()` sends a notification
//! 2. The effect loop receives notifications and applies debouncing
//! 3. Effects are processed with a time budget (~16ms for 60fps)
//! 4. Skippable effects that don't fit in the budget are sent to the background worker
//!
//! ## Usage
//!
//! ```ignore
//! // Start the effect loop with default settings
//! EffectLoop::new().spawn();
//!
//! // Or with custom configuration
//! EffectLoop::new()
//!     .debounce(Duration::from_millis(2))
//!     .max_debounce(Duration::from_millis(10))
//!     .budget(Duration::from_millis(8))
//!     .spawn_fn(|f| {
//!         std::thread::Builder::new()
//!             .name("effect-loop".into())
//!             .stack_size(1024 * 1024)
//!             .spawn(f)
//!             .unwrap()
//!     })
//!     .spawn();
//!
//! // Or for sync applications/tests, just call flush_effects() manually
//! signal.emit();
//! flush_effects();
//! ```

use crate::arena::EffectId;
use indexmap::IndexSet;
use std::sync::mpsc::{self, Sender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Global sender for notifying the effect loop.
///
/// This is lazily initialized when `EffectLoop::spawn()` is called.
/// If not initialized, `notify_effect_loop()` is a no-op.
static EFFECT_NOTIFIER: OnceLock<Sender<()>> = OnceLock::new();

/// Background worker for processing deferred (skippable) effects.
///
/// Uses a shared IndexSet for deduplication under load.
static BACKGROUND_WORKER: OnceLock<BackgroundWorker> = OnceLock::new();

/// Notify the effect loop that effects need processing.
///
/// This is called by `schedule_effect_processing()` to wake up the effect loop.
/// If the effect loop hasn't been spawned, this is a no-op.
///
/// This function is non-blocking and safe to call from any context.
pub fn notify_effect_loop() {
    if let Some(sender) = EFFECT_NOTIFIER.get() {
        // Ignore send errors (receiver dropped means loop has stopped)
        let _ = sender.send(());
    }
}

/// Default debounce delay for effect processing.
///
/// After receiving a notification, the loop waits this long before processing.
/// New notifications during this period reset the timer.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(4);

/// Maximum wait time for debouncing.
///
/// Even if notifications keep arriving, we'll process after this duration
/// to prevent unbounded latency.
pub const DEFAULT_MAX_DEBOUNCE: Duration = Duration::from_millis(16);

/// Default time budget for effect processing (~60fps).
pub const DEFAULT_BUDGET: Duration = Duration::from_millis(16);

/// Builder for configuring and spawning the effect processing loop.
///
/// # Example
///
/// ```ignore
/// use std::time::Duration;
///
/// // Default configuration
/// EffectLoop::new().spawn();
///
/// // Custom configuration
/// EffectLoop::new()
///     .debounce(Duration::from_millis(2))
///     .max_debounce(Duration::from_millis(10))
///     .budget(Duration::from_millis(8))
///     .spawn_fn(|f| {
///         std::thread::Builder::new()
///             .name("my-effect-loop".into())
///             .spawn(f)
///             .unwrap()
///     })
///     .spawn();
/// ```
#[allow(clippy::type_complexity)]
pub struct EffectLoop {
    debounce: Duration,
    max_debounce: Duration,
    budget: Duration,
    spawn_fn: Option<Box<dyn FnOnce(Box<dyn FnOnce() + Send>) -> JoinHandle<()> + Send>>,
}

impl Default for EffectLoop {
    fn default() -> Self {
        Self::new()
    }
}

impl EffectLoop {
    /// Create a new effect loop builder with default settings.
    ///
    /// Defaults:
    /// - `debounce`: 4ms
    /// - `max_debounce`: 16ms
    /// - `budget`: 16ms
    /// - `spawn_fn`: `std::thread::spawn`
    pub fn new() -> Self {
        Self {
            debounce: DEFAULT_DEBOUNCE,
            max_debounce: DEFAULT_MAX_DEBOUNCE,
            budget: DEFAULT_BUDGET,
            spawn_fn: None,
        }
    }

    /// Set the debounce delay.
    ///
    /// After receiving a notification, the loop waits this long before processing.
    /// New notifications during this period reset the timer (up to `max_debounce`).
    ///
    /// Default: 4ms
    pub fn debounce(mut self, duration: Duration) -> Self {
        self.debounce = duration;
        self
    }

    /// Set the maximum debounce wait time.
    ///
    /// Even if notifications keep arriving, processing will start after this duration.
    /// This prevents unbounded latency under constant load.
    ///
    /// Default: 16ms
    pub fn max_debounce(mut self, duration: Duration) -> Self {
        self.max_debounce = duration;
        self
    }

    /// Set the time budget for effect processing.
    ///
    /// Effects are processed until this budget is exceeded. Skippable effects
    /// that don't fit are deferred to the background worker.
    ///
    /// Default: 16ms (~60fps)
    pub fn budget(mut self, duration: Duration) -> Self {
        self.budget = duration;
        self
    }

    /// Set a custom thread spawning function.
    ///
    /// Use this to configure thread properties like name, stack size, or priority.
    ///
    /// # Example
    ///
    /// ```ignore
    /// EffectLoop::new()
    ///     .spawn_fn(|f| {
    ///         std::thread::Builder::new()
    ///             .name("effect-loop".into())
    ///             .stack_size(2 * 1024 * 1024)
    ///             .spawn(f)
    ///             .unwrap()
    ///     })
    ///     .spawn();
    /// ```
    pub fn spawn_fn<F>(mut self, f: F) -> Self
    where
        F: FnOnce(Box<dyn FnOnce() + Send>) -> JoinHandle<()> + Send + 'static,
    {
        self.spawn_fn = Some(Box::new(f));
        self
    }

    /// Spawn the effect processing loop.
    ///
    /// This starts a background thread that:
    /// 1. Waits for notification that effects need processing
    /// 2. Applies debouncing (resets on new notifications, up to max wait)
    /// 3. Processes effects with a time budget
    /// 4. Defers skippable effects that exceed budget to background worker
    ///
    /// Returns a handle to the spawned thread.
    ///
    /// ## Debouncing
    ///
    /// The loop uses a resetting debounce: each new notification resets the timer.
    /// This batches rapid signal emissions efficiently. However, a maximum wait time
    /// ensures we don't defer processing indefinitely under constant load.
    ///
    /// ## Battery Efficiency
    ///
    /// When no effects are pending, the loop blocks on the channel receiver,
    /// consuming zero CPU cycles.
    pub fn spawn(self) -> JoinHandle<()> {
        let (tx, rx) = mpsc::channel::<()>();

        // Store the sender globally so notify_effect_loop() can use it
        let _ = EFFECT_NOTIFIER.set(tx);

        // Ensure background worker is initialized
        let _ = BACKGROUND_WORKER.get_or_init(|| BackgroundWorker::new(self.spawn_fn.is_some()));

        let debounce = self.debounce;
        let max_debounce = self.max_debounce;
        let budget = self.budget;

        let loop_fn: Box<dyn FnOnce() + Send> = Box::new(move || {
            effect_loop(rx, debounce, max_debounce, budget);
        });

        match self.spawn_fn {
            Some(spawn_fn) => spawn_fn(loop_fn),
            None => thread::spawn(loop_fn),
        }
    }
}

/// The main effect processing loop.
fn effect_loop(
    rx: mpsc::Receiver<()>,
    debounce: Duration,
    max_debounce: Duration,
    budget: Duration,
) {
    let mut must_run = Vec::with_capacity(64);
    let mut skippable = Vec::with_capacity(64);

    loop {
        // Wait for first notification (blocking)
        if rx.recv().is_err() {
            // Channel closed, exit loop
            break;
        }

        // Debounce: wait for more notifications, resetting timer on each
        let debounce_start = Instant::now();
        loop {
            // Check if we've exceeded max wait time
            if debounce_start.elapsed() >= max_debounce {
                break;
            }

            // Calculate remaining debounce time
            let remaining_max = max_debounce.saturating_sub(debounce_start.elapsed());
            let timeout = debounce.min(remaining_max);

            // Try to receive with timeout
            match rx.recv_timeout(timeout) {
                Ok(()) => {
                    // Got another notification, continue debouncing
                    // (the loop will check max_debounce at the top)
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // Debounce period elapsed without new notifications
                    break;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // Channel closed, exit the outer loop
                    return;
                }
            }
        }

        // Drain any remaining notifications that arrived during debounce
        loop {
            match rx.try_recv() {
                Ok(()) => continue,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        // Wait for any active transactions to complete
        crate::transaction::wait_for_transactions();

        // Process effects with budget
        crate::effect::flush_effects_with_budget(budget, &mut must_run, &mut skippable);

        // Defer skippable effects that didn't fit in the budget to background worker
        if !skippable.is_empty() {
            if let Some(worker) = BACKGROUND_WORKER.get() {
                worker.submit(skippable.drain(..));
            }
        }
    }
}

/// Background worker for processing deferred effects.
///
/// Uses an IndexSet for automatic deduplication - if the same effect is
/// submitted multiple times before being processed, it only runs once.
struct BackgroundWorker {
    inner: Arc<BackgroundWorkerInner>,
    #[allow(dead_code)]
    handle: JoinHandle<()>,
}

struct BackgroundWorkerInner {
    /// Pending effects to process, using IndexSet for O(1) dedup
    pending: Mutex<IndexSet<EffectId>>,
    /// Condition variable to wake the worker
    condvar: Condvar,
}

impl BackgroundWorker {
    fn new(has_custom_spawn: bool) -> Self {
        let inner = Arc::new(BackgroundWorkerInner {
            pending: Mutex::new(IndexSet::new()),
            condvar: Condvar::new(),
        });

        let worker_inner = Arc::clone(&inner);
        let worker_fn: Box<dyn FnOnce() + Send> = Box::new(move || {
            Self::worker_loop(worker_inner);
        });

        // Use default spawn for background worker
        // (custom spawn_fn is primarily for the main effect loop)
        let handle = if has_custom_spawn {
            // If user provided custom spawn, they likely want consistent thread config
            // but background worker is less critical, use default
            thread::spawn(worker_fn)
        } else {
            thread::spawn(worker_fn)
        };

        BackgroundWorker { inner, handle }
    }

    /// Submit effects to be processed in the background.
    fn submit(&self, effects: impl Iterator<Item = EffectId>) {
        {
            let mut pending = self.inner.pending.lock().unwrap();
            pending.extend(effects);
        }
        self.inner.condvar.notify_one();
    }

    /// The background worker loop.
    fn worker_loop(inner: Arc<BackgroundWorkerInner>) {
        loop {
            // Wait for work
            let effect_id = {
                let mut pending = inner.pending.lock().unwrap();
                loop {
                    if let Some(id) = pending.pop() {
                        break id;
                    }
                    // Wait for notification
                    pending = inner.condvar.wait(pending).unwrap();
                }
            };

            // Process the effect (lock is released)
            effect_id.update_if_necessary(false);
        }
    }
}

// Keep the old function for backwards compatibility / convenience
/// Spawn the effect processing loop with default settings.
///
/// This is a convenience function equivalent to `EffectLoop::new().spawn()`.
///
/// For custom configuration, use the [`EffectLoop`] builder.
pub fn spawn_effect_loop() -> JoinHandle<()> {
    EffectLoop::new().spawn()
}
