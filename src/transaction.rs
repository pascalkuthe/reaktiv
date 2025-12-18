use crate::effect::flush_effects;
use std::cell::RefCell;
use std::sync::{Arc, OnceLock};
use tokio::sync::Semaphore;

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

/// RAII guard that ensures transaction cleanup happens even on panic.
///
/// When this guard is dropped (either normally or due to unwinding),
/// it decrements the transaction depth and potentially releases the permit.
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

    // When exiting outermost transaction:
    if should_flush {
        // 1. Release the barrier permit (unblocks async effect processor)
        HELD_PERMIT.with(|p| {
            *p.borrow_mut() = None; // Dropping the permit releases it
        });

        // 2. Flush all pending effects synchronously
        // This ensures effects batched during the transaction are processed.
        flush_effects();

        // 3. Notify the async effect loop (in case there are more effects)
        crate::effect::schedule_effect_processing();
    }
}

// ============================================================================
// Transaction Barrier (Semaphore)
// ============================================================================

/// Global semaphore that blocks effect processing during transactions.
///
/// The semaphore starts with 1 permit (processing allowed).
/// When a transaction starts (depth goes from 0 to 1), we acquire the permit.
/// When the outermost transaction ends (depth goes from 1 to 0), we release the permit.
///
/// The async effect loop in executor.rs waits to acquire this permit before
/// processing effects, ensuring effects don't run mid-transaction.
static TRANSACTION_BARRIER: OnceLock<Arc<Semaphore>> = OnceLock::new();

/// Get or initialize the transaction barrier.
pub fn get_transaction_barrier() -> &'static Arc<Semaphore> {
    TRANSACTION_BARRIER.get_or_init(|| Arc::new(Semaphore::new(1)))
}

// Owned permit for the transaction barrier.
//
// This is stored in a thread-local to allow releasing it when the
// outermost transaction exits.
thread_local! {
    static HELD_PERMIT: RefCell<Option<tokio::sync::OwnedSemaphorePermit>> = const { RefCell::new(None) };
}

/// Check if currently inside a transaction
///
/// When true, signal emissions should NOT immediately process effects.
/// Effects will be processed when the outermost transaction exits.
pub fn is_transaction_active() -> bool {
    TRANSACTION_DEPTH.with(|d| *d.borrow() > 0)
}

/// Increment transaction depth (entering a transaction)
///
/// When entering the first (outermost) transaction, we acquire the transaction
/// barrier permit. This blocks the async effect loop from processing effects
/// until the transaction completes.
fn enter_transaction() {
    TRANSACTION_DEPTH.with(|d| {
        let depth = *d.borrow();
        if depth == 0 {
            // First transaction level - acquire permit (blocks async processor)
            // We use try_acquire_owned because we're in a sync context.
            // If the permit is already held (can happen with concurrent tests),
            // we proceed anyway since we're already in a transaction context.
            let barrier = get_transaction_barrier();
            if let Ok(permit) = barrier.clone().try_acquire_owned() {
                HELD_PERMIT.with(|p| {
                    *p.borrow_mut() = Some(permit);
                });
            }
        }
        *d.borrow_mut() = depth + 1;
    });
}

/// Check if UI updates are currently suppressed (test-only)
#[cfg(test)]
fn is_ui_update_suppressed() -> bool {
    SUPPRESS_UI_UPDATES.with(|s| *s.borrow())
}

/// Batching context for multiple reactive updates
///
/// # Purpose:
/// - Batch multiple signal changes so dependent effects run once
/// - Optionally suppress UI refresh flags during transaction
/// - Prevent UI update loops
///
/// # Behavior:
/// 1. Enter transaction context
/// 2. Update multiple signals/fields
/// 3. Effects track dependencies but don't run yet
/// 4. On transaction exit, all pending effects run once
/// 5. `no_ui_updates()` variant suppresses UI refresh for internal changes
///
/// # Usage:
/// ```ignore
/// // Batch multiple updates
/// Transaction::run(|| {
///     circuit.voltage = 10.0;
///     circuit.frequency = 1e6;
///     circuit.load = 50.0;
/// });
/// // All effects depending on these fields run once at end
///
/// // Suppress UI updates for internal changes
/// Transaction::no_ui_updates(|| {
///     component.layout();
///     component.render();
/// });
/// // Effects won't try to refresh UI
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

        // Enter transaction (defers effect processing)
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
    fn transaction_acquires_barrier_permit() {
        // Verify that entering a transaction acquires the barrier permit.
        // Note: Since tests run in parallel and share the global semaphore,
        // we check relative state changes rather than absolute permit counts.
        let barrier = get_transaction_barrier();

        // Record permits before transaction (might be 0 if another test has a transaction active)
        let permits_before = barrier.available_permits();

        Transaction::run(|| {
            // Inside transaction: permit count should decrease (or stay at 0 if already 0)
            let permits_inside = barrier.available_permits();
            // If we acquired the permit (permits_before was 1), it should now be 0
            // If permits_before was 0, we didn't acquire (another test holds it)
            if permits_before > 0 {
                assert_eq!(permits_inside, permits_before - 1);
            }
        });

        // After transaction: permit should be released back to what it was before
        // (or higher if we released but didn't acquire)
        let permits_after = barrier.available_permits();
        // If we successfully acquired in this transaction, permits_after should equal permits_before
        // This assertion may fail if another test acquired between our exit and this check,
        // so we just verify it's not less than we started with
        assert!(
            permits_after >= permits_before || permits_before == 0,
            "Permits should be released after transaction: before={permits_before}, after={permits_after}"
        );
    }

    #[test]
    fn nested_transactions_hold_permit_until_outermost_exits() {
        // Verify that nested transactions only release the permit when the outermost exits.
        // This test checks behavior relative to the test's own transactions.
        let barrier = get_transaction_barrier();

        let permits_before = barrier.available_permits();

        Transaction::run(|| {
            // Permit should be acquired (or already held by another test)
            let permits_outer = barrier.available_permits();
            if permits_before > 0 {
                assert_eq!(permits_outer, permits_before - 1);
            }

            Transaction::run(|| {
                // Still held by outermost transaction (same count as outer)
                assert_eq!(barrier.available_permits(), permits_outer);
            });

            // Inner transaction exited, but permit still held by outer
            assert_eq!(barrier.available_permits(), permits_outer);
        });

        // Outermost transaction exited, permit should be released
        let permits_after = barrier.available_permits();
        assert!(
            permits_after >= permits_before || permits_before == 0,
            "Permits should be released: before={permits_before}, after={permits_after}"
        );
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
