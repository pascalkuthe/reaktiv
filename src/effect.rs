use crate::arena::signal_arena::remove_signal_writer;
use crate::arena::with_signal_arena;
use crate::arena::{
    EffectId, EffectMetadata, current_effect, destroy_children, effect_arena_insert,
    effect_arena_remove, mark_effect_pending, pending_effects_count, remove_from_pending_set,
    take_pending_effects,
};
use std::cell::Cell;

// Thread-local flag to track if effect processing has been scheduled.
// This enables debouncing - multiple signal emissions only schedule one processing.
thread_local! {
    static PROCESSING_SCHEDULED: Cell<bool> = const { Cell::new(false) };
}

/// Schedule effect processing (non-blocking debounce)
///
/// This marks that effects need to be processed, but doesn't process them immediately.
/// Multiple calls to this function only schedule one processing.
/// The actual processing happens when `flush_effects()` is called (typically by an event loop)
/// or when a transaction ends.
///
/// This also notifies the async effect loop (if running) to wake up and process effects.
/// If no async loop is running, the notification is a no-op.
///
/// This enables efficient batching of multiple signal changes into a single effect run.
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

/// Flush all pending effects (explicit processing trigger)
///
/// This is the main entry point for the application's event loop to process effects.
/// It clears the scheduled flag and processes all pending effects.
///
/// Returns the number of effects processed.
///
/// # Usage
///
/// Call this from your event loop or at points where you want effects to be processed:
///
/// ```ignore
/// // In an event loop
/// loop {
///     handle_events();
///     flush_effects();  // Process any pending reactive updates
/// }
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

    // Clear old outputs (for push-pull system)
    // Take the outputs and remove this effect from each signal's writers map
    if let Some(old_outputs) = effect_id.take_outputs() {
        for signal_id in old_outputs {
            remove_signal_writer(signal_id, effect_id);
        }
    }

    // Clear old subscriptions
    if let Some(old_sources) = effect_id.sources() {
        for source_id in &old_sources {
            source_id.remove_subscriber(effect_id);
        }
    }
    effect_id.clear_sources();

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
/// Use this when you need to read a signal's value without creating a dependency.
/// This is useful for:
/// - Reading initial values without subscribing
/// - Implementing one-time reads
/// - Avoiding circular dependencies
///
/// # Example
/// ```ignore
/// let effect = Effect::new(move || {
///     // This read creates a dependency
///     let tracked_value = signal1.get();
///
///     // This read does NOT create a dependency
///     let untracked_value = untracked(|| signal2.get());
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

/// Run a single effect for pull-based verification
///
/// This is called from SignalId::pull() when a stale writer effect needs to run.
/// It's similar to run_single_effect but also removes the effect from the pending set.
///
/// This function handles the full lifecycle of running an effect.
/// See `run_single_effect_internal` for details.
#[allow(dead_code)]
pub fn run_single_effect_for_pull(effect_id: EffectId) {
    run_single_effect_internal(effect_id, true);
}

/// Reactive computation with side effects and automatic dependency tracking
///
/// # Architecture
/// Effect is a thin owning wrapper around EffectId. All effect state lives in
/// the arena's EffectMetadata:
/// - callback: the effect function
/// - sources: signals this effect depends on
/// - pending: whether the effect needs to run
///
/// This eliminates the need for external registries like EFFECT_REGISTRY.
///
/// # How effects work:
/// 1. Create with a function that reads reactive signals
/// 2. Effect runs immediately, tracking all signal reads as dependencies
/// 3. When any source signal changes, effect is marked pending
/// 4. Pending effects are processed by the executor
/// 5. Duplicate invalidations are deduplicated via the pending flag
///
/// # Debouncing:
/// - `pending` flag in arena tracks if effect is already scheduled
/// - First invalidation: set pending=true
/// - Subsequent invalidations: skip (already pending)
/// - Effect runs: clear pending
/// - Result: 20 rapid changes -> 1 effect run, not 20
///
/// # Example:
/// ```ignore
/// let power = Effect::new(|| {
///     println!("Power: {}", circuit.voltage * circuit.current);
/// });
///
/// // Effect runs immediately during construction
/// // Tracks dependencies on circuit.voltage and circuit.current
///
/// circuit.voltage = 10.0;  // Effect marked pending
/// circuit.voltage = 11.0;  // Skipped (already pending)
/// circuit.voltage = 12.0;  // Skipped (already pending)
///
/// Effect::process_all();  // Effect runs once with final voltage=12.0
/// ```
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
    /// This effect can run in parallel with other effects.
    pub fn new<F>(f: F) -> Self
    where
        F: FnMut() + Send + 'static,
    {
        Self::new_internal(f, false)
    }

    /// Create a new local effect (runs immediately, always on main thread)
    ///
    /// Local effects are used for Python callbacks that require the GIL.
    /// They run sequentially on the current thread instead of in parallel.
    /// If created inside another effect's callback, establishes parent/child relationship.
    pub fn new_local<F>(f: F) -> Self
    where
        F: FnMut() + Send + 'static,
    {
        Self::new_internal(f, true)
    }

    /// Internal constructor for effects with configurable local flag
    fn new_internal<F>(f: F, local: bool) -> Self
    where
        F: FnMut() + Send + 'static,
    {
        // 1. Check if we're inside another effect (establish parent/child relationship)
        let parent = current_effect();

        // 2. Create EffectMetadata with the callback, parent, and local flag
        let metadata =
            EffectMetadata::new_with_callback_parent_and_local(Box::new(f), parent, local);
        let id = effect_arena_insert(metadata);

        // 3. Register as child of parent (if any)
        if let Some(parent_id) = parent {
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
        if let Some(old_sources) = self.id.sources() {
            for source_id in &old_sources {
                source_id.remove_subscriber(self.id);
            }
        }
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
    #[allow(dead_code)]
    pub(crate) fn id(&self) -> EffectId {
        self.id
    }

    /// Get current sources (internal use only)
    #[allow(dead_code)]
    pub(crate) fn sources(&self) -> Vec<crate::arena::SignalId> {
        self.id.sources().unwrap_or_default()
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
                    effect_id.update_if_necessary();
                    total += 1;
                }
            }
        }

        total
    }

    /// Get count of pending effects
    ///
    /// This is O(1) using the efficient pending set.
    pub fn queue_len() -> usize {
        pending_effects_count()
    }
}

impl Drop for Effect {
    fn drop(&mut self) {
        // Check if this effect has a parent (is a child effect)
        let has_parent = self.id.parent().is_some();

        if has_parent {
            // Child effects created inside a parent's callback should NOT be cleaned up
            // when their Effect is dropped. The parent owns them and will clean them
            // up when it re-runs (via destroy_children) or when the parent is dropped.
            //
            // This allows child effects to remain active even after the local variable
            // goes out of scope in the parent's callback.
            return;
        }

        // For top-level effects (no parent), do full cleanup:

        // 1. Remove from pending set (if pending)
        remove_from_pending_set(self.id);

        // 2. Destroy all child effects (using function from effect_arena)
        destroy_children(self.id);

        // 3. Get all sources and outputs from arena
        let (sources, outputs) = if let Some(result) = self
            .id
            .with(|metadata| (metadata.get_sources(), metadata.get_outputs()))
        {
            result
        } else {
            return; // Already removed
        };

        // 4. Remove ourselves from each signal's subscribers
        with_signal_arena(|arena| {
            for source_id in sources {
                if let Some(node) = arena.get(source_id.index()) {
                    node.subscribers
                        .write()
                        .retain(|sub| sub.effect_id != self.id);
                }
            }
        });

        // 5. Remove ourselves from SIGNAL_WRITERS for each output
        for signal_id in outputs {
            remove_signal_writer(signal_id, self.id);
        }

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
        let signal_node_id = signal.node_id();
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
        let subscribers = signal_node_id.subscribers().unwrap();
        assert_eq!(subscribers.len(), 1);
        assert_eq!(subscribers[0].effect_id, effect.id());

        // Verify effect tracks signal as source
        let sources = effect.id().sources().unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0], signal_node_id);

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
        let signal_node_id = signal.node_id();
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
        let signal_node_id = signal.node_id();
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
    fn local_effect_runs_sequentially() {
        // Verify local effects are marked as local
        let effect = Effect::new_local(|| {});
        assert!(effect.id().is_local());

        // Regular effects are not local
        let regular_effect = Effect::new(|| {});
        assert!(!regular_effect.id().is_local());
    }

    #[test]
    fn local_effect_executes_correctly() {
        use std::sync::atomic::AtomicI32;

        let counter = Arc::new(AtomicI32::new(0));
        let counter_clone = counter.clone();

        // Create a local effect
        let _effect = Effect::new_local(move || {
            counter_clone.fetch_add(1, Ordering::Relaxed);
        });

        // Effect runs immediately during construction
        assert_eq!(counter.load(Ordering::Relaxed), 1);

        // Create a signal and subscribe the effect
        let signal = crate::Signal::new();
        let signal_node_id = signal.node_id();
        let counter_clone2 = counter.clone();

        let _effect2 = Effect::new_local(move || {
            signal_node_id.track_dependency();
            counter_clone2.fetch_add(1, Ordering::Relaxed);
        });

        // Effect ran during construction
        assert_eq!(counter.load(Ordering::Relaxed), 2);

        // Emit and flush
        signal.emit();
        flush_effects();

        // Effect ran again after flush
        assert_eq!(counter.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn debounce_multiple_emissions() {
        // Test that multiple signal emissions only trigger processing once
        use std::sync::atomic::AtomicI32;

        let signal = crate::Signal::new();
        let signal_node_id = signal.node_id();
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
        let signal_node_id = signal.node_id();
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

    #[test]
    fn parallel_and_local_effects_both_process() {
        // Test that both parallel and local effects are processed
        use std::sync::atomic::AtomicI32;

        let signal = crate::Signal::new();
        let signal_node_id = signal.node_id();

        let parallel_counter = Arc::new(AtomicI32::new(0));
        let local_counter = Arc::new(AtomicI32::new(0));

        let parallel_clone = parallel_counter.clone();
        let _parallel_effect = Effect::new(move || {
            signal_node_id.track_dependency();
            parallel_clone.fetch_add(1, Ordering::Relaxed);
        });

        let local_clone = local_counter.clone();
        let _local_effect = Effect::new_local(move || {
            signal_node_id.track_dependency();
            local_clone.fetch_add(1, Ordering::Relaxed);
        });

        // Both ran during construction
        assert_eq!(parallel_counter.load(Ordering::Relaxed), 1);
        assert_eq!(local_counter.load(Ordering::Relaxed), 1);

        // Emit and flush
        signal.emit();
        flush_effects();

        // Both ran after flush
        assert_eq!(parallel_counter.load(Ordering::Relaxed), 2);
        assert_eq!(local_counter.load(Ordering::Relaxed), 2);
    }
}
