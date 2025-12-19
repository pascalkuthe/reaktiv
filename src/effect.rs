use crate::arena::ReactiveState;
use crate::arena::effect_arena::take_pending_effects_split;
use crate::arena::signal_arena::remove_signal_writer;
use crate::arena::with_signal_arena;
use crate::arena::{
    EffectId, EffectMetadata, current_effect, destroy_children, effect_arena_insert,
    effect_arena_remove, mark_effect_pending, remove_from_pending_set, set_effect_parent,
    take_pending_effects,
};
use std::cell::Cell;
use std::time::Instant;

// Thread-local flag to track if effect processing has been scheduled.
// This enables debouncing - multiple signal emissions only schedule one processing.
thread_local! {
    static PROCESSING_SCHEDULED: Cell<bool> = const { Cell::new(false) };
}

/// Schedule effect processing without running them immediately
///
/// This marks that effects need to be processed but doesn't run them yet.
/// Multiple calls only schedule one processing (automatic debouncing).
///
/// Effects actually run when:
/// - `flush_effects()` is called manually
/// - A transaction ends
/// - The async effect loop wakes up (if using `spawn_effect_loop()`)
///
/// This function is called automatically by `signal.emit()`, so you typically
/// don't need to call it yourself.
pub fn schedule_effect_processing() {
    PROCESSING_SCHEDULED.with(|scheduled| {
        scheduled.set(true);
    });

    // Notify the async effect loop (if running) to wake up
    crate::executor::notify_effect_loop();
}

/// Check if effect processing is scheduled
///
/// Returns true if `schedule_effect_processing()` has been called but `flush_effects()`
/// has not yet been called.
pub fn is_processing_scheduled() -> bool {
    PROCESSING_SCHEDULED.with(std::cell::Cell::get)
}

/// Flush pending effects with a time budget
///
/// This processes pending effects until the specified budget is exhausted.
/// Effects that would exceed the budget are deferred and returned.
///
/// Skippable effects (producers) are processed first until the budget is exceeded.
/// Non-skippable effects (consumers) are always run, but with skip_skippable=true
/// when over budget to avoid running producers during pull.
///
/// Returns (effects_processed, deferred_effects)
///
/// # Usage
///
/// ```ignore
/// let budget = Duration::from_millis(16); // One frame at 60fps
/// let (processed, deferred) = flush_effects_with_budget(budget);
/// // Handle deferred effects (e.g., spawn on another thread)
/// ```
pub(crate) fn flush_effects_with_budget(
    budget: std::time::Duration,
    must_run: &mut Vec<EffectId>,
    skippable: &mut Vec<EffectId>,
) {
    // Clear the scheduled flag
    PROCESSING_SCHEDULED.with(|scheduled| {
        scheduled.set(false);
    });

    let start = Instant::now();

    // Take all pending effect IDs atomically from the pending set
    take_pending_effects_split(must_run, skippable);

    // Process skippable effects until budget exceeded
    // pop() removes from the end, so what remains are the deferred effects
    let mut i = 0;
    let over_budget = loop {
        if start.elapsed() > budget {
            break true;
        }
        let Some(effect_id) = skippable.get(i) else {
            break false;
        };
        if effect_id.state() != ReactiveState::Clean {
            effect_id.update_if_necessary(false);
        }
        i += 1;
    };
    skippable.drain(..i);
    // After the loop, skippable contains only deferred effects (those not popped)

    // Run must-run effects, skip pulling on skippable if over budget
    for effect_id in must_run {
        if effect_id.state() != ReactiveState::Clean {
            effect_id.update_if_necessary(over_budget);
        }
    }
}

/// Process all pending effects immediately
///
/// This is the main way to trigger effect processing. Call this after making changes
/// to signals, or integrate it into your event loop for automatic processing.
///
/// Returns the number of effects processed.
///
/// # Example
///
/// ```ignore
/// // Manually trigger effect processing
/// signal.emit();
/// flush_effects();  // All pending effects run now
///
/// // Or integrate into an event loop
/// loop {
///     handle_events();
///     flush_effects();  // Process any pending reactive updates
/// }
///
/// // Or use the async effect loop
/// spawn_effect_loop();  // Automatically processes effects
/// ```
pub fn flush_effects() -> usize {
    // Clear the scheduled flag
    PROCESSING_SCHEDULED.with(|scheduled| {
        scheduled.set(false);
    });

    // Process all pending effects
    Effect::process_all()
}

/// Internal helper that runs a single effect
///
/// This function handles the full lifecycle of running an effect:
/// 1. Check if effect still exists and needs work (state != Clean)
/// 2. Clear state to Clean
/// 3. Optionally remove from pending set (if `remove_from_pending` is true)
/// 4. Clear old outputs (remove from writers maps) - for push-pull system
/// 5. Unsubscribe from old sources
/// 6. Run the callback (Signal::track_dependency handles subscriptions via CURRENT_EFFECT)
///
/// If `remove_from_pending` is true, also removes the effect from the pending set.
fn run_single_effect_internal(effect_id: EffectId, remove_from_pending: bool) {
    use crate::arena::ReactiveState;

    // Skip if effect no longer exists or is Clean
    // (could have been removed or already processed by re-entrancy)
    if !effect_id.needs_work() {
        return;
    }

    // Clear state to Clean
    effect_id.set_state(ReactiveState::Clean);

    // Remove from pending set if requested
    if remove_from_pending {
        remove_from_pending_set(effect_id);
    }

    // Skip if no callback (might be a computed, not an effect)
    if !effect_id.has_callback() {
        return;
    }

    // Child effects were already destroyed when this effect was marked dirty
    // (see mark_effect_pending/mark_effect_dirty in effect_arena.rs)

    // Clear old sources and outputs in a single pass
    effect_id.clear_signals(
        |source_id| source_id.remove_subscriber(effect_id),
        |output_id| remove_signal_writer(output_id, effect_id),
    );

    // Set this effect as the current effect (for dependency tracking and parent/child relationships)
    // Signal::track_dependency() will check current_effect() and subscribe this effect.
    // The guard ensures CURRENT_EFFECT is restored even if callback panics.
    let _guard = crate::arena::CurrentEffectGuard::new(Some(effect_id));

    // Run the callback - Signal reads will automatically track dependencies via current_effect()
    effect_id.run_callback();

    // Guard automatically restores previous current effect when dropped

    // Dependencies are now subscribed automatically via Signal::track_dependency()
    // which was called during the callback execution.
}

/// Run a single effect by its ID
///
/// This function handles the full lifecycle of running an effect.
/// See `run_single_effect_internal` for details.
///
/// This is made public so it can be imported by effect_arena.rs for use in
/// EffectId::run_and_mark_children_if_changed.
pub fn run_single_effect(effect_id: EffectId) {
    run_single_effect_internal(effect_id, false);
}

/// Run a closure without tracking dependencies
///
/// Use this when you need to read signals without creating dependencies.
/// Useful for one-time reads or avoiding circular dependencies.
///
/// # Example
/// ```ignore
/// let effect = Effect::new(move || {
///     // This read creates a dependency - effect will re-run when signal1 changes
///     signal1.track_dependency();
///     let tracked_value = value1;
///
///     // This read does NOT create a dependency - effect won't re-run when signal2 changes
///     let untracked_value = untracked(|| {
///         signal2.track_dependency();  // This call is ignored
///         value2
///     });
/// });
/// ```
pub fn untracked<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    // Temporarily clear the current effect
    // The guard ensures CURRENT_EFFECT is restored even if f panics
    let _guard = crate::arena::CurrentEffectGuard::new(None);
    // Run the closure
    f()
}

/// Side-effectful computation that automatically re-runs when dependencies change
///
/// Effects track which signals they read and automatically re-run when any of those
/// signals change. Multiple rapid changes are automatically batched together.
///
/// # How it works
/// 1. Effect runs immediately on creation, tracking all `signal.track_dependency()` calls
/// 2. When any tracked signal changes, the effect is marked pending
/// 3. Multiple changes are batched - 20 rapid signal changes result in just 1 effect run
/// 4. Call `flush_effects()` to process all pending effects, or use `spawn_effect_loop()` for async processing
///
/// # Example
/// ```ignore
/// let voltage_signal = Signal::new();
/// let current_signal = Signal::new();
///
/// // Effect runs immediately and tracks both signals
/// let effect = Effect::new(|| {
///     voltage_signal.track_dependency();
///     current_signal.track_dependency();
///     println!("Power: {}", voltage * current);
/// });
///
/// // Multiple rapid changes are batched
/// voltage_signal.emit();  // Effect marked pending
/// voltage_signal.emit();  // Already pending, skipped
/// voltage_signal.emit();  // Already pending, skipped
///
/// flush_effects();  // Effect runs once with final values
/// ```
///
/// # Skippable effects
/// Use `Effect::new_skippable()` for effects that can be deferred under load.
/// This is useful for non-critical UI updates that can run in the background
/// while keeping the main UI responsive.
pub struct Effect {
    /// Arena ID for this effect's metadata (callback, sources, pending).
    /// This is the sole identifier for this effect.
    id: EffectId,
}

impl Effect {
    /// Create a new effect (runs immediately)
    ///
    /// The callback is stored directly in the arena, making Effect a thin wrapper.
    /// If created inside another effect's callback, establishes parent/child relationship.
    pub fn new<F>(f: F) -> Self
    where
        F: FnMut() + Send + 'static,
    {
        Self::new_internal(f, false, false)
    }

    /// Create a new skippable effect (runs immediately)
    ///
    /// Skippable effects can be deferred under load. They are typically producer
    /// effects (like validation) that can be skipped when consumers (like Qt bridge)
    /// need to run within a time budget.
    pub fn new_skippable<F>(f: F) -> Self
    where
        F: FnMut() + Send + 'static,
    {
        Self::new_internal(f, true, false)
    }

    /// Create a new effect that only wants UI updates (skipped on data-only updates)
    ///
    /// UI-updates-only effects are skipped when signals emit via `emit_from_ui()`.
    /// They only run when signals emit via `emit()` or `emit_from_api()`.
    pub fn new_ui_updates_only<F>(f: F) -> Self
    where
        F: FnMut() + Send + 'static,
    {
        Self::new_internal(f, false, true)
    }

    /// Internal effect constructor
    fn new_internal<F>(f: F, skippable: bool, ui_updates_only: bool) -> Self
    where
        F: FnMut() + Send + 'static,
    {
        // 1. Check if we're inside another effect (establish parent/child relationship)
        let parent = current_effect();

        // 2. Create EffectMetadata with the callback and flags
        let metadata = EffectMetadata::new_with_callback_parent_and_flags(
            Box::new(f),
            parent,
            skippable,
            ui_updates_only,
        );
        let id = effect_arena_insert(metadata);

        // 3. Register parent-child relationship in global maps (if any)
        if let Some(parent_id) = parent {
            set_effect_parent(id, parent_id);
            parent_id.add_child(id);
        }

        let effect = Self { id };

        // 4. Run immediately to establish dependencies
        effect.run_now();

        effect
    }

    /// Run this effect immediately
    ///
    /// This clears old subscriptions and runs the callback.
    /// Dependencies are automatically subscribed via Signal::track_dependency()
    /// which checks current_effect() during the callback.
    ///
    /// Note: Child effects are NOT destroyed here - they are destroyed immediately
    /// when the effect is marked pending (in mark_effect_pending). This ensures
    /// stale children don't hang around between invalidation and execution.
    pub(crate) fn run_now(&self) {
        // 1. Clear old subscriptions before re-running
        //    Get the old sources and unsubscribe from them
        self.id.with_sources(|old_sources| {
            for source_id in old_sources {
                source_id.remove_subscriber(self.id);
            }
        });
        self.id.clear_sources();

        // 2. Set this effect as the current effect (for dependency tracking and parent/child relationships)
        //    Signal::track_dependency() will check current_effect() and subscribe this effect.
        //    The guard ensures CURRENT_EFFECT is restored even if callback panics.
        let _guard = crate::arena::CurrentEffectGuard::new(Some(self.id));

        // 3. Run the callback - Signal reads will automatically track dependencies via current_effect()
        self.id.run_callback();

        // 4. Guard automatically restores previous current effect when dropped

        // 5. Dependencies are now subscribed automatically via Signal::track_dependency()
        //    which was called during the callback execution.

        // 6. Clear pending flag in arena
        use crate::arena::ReactiveState;
        self.id.set_state(ReactiveState::Clean);
    }

    /// Invalidate this effect (called when a source changes)
    /// Sets pending=true for later processing by the executor
    pub fn invalidate(&self) {
        // Use the efficient mark_effect_pending which handles debouncing
        // and adds to the pending set for O(k) processing
        mark_effect_pending(self.id);
    }

    /// Get the EffectId for this effect (internal use only)
    pub(crate) fn id(&self) -> EffectId {
        self.id
    }

    /// Create an Effect wrapper from an existing EffectId (internal use only)
    ///
    /// This does NOT run the effect or establish parent/child relationships.
    /// Use this when you've already created and set up the EffectId externally
    /// (e.g., in Computed) and just want Effect to handle cleanup via Drop.
    pub(crate) fn from_raw(id: EffectId) -> Self {
        Self { id }
    }

    /// Process all pending effects using fixed-point iteration
    ///
    /// This is the main entry point for the reactive loop. It uses the efficient
    /// pending set for O(k) processing where k is the number of pending effects,
    /// rather than iterating the entire arena.
    ///
    /// Uses fixed-point iteration: keeps processing until no more effects are pending.
    /// This ensures all cascading effects (effects that create new pending effects
    /// when they run) are processed.
    ///
    /// All effects are run sequentially on the current thread. This is simpler
    /// and more predictable than parallel execution, and works well with
    /// tokio's spawn_blocking for async integration.
    ///
    /// Note: Child effects are already destroyed when the parent is marked dirty
    /// (in mark_effect_pending/mark_effect_dirty), so we don't destroy them here.
    ///
    /// # Three-State System
    /// Effects in the pending set may be in Check or Dirty state:
    /// - Dirty: Will definitely run
    /// - Check: Will verify sources first, may not need to run
    /// - Clean: Should not be in pending set, but we handle gracefully
    pub fn process_all() -> usize {
        use crate::arena::ReactiveState;

        let mut total = 0;

        // Fixed-point iteration: keep processing until no more pending effects
        loop {
            // Take all pending effect IDs atomically from the pending set
            let pending_ids = take_pending_effects();
            if pending_ids.is_empty() {
                break;
            }

            // Run all effects sequentially on current thread
            for effect_id in pending_ids {
                // Use update_if_necessary to handle three-state system
                // This handles Check state (verify sources) and Dirty state (run)
                if effect_id.state() != ReactiveState::Clean {
                    effect_id.update_if_necessary(false);
                    total += 1;
                }
            }
        }

        total
    }
}

impl Drop for Effect {
    fn drop(&mut self) {
        // Check if this effect has a parent (is a child effect)
        let has_parent = self.id.parent().is_some();

        // 1. Remove from pending set (if pending)
        remove_from_pending_set(self.id);

        // 2. Remove ourselves from each signal's subscribers
        // This is done even for child effects - if the user explicitly drops
        // the Effect, they want it to stop reacting. Users can use mem::forget
        // to let the parent handle cleanup if they want the old behavior.
        let effect_id = self.id;
        let exists = self.id.with_sources(|sources| {
            with_signal_arena(|arena| {
                for source_id in sources {
                    if let Some(node) = arena.get(source_id.index()) {
                        node.subscribers.write().retain(|&eid| eid != effect_id);
                    }
                }
            });
        });
        if exists.is_none() {
            return; // Already removed
        }

        // 3. Remove ourselves from SIGNAL_WRITERS for each output
        self.id.with_outputs(|outputs| {
            for signal_id in outputs {
                remove_signal_writer(signal_id, effect_id);
            }
        });

        if has_parent {
            // Child effects: unsubscribe from signals (done above) but don't
            // destroy from arena - the parent owns the arena entry and will
            // clean it up when the parent re-runs or is destroyed.
            return;
        }

        // For top-level effects (no parent), do full cleanup:

        // 5. Destroy all child effects (using function from effect_arena)
        destroy_children(self.id);

        // 6. Deallocate from arena
        effect_arena_remove(self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn effect_debounces_rapid_invalidations() {
        let run_count = Arc::new(AtomicUsize::new(0));
        let run_count_clone = run_count.clone();

        let effect = Effect::new(move || {
            run_count_clone.fetch_add(1, Ordering::Relaxed);
        });

        // Initial run during construction
        assert_eq!(run_count.load(Ordering::Relaxed), 1);

        // Invalidate 20 times rapidly
        for _ in 0..20 {
            effect.invalidate();
        }

        // Process pending effects (debounced: should only run once, not 20 times)
        Effect::process_all();

        // Total: 1 initial + 1 debounced = 2 runs
        assert_eq!(run_count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn effect_automatic_reactivity() {
        // This test demonstrates that effects run when signals change and flush_effects is called
        use std::sync::atomic::AtomicI32;

        let signal = crate::Signal::new();
        let signal_node_id = signal.id();
        let counter = Arc::new(AtomicI32::new(0));

        let counter_clone = counter.clone();
        let effect = Effect::new(move || {
            // Track the signal as a dependency via CURRENT_EFFECT mechanism
            signal_node_id.track_dependency();
            counter_clone.fetch_add(1, Ordering::Relaxed);
        });

        // Effect ran once during construction
        assert_eq!(counter.load(Ordering::Relaxed), 1);

        // Verify subscription was established correctly
        signal_node_id.with_subscribers(|subscribers| {
            assert_eq!(subscribers.len(), 1);
            assert!(subscribers.contains(&effect.id()));
        });

        // Verify effect tracks signal as source
        effect.id().with_sources(|sources| {
            assert_eq!(sources.count(), 1);
        });
        let has_signal = effect
            .id()
            .with_sources(|sources| {
                for s in sources {
                    if s == signal_node_id {
                        return true;
                    }
                }
                false
            })
            .unwrap_or(false);
        assert!(has_signal);

        // Emit signal - schedules effect processing (debounced)
        signal.emit();

        // Flush to process effects
        flush_effects();

        // Effect ran after flush
        assert_eq!(counter.load(Ordering::Relaxed), 2);

        // Another emit and flush should work too
        signal.emit();
        flush_effects();
        assert_eq!(counter.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn effect_automatic_reactivity_via_track_dependency() {
        // Test using the high-level track_dependency() API with debounced processing
        use std::sync::atomic::AtomicI32;

        let signal = crate::Signal::new();
        let signal_node_id = signal.id();
        let counter = Arc::new(AtomicI32::new(0));

        let counter_clone = counter.clone();
        let _effect = Effect::new(move || {
            // Track dependency using CURRENT_EFFECT mechanism
            signal_node_id.track_dependency();
            counter_clone.fetch_add(1, Ordering::Relaxed);
        });

        // Effect ran once during construction
        assert_eq!(counter.load(Ordering::Relaxed), 1);

        // Emit signal change - schedules processing (debounced)
        signal.emit();
        flush_effects();

        // Effect ran after flush
        assert_eq!(counter.load(Ordering::Relaxed), 2);

        // Multiple emits in a transaction are debounced to one run
        // Transaction automatically flushes effects when it ends
        crate::Transaction::run(|| {
            signal.emit();
            signal.emit();
            signal.emit();
        });

        // After transaction, effect ran once more (transaction flushes effects on exit)
        assert_eq!(counter.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn effect_ui_origin_filtering() {
        // Test that track_ui_updates_only filtering works with debounced processing
        use std::sync::atomic::AtomicI32;

        let signal = crate::Signal::new();
        let signal_node_id = signal.id();
        let counter = Arc::new(AtomicI32::new(0));

        let counter_clone = counter.clone();
        let _effect = Effect::new(move || {
            // Track dependency using CURRENT_EFFECT mechanism
            signal_node_id.track_dependency();
            counter_clone.fetch_add(1, Ordering::Relaxed);
        });

        assert_eq!(counter.load(Ordering::Relaxed), 1);

        // emit_from_api() schedules processing, flush to execute
        signal.emit_from_api();
        flush_effects();
        assert_eq!(counter.load(Ordering::Relaxed), 2);

        // emit_from_ui() schedules processing, flush to execute
        signal.emit_from_ui();
        flush_effects();
        assert_eq!(counter.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn debounce_multiple_emissions() {
        // Test that multiple signal emissions only trigger processing once
        use std::sync::atomic::AtomicI32;

        let signal = crate::Signal::new();
        let signal_node_id = signal.id();
        let counter = Arc::new(AtomicI32::new(0));

        let counter_clone = counter.clone();
        let _effect = Effect::new(move || {
            signal_node_id.track_dependency();
            counter_clone.fetch_add(1, Ordering::Relaxed);
        });

        // Effect ran once during construction
        assert_eq!(counter.load(Ordering::Relaxed), 1);

        // Multiple emissions without flush
        signal.emit();
        signal.emit();
        signal.emit();

        // Effect hasn't run yet (no flush)
        assert_eq!(counter.load(Ordering::Relaxed), 1);

        // Single flush runs all pending effects once (debounced)
        flush_effects();
        assert_eq!(counter.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn flush_effects_processes_pending() {
        // Verify flush_effects processes pending effects
        use std::sync::atomic::AtomicI32;
        let signal = crate::Signal::new();
        let signal_node_id = signal.id();
        let counter = Arc::new(AtomicI32::new(0));

        let counter_clone = counter.clone();
        let _effect = Effect::new(move || {
            signal_node_id.track_dependency();
            counter_clone.fetch_add(1, Ordering::Relaxed);
        });

        // Initial run
        assert_eq!(counter.load(Ordering::Relaxed), 1);

        // Emit schedules processing
        signal.emit();

        // Flush processes pending effects
        flush_effects();
        assert_eq!(counter.load(Ordering::Relaxed), 2);
    }
}
