use crate::effect::flush_effects;
use std::cell::RefCell;
use std::sync::{Condvar, Mutex, OnceLock};

// Thread-local flag to suppress UI updates
// When true, all signals get update_ui=false
thread_local! {
    static SUPPRESS_UI_UPDATES: RefCell<bool> = const { RefCell::new(false) };
}

// Thread-local transaction depth counter
// When > 0, signal emissions don't immediately process effects
thread_local! {
    static TRANSACTION_DEPTH: RefCell<usize> = const { RefCell::new(0) };
}

/// Global barrier that blocks the effect loop during transactions.
///
/// The barrier consists of:
/// - A counter of active transactions (across all threads)
/// - A condvar to wake the effect loop when transactions complete
///
/// When any transaction is active, the effect loop waits on the condvar.
/// When all transactions complete, the condvar is notified.
static TRANSACTION_BARRIER: OnceLock<TransactionBarrier> = OnceLock::new();

struct TransactionBarrier {
    /// Number of active transactions (0 = effect loop can proceed)
    active_count: Mutex<usize>,
    /// Condvar to wake the effect loop when transactions complete
    condvar: Condvar,
}

impl TransactionBarrier {
    fn new() -> Self {
        Self {
            active_count: Mutex::new(0),
            condvar: Condvar::new(),
        }
    }

    /// Increment the active transaction count (blocks effect loop)
    fn enter(&self) {
        let mut count = self.active_count.lock().unwrap();
        *count += 1;
    }

    /// Decrement the active transaction count, notify effect loop if zero
    fn exit(&self) {
        let mut count = self.active_count.lock().unwrap();
        *count = count.saturating_sub(1);
        if *count == 0 {
            self.condvar.notify_all();
        }
    }

    /// Wait until no transactions are active
    fn wait_until_clear(&self) {
        let mut count = self.active_count.lock().unwrap();
        while *count > 0 {
            count = self.condvar.wait(count).unwrap();
        }
    }
}

fn get_barrier() -> &'static TransactionBarrier {
    TRANSACTION_BARRIER.get_or_init(TransactionBarrier::new)
}

/// Wait until no transactions are active.
///
/// This is called by the effect loop before processing effects to ensure
/// effects don't run mid-transaction.
pub fn wait_for_transactions() {
    get_barrier().wait_until_clear();
}

/// RAII guard that ensures transaction cleanup happens even on panic.
///
/// When this guard is dropped (either normally or due to unwinding),
/// it decrements the transaction depth and potentially flushes effects.
struct TransactionGuard {
    prev_suppress: bool,
}

impl Drop for TransactionGuard {
    fn drop(&mut self) {
        // Restore previous UI suppression state
        SUPPRESS_UI_UPDATES.with(|s| {
            *s.borrow_mut() = self.prev_suppress;
        });

        // Exit transaction (always runs, even on panic)
        exit_transaction_inner();
    }
}

/// Internal exit transaction logic extracted for use by both normal exit and Drop.
fn exit_transaction_inner() {
    let should_flush = TRANSACTION_DEPTH.with(|d| {
        let mut depth = d.borrow_mut();
        *depth = depth.saturating_sub(1);
        *depth == 0
    });

    // When exiting outermost transaction on this thread:
    if should_flush {
        // 1. Release the global barrier (unblocks effect loop)
        get_barrier().exit();

        // 2. Flush all pending effects synchronously
        // This ensures effects batched during the transaction are processed.
        flush_effects();

        // 3. Notify the effect loop (in case there are more effects)
        crate::effect::schedule_effect_processing();
    }
}

/// Check if currently inside a transaction
///
/// When true, signal emissions should NOT immediately process effects.
/// Effects will be processed when the outermost transaction exits.
pub fn is_transaction_active() -> bool {
    TRANSACTION_DEPTH.with(|d| *d.borrow() > 0)
}

/// Increment transaction depth (entering a transaction)
fn enter_transaction() {
    TRANSACTION_DEPTH.with(|d| {
        let depth = *d.borrow();
        if depth == 0 {
            // First transaction on this thread - acquire global barrier
            get_barrier().enter();
        }
        *d.borrow_mut() = depth + 1;
    });
}

/// Check if UI updates are currently suppressed (test-only)
#[cfg(test)]
fn is_ui_update_suppressed() -> bool {
    SUPPRESS_UI_UPDATES.with(|s| *s.borrow())
}

/// Batch multiple signal changes into a single effect run
///
/// Transactions let you update multiple signals while ensuring dependent effects
/// only run once at the end. This is more efficient than running effects after
/// each individual change.
///
/// # How it works
/// 1. Enter transaction context
/// 2. Update multiple signals - they're marked pending but effects don't run yet
/// 3. On transaction exit, all pending effects run once
///
/// # Example
/// ```ignore
/// // Without transaction: effect runs 3 times
/// voltage_signal.emit();   // Effect runs
/// current_signal.emit();   // Effect runs
/// load_signal.emit();      // Effect runs
///
/// // With transaction: effect runs once
/// Transaction::run(|| {
///     voltage_signal.emit();
///     current_signal.emit();
///     load_signal.emit();
/// });  // All effects run once at end
/// ```
///
/// # Suppressing UI updates
/// Use `no_ui_updates()` to prevent UI-triggered effects from running during
/// internal updates. This is useful for preventing update loops:
/// ```ignore
/// Transaction::no_ui_updates(|| {
///     component.internal_recalculation();
/// });
/// ```
pub struct Transaction {
    _suppress_ui: bool,
}

impl Transaction {
    /// Run a function within a transaction context
    ///
    /// Effects are batched and debounced. UI updates are NOT suppressed.
    pub fn run<F, R>(f: F) -> R
    where
        F: FnOnce() -> R,
    {
        Self::run_with_suppress(f, false)
    }

    /// Run a function with UI updates suppressed
    ///
    /// All signals within this context get update_ui=false
    pub fn no_ui_updates<F, R>(f: F) -> R
    where
        F: FnOnce() -> R,
    {
        Self::run_with_suppress(f, true)
    }

    fn run_with_suppress<F, R>(f: F, suppress_ui: bool) -> R
    where
        F: FnOnce() -> R,
    {
        // Save previous UI suppression state and compute new state
        // Use OR logic: if currently suppressed or request to suppress, stay suppressed
        let (prev_suppress, should_suppress) = SUPPRESS_UI_UPDATES.with(|s| {
            let old = *s.borrow();
            let new = old || suppress_ui; // Sticky: outer suppression overrides
            (old, new)
        });

        // Set the UI suppression state
        SUPPRESS_UI_UPDATES.with(|s| {
            *s.borrow_mut() = should_suppress;
        });

        // Enter transaction (defers effect processing, acquires barrier)
        enter_transaction();

        // Create RAII guard that will clean up on both normal return and panic.
        // The guard stores prev_suppress and will restore it + exit transaction on drop.
        let _guard = TransactionGuard { prev_suppress };

        // Run the function
        // If f() panics, _guard will be dropped during unwinding,
        // which will restore UI suppression state and exit the transaction.
        f()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transaction_suppresses_ui_updates() {
        assert!(!is_ui_update_suppressed());

        Transaction::no_ui_updates(|| {
            assert!(is_ui_update_suppressed());
        });

        assert!(!is_ui_update_suppressed());
    }

    #[test]
    fn transaction_nesting_preserves_state() {
        assert!(!is_ui_update_suppressed());

        Transaction::no_ui_updates(|| {
            assert!(is_ui_update_suppressed());

            Transaction::run(|| {
                // Entered with suppress=true, stayed true
                assert!(is_ui_update_suppressed());
            });

            assert!(is_ui_update_suppressed());
        });

        assert!(!is_ui_update_suppressed());
    }

    #[test]
    fn transaction_run_allows_ui_updates() {
        assert!(!is_ui_update_suppressed());

        Transaction::run(|| {
            assert!(!is_ui_update_suppressed());
        });

        assert!(!is_ui_update_suppressed());
    }

    #[test]
    fn transaction_returns_value() {
        let result = Transaction::run(|| 42);

        assert_eq!(result, 42);
    }

    #[test]
    fn transaction_no_ui_updates_returns_value() {
        let result = Transaction::no_ui_updates(|| "hello");

        assert_eq!(result, "hello");
    }
}
