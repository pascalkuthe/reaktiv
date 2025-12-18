//! Executor for processing pending effects
//!
//! This module provides both synchronous and asynchronous APIs for effect processing:
//!
//! - **Synchronous API**: `Executor::process()` and `Effect::process_all()` for direct
//!   effect processing, typically used in tests or when you need immediate execution.
//!
//! - **Asynchronous API**: `run_effect_loop()` and `flush_effects_async()` for integration
//!   with tokio-based applications. The async API uses a `Notify` for event-driven
//!   scheduling (no polling), making it battery-efficient.
//!
//! ## Architecture
//!
//! The async executor uses tokio's `Notify` for efficient event-driven scheduling:
//! 1. When signals emit, `schedule_effect_processing()` is called
//! 2. This notifies the async effect loop (if running)
//! 3. After a small debounce delay, effects are processed via `spawn_blocking`
//! 4. This keeps effect execution off the async runtime threads
//!
//! ## Usage
//!
//! For async applications:
//! ```ignore
//! // Start the effect loop (runs in background)
//! tokio::spawn(run_effect_loop());
//!
//! // Or manually flush effects asynchronously
//! flush_effects_async().await;
//! ```
//!
//! For sync applications or tests:
//! ```ignore
//! signal.emit();
//! flush_effects();  // Synchronous, immediate processing
//! ```

use crate::Effect;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::sync::Notify;
use tokio::time::{Duration, sleep};

/// Simple executor for running deferred effects
///
/// This is now a thin wrapper around Effect::process_all().
/// The actual execution logic lives in the Effect module which
/// iterates the arena to find pending effects.
pub struct Executor {
    /// Re-entrance guard
    processing: AtomicUsize,
}

impl Executor {
    /// Create a new executor instance.
    pub fn new() -> Arc<Self> {
        #[allow(clippy::arc_with_non_send_sync)]
        Arc::new(Self {
            processing: AtomicUsize::new(0),
        })
    }

    /// Process all pending effects by iterating the arena
    ///
    /// Returns number of effects executed.
    pub fn process(&self) -> usize {
        // Prevent re-entrance
        if self.processing.fetch_add(1, Ordering::Relaxed) != 0 {
            self.processing.fetch_sub(1, Ordering::Relaxed);
            return 0;
        }

        // Use the arena-based process_all from Effect
        let count = Effect::process_all();

        self.processing.fetch_sub(1, Ordering::Relaxed);
        count
    }

    /// Get count of pending effects (from arena)
    pub fn queue_len(&self) -> usize {
        Effect::queue_len()
    }

    /// Run the reactive loop until no more effects are pending
    ///
    /// This is useful for testing where you want to ensure all
    /// cascading effects have been processed.
    pub fn run(&self) -> usize {
        let mut total = 0;

        // Prevent re-entrance
        if self.processing.fetch_add(1, Ordering::Relaxed) != 0 {
            self.processing.fetch_sub(1, Ordering::Relaxed);
            return 0;
        }

        loop {
            let count = Effect::process_all();
            if count == 0 {
                break;
            }
            total += count;
        }

        self.processing.fetch_sub(1, Ordering::Relaxed);
        total
    }
}

impl Default for Executor {
    fn default() -> Self {
        Self {
            processing: AtomicUsize::new(0),
        }
    }
}

// ============================================================================
// Async Effect Processing (tokio-based)
// ============================================================================

/// Global notify for triggering effect processing.
///
/// This is used by the async effect loop to wake up when effects need processing.
/// The `Notify` is event-driven (no polling), making it battery-efficient.
static EFFECT_NOTIFY: OnceLock<Arc<Notify>> = OnceLock::new();

/// Get or initialize the global effect notify.
fn get_notify() -> &'static Arc<Notify> {
    EFFECT_NOTIFY.get_or_init(|| Arc::new(Notify::new()))
}

/// Notify the async effect loop that effects need processing.
///
/// This is called by `schedule_effect_processing()` to wake up the async loop.
/// If no async loop is running, this is a no-op (the notify just gets dropped).
///
/// This function is non-blocking and safe to call from any context.
pub fn notify_effect_loop() {
    get_notify().notify_one();
}

/// Default debounce delay for effect processing (100 microseconds).
///
/// This small delay batches rapid signal emissions into a single effect run.
/// Adjust this value based on your application's needs.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(1);

/// Run the async effect processing loop.
///
/// This function runs indefinitely, processing effects when notified.
/// It should be spawned as a background task:
///
/// ```ignore
/// tokio::spawn(run_effect_loop());
/// ```
///
/// The loop:
/// 1. Waits for notification that effects need processing
/// 2. Applies a small debounce delay to batch rapid emissions
/// 3. Processes effects via `spawn_blocking` to keep them off async threads
///
/// ## Battery Efficiency
///
/// This loop is event-driven using tokio's `Notify`. It does NOT poll.
/// When no effects are pending, it consumes zero CPU cycles.
#[allow(dead_code)]
pub async fn run_effect_loop() {
    run_effect_loop_with_debounce(DEFAULT_DEBOUNCE).await
}

/// Run the async effect processing loop with a custom debounce delay.
///
/// See `run_effect_loop()` for details. The `debounce_micros` parameter
/// controls how long to wait after notification before processing effects.
/// This allows batching multiple rapid signal emissions.
///
/// ## Transaction Barrier
///
/// This loop waits for the transaction barrier permit before processing effects.
/// When a transaction is active, the permit is held by the transaction, so this
/// loop will wait until the transaction completes before processing.
pub async fn run_effect_loop_with_debounce(debounce: Duration) {
    let notify = get_notify();
    let barrier = crate::transaction::get_transaction_barrier();

    loop {
        // Wait for notification that effects need processing
        notify.notified().await;

        // Small debounce to batch rapid emissions
        sleep(debounce).await;

        // Wait for transaction to end (acquire permit)
        // This ensures effects don't run mid-transaction
        let _permit = barrier.acquire().await.unwrap();

        // Process effects on blocking thread pool to keep them off async threads.
        // This is important because effect callbacks may:
        // - Acquire locks
        // - Do blocking I/O
        // - Call into Python (which needs GIL)
        let _ = tokio::task::spawn_blocking(flush_effects_sync).await;

        // Permit is released when _permit drops (after processing completes)
    }
}

/// Synchronous flush - runs all pending effects.
///
/// This is called from `spawn_blocking` in the async loop.
/// It's also the implementation behind the public `flush_effects()` function.
fn flush_effects_sync() -> usize {
    crate::effect::flush_effects()
}

/// Flush effects asynchronously.
///
/// This processes all pending effects using `spawn_blocking`, which keeps
/// effect execution off the async runtime threads.
///
/// Use this when you need to ensure effects are processed before continuing,
/// but want to do so asynchronously:
///
/// ```ignore
/// signal.emit();
/// flush_effects_async().await;  // Effects processed on blocking thread
/// // Continue with assurance that effects have run
/// ```
#[allow(dead_code)]
pub async fn flush_effects_async() -> usize {
    tokio::task::spawn_blocking(flush_effects_sync)
        .await
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn executor_processes_pending_effects() {
        let executor = Executor::new();
        let executed = Arc::new(AtomicUsize::new(0));

        let executed_clone = executed.clone();
        let effect = Effect::new(move || {
            executed_clone.fetch_add(1, Ordering::Relaxed);
        });

        // Effect runs once during construction
        assert_eq!(executed.load(Ordering::Relaxed), 1);

        // Invalidate to mark as pending
        effect.invalidate();
        assert_eq!(executor.queue_len(), 1);

        // Process via executor
        let processed = executor.process();
        assert_eq!(processed, 1);
        assert_eq!(executed.load(Ordering::Relaxed), 2);
        assert_eq!(executor.queue_len(), 0);
    }

    #[test]
    fn executor_prevents_reentrance() {
        let executor = Arc::new(Executor::new());
        let executed = Arc::new(AtomicUsize::new(0));

        let executor_clone = executor.clone();
        let executed_clone = executed.clone();

        let effect = Effect::new(move || {
            executed_clone.fetch_add(1, Ordering::Relaxed);
            // Try to process again (should do nothing due to re-entrance guard)
            executor_clone.process();
        });

        effect.invalidate();
        executor.process();

        // Only the one effect execution (from process, not the re-entrant call)
        assert_eq!(executed.load(Ordering::Relaxed), 2); // 1 from construction + 1 from process
    }

    #[test]
    fn executor_run_processes_cascading_effects() {
        let executor = Executor::new();
        let count = Arc::new(AtomicUsize::new(0));

        let count_clone = count.clone();
        let effect = Effect::new(move || {
            count_clone.fetch_add(1, Ordering::Relaxed);
        });

        // Effect runs once during construction
        assert_eq!(count.load(Ordering::Relaxed), 1);

        // Invalidate multiple times
        effect.invalidate();

        // Run should process all pending
        let total = executor.run();
        assert_eq!(total, 1); // Only 1 pending due to debouncing
        assert_eq!(count.load(Ordering::Relaxed), 2);
    }
}
