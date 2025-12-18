use crate::arena::{
    EffectId, EffectMetadata, SignalId, SignalMetadata, current_effect, effect_arena_insert,
    effect_arena_remove, remove_from_pending_set, signal_arena_insert, signal_arena_remove,
    with_signal_arena,
};
use parking_lot::{Mutex, RwLock};
use std::sync::Arc;

/// Memoized derived value that automatically updates when sources change
///
/// # PRINCIPLED MODEL - Effect + Signal Composition:
/// Computed = Signal (for output) + Effect (for subscription)
/// - **signal**: For output - other computeds/effects subscribe to this
/// - **effect_id**: Internal Effect ID that handles all subscription logic
///
/// This composition model leverages Effect's existing subscription machinery:
/// - Effect handles tracking dependencies via Observer
/// - Effect handles subscribing to signals
/// - Effect handles re-running when dependencies change
/// - Computed just wraps this with a cached value and a Signal for output
///
/// # How it works:
/// 1. Creation: An internal Effect is created that computes and caches the value
/// 2. The Effect runs immediately, computing the initial value
/// 3. When dependencies change, the Effect is re-run automatically
/// 4. If the new value differs from the old (requires T: PartialEq), the Signal is notified
/// 5. Downstream subscribers are only notified when the value actually changes
///
/// # Memory:
/// Owns the computed value (`RwLock<Option<T>>`) but not the input data.
///
/// # Example:
/// ```ignore
/// let power = Computed::new(|| {
///     circuit.voltage * circuit.current
/// });
///
/// let p1 = power.get();  // Returns cached value (computed on creation)
/// let p2 = power.get();  // Returns cached (no recomputation)
/// circuit.voltage = 20.0;
/// // When effects are flushed, Computed recomputes automatically
/// let p3 = power.get();  // Returns new cached value
/// ```
pub struct Computed<T> {
    /// Signal for output - others subscribe to this to get notified when this computed changes
    signal_id: SignalId,

    /// EffectId for the internal effect that handles subscription logic
    effect_id: EffectId,

    /// Cached computed value
    value: Arc<RwLock<Option<T>>>,
}

impl<T: Clone + PartialEq + Send + Sync + 'static> Computed<T> {
    /// Create a new computed value
    ///
    /// The computation runs immediately and the result is cached.
    /// Dependencies are tracked automatically via CURRENT_EFFECT mechanism.
    pub fn new<F>(f: F) -> Arc<Self>
    where
        F: FnMut() -> T + Send + 'static,
    {
        // Allocate signal metadata for output (version + subscribers)
        let signal_metadata = SignalMetadata::new();
        let signal_id = signal_arena_insert(signal_metadata);

        // Create shared state for the value and function
        let value: Arc<RwLock<Option<T>>> = Arc::new(RwLock::new(None));

        // Wrap function in Arc<Mutex> for sharing
        let f: Arc<Mutex<Box<dyn FnMut() -> T + Send>>> =
            Arc::new(Mutex::new(Box::new(f) as Box<dyn FnMut() -> T + Send>));

        // Create effect metadata with callback that recomputes and updates value
        let value_for_effect = value.clone();
        let f_for_effect = f.clone();

        // Check if we're inside another effect (establish parent/child relationship)
        let parent = current_effect();

        let callback: Box<dyn FnMut() + Send> = Box::new(move || {
            // Get old value for comparison
            let old_value = value_for_effect.read().clone();

            // Compute new value - Signal::track_dependency() will automatically
            // subscribe the current effect via CURRENT_EFFECT mechanism
            let new_result = {
                let mut f_guard = f_for_effect.lock();
                (*f_guard)()
            };

            // Check if value actually changed
            let changed = match &old_value {
                Some(old) => old != &new_result,
                None => true, // First computation always "changes"
            };

            // Update cached value
            *value_for_effect.write() = Some(new_result);

            // Only increment signal version if value actually changed
            if changed {
                signal_id.increment_version();
            }
        });

        let metadata = EffectMetadata::new_with_callback_and_parent(callback, parent);
        let effect_id = effect_arena_insert(metadata);

        // Register as child of parent (if any)
        if let Some(parent_id) = parent {
            parent_id.add_child(effect_id);
        }

        // Run initial computation with CURRENT_EFFECT set to track dependencies
        // Signal::track_dependency() will check current_effect() and subscribe automatically
        // The guard ensures CURRENT_EFFECT is restored even if callback panics
        let _guard = crate::arena::CurrentEffectGuard::new(Some(effect_id));
        effect_id.run_callback();
        // Guard automatically restores previous current effect when dropped

        Arc::new(Self {
            signal_id,
            effect_id,
            value,
        })
    }

    /// Create a lazy computed value that defers computation until first access
    ///
    /// Unlike `Computed::new()` which computes immediately, this only computes
    /// when `get()` is first called. Useful for expensive computations that may
    /// never be needed.
    ///
    /// # Example:
    /// ```ignore
    /// let expensive_calc = Computed::lazy(|| {
    ///     // This won't run until the first get() call
    ///     heavy_computation()
    /// });
    ///
    /// // Computation hasn't run yet
    /// let result = expensive_calc.get();  // Runs now
    /// let result2 = expensive_calc.get(); // Returns cached value
    /// ```
    pub fn lazy<F>(f: F) -> Arc<Self>
    where
        F: FnMut() -> T + Send + 'static,
    {
        // Allocate signal metadata for output (version + subscribers)
        let signal_metadata = SignalMetadata::new();
        let signal_id = signal_arena_insert(signal_metadata);

        // Create shared state for the value and function
        // Start with None - will be computed on first access
        let value: Arc<RwLock<Option<T>>> = Arc::new(RwLock::new(None));

        // Wrap function in Arc<Mutex> for sharing
        let f: Arc<Mutex<Box<dyn FnMut() -> T + Send>>> =
            Arc::new(Mutex::new(Box::new(f) as Box<dyn FnMut() -> T + Send>));

        // Create effect metadata with callback that recomputes and updates value
        let value_for_effect = value.clone();
        let f_for_effect = f.clone();

        // Check if we're inside another effect (establish parent/child relationship)
        let parent = current_effect();

        let callback: Box<dyn FnMut() + Send> = Box::new(move || {
            // Get old value for comparison
            let old_value = value_for_effect.read().clone();

            // Compute new value - Signal::track_dependency() will automatically
            // subscribe the current effect via CURRENT_EFFECT mechanism
            let new_result = {
                let mut f_guard = f_for_effect.lock();
                (*f_guard)()
            };

            // Check if value actually changed
            let changed = match &old_value {
                Some(old) => old != &new_result,
                None => true, // First computation always "changes"
            };

            // Update cached value
            *value_for_effect.write() = Some(new_result);

            // Only increment signal version if value actually changed
            if changed {
                signal_id.increment_version();
            }
        });

        // Create metadata but mark as Dirty so first access triggers computation
        let metadata = EffectMetadata::new_with_callback_and_parent(callback, parent);
        // Set initial state to Dirty to defer computation
        metadata.set_state(crate::arena::ReactiveState::Dirty);
        let effect_id = effect_arena_insert(metadata);

        // Register as child of parent (if any)
        if let Some(parent_id) = parent {
            parent_id.add_child(effect_id);
        }

        // DON'T run initial computation - that's the whole point of lazy()
        // The first get() will call update_if_necessary() which will run the callback

        Arc::new(Self {
            signal_id,
            effect_id,
            value,
        })
    }

    /// Get the memoized value
    ///
    /// Returns the cached value. The value is automatically kept up-to-date
    /// by the internal Effect when dependencies change.
    ///
    /// If the cached value is stale (effect state is Dirty/Check) and either:
    /// - No effect is currently active, OR
    /// - The current effect doesn't yet depend on this computed
    ///
    /// Then this method will recompute the value before returning it.
    pub fn get(&self) -> T {
        // Recompute if stale and not already being tracked by current effect
        let should_recompute = self.effect_id.needs_work() && {
            match current_effect() {
                None => true, // No effect active, always recompute if stale
                Some(current) => {
                    // Check if current effect already depends on us
                    current
                        .sources()
                        .is_none_or(|sources| !sources.contains(&self.signal_id))
                }
            }
        };

        if should_recompute {
            self.effect_id.update_if_necessary();
        }

        // Track this computed as a dependency if we're inside an effect
        // This uses the same CURRENT_EFFECT mechanism as Signal::track_dependency()
        if let Some(effect_id) = current_effect() {
            // Subscribe the effect to this computed's output signal
            effect_id.add_source(self.signal_id);
            let subscriber = crate::arena::AnySubscriber::new(effect_id, false);
            self.signal_id.add_subscriber(subscriber);
        }

        // Return cached value (Effect keeps it up-to-date)
        self.value
            .read()
            .clone()
            .expect("computed value should always be set after creation")
    }

    /// Mark cache as dirty (called by sources when they change)
    ///
    /// Note: With the Effect-based implementation, this is typically not needed
    /// as the Effect handles invalidation automatically. This method is kept
    /// for backwards compatibility and manual invalidation use cases.
    pub fn invalidate(&self) {
        // Mark the effect as pending so it will re-run
        crate::arena::mark_effect_pending(self.effect_id);
    }

    /// Get the SignalId for this computed value (internal use only)
    #[allow(dead_code)]
    pub(crate) fn signal_id(&self) -> SignalId {
        self.signal_id
    }

    /// Get the EffectId for this computed value (internal use only)
    #[allow(dead_code)]
    pub(crate) fn effect_id(&self) -> EffectId {
        self.effect_id
    }
}

impl<T> Drop for Computed<T> {
    fn drop(&mut self) {
        // Check if this computed was created inside another effect (is a child)
        let has_parent = self.effect_id.parent().is_some();

        if has_parent {
            // Child computeds created inside a parent's callback should NOT be cleaned up
            // when their Arc<Computed> is dropped. The parent owns them and will clean them
            // up when it re-runs (via destroy_children) or when the parent is dropped.
            //
            // This allows child computeds to remain active even after the local variable
            // goes out of scope in the parent's callback.
            return;
        }

        // For top-level computeds (no parent), do full cleanup:

        // 1. Remove from pending set (if pending)
        remove_from_pending_set(self.effect_id);

        // 2. Get all sources from effect arena
        let sources = if let Some(sources) = self
            .effect_id
            .with(super::arena::effect_arena::EffectMetadata::get_sources)
        {
            sources
        } else {
            // Already removed, still try to clean up signal_id
            signal_arena_remove(self.signal_id);
            return;
        };

        // 3. Remove ourselves from each signal's subscribers
        with_signal_arena(|signal_arena| {
            for source_id in sources {
                if let Some(node) = signal_arena.get(source_id.index()) {
                    node.subscribers
                        .write()
                        .retain(|sub| sub.effect_id != self.effect_id);
                }
            }
        });

        // 4. Deallocate effect_id from effect arena
        effect_arena_remove(self.effect_id);

        // 5. Deallocate signal_id from signal arena
        signal_arena_remove(self.signal_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn computed_caches_value() {
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        let computed = Computed::new(move || {
            call_count_clone.fetch_add(1, Ordering::Relaxed);
            42
        });

        let v1 = computed.get();
        let v2 = computed.get();
        let v3 = computed.get();

        assert_eq!(v1, 42);
        assert_eq!(v2, 42);
        assert_eq!(v3, 42);
        assert_eq!(call_count.load(Ordering::Relaxed), 1); // Called only once during creation!
    }

    #[test]
    fn computed_recomputes_when_invalidated() {
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        let computed = Computed::new(move || {
            call_count_clone.fetch_add(1, Ordering::Relaxed);
            42
        });

        let _v1 = computed.get();
        assert_eq!(call_count.load(Ordering::Relaxed), 1);

        computed.invalidate();

        // Flush effects to process the invalidation
        crate::effect::flush_effects();

        let _v2 = computed.get();
        assert_eq!(call_count.load(Ordering::Relaxed), 2);

        let _v3 = computed.get();
        assert_eq!(call_count.load(Ordering::Relaxed), 2); // Still cached
    }

    #[test]
    fn computed_only_notifies_on_change() {
        // Test that signal version only increments when value actually changes
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0));
        let counter_clone = counter.clone();

        let computed = Computed::new(move || {
            // Always returns the same value
            counter_clone.fetch_add(1, Ordering::Relaxed);
            42
        });

        // Initial computation sets version to 1
        let initial_version = computed.signal_id().version().unwrap();
        assert_eq!(initial_version, 1);

        // Invalidate and re-run
        computed.invalidate();
        crate::effect::flush_effects();

        // Value didn't change, so version should still be 1
        // (the effect ran, but signal wasn't notified because value is same)
        let new_version = computed.signal_id().version().unwrap();
        assert_eq!(
            new_version, 1,
            "Version should not increment when value doesn't change"
        );

        // Effect ran twice though
        assert_eq!(counter.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn computed_notifies_on_actual_change() {
        // Test that signal version increments when value actually changes
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0));
        let counter_clone = counter.clone();

        let computed = Computed::new(move || {
            // Returns incrementing values
            counter_clone.fetch_add(1, Ordering::Relaxed)
        });

        // Initial computation: counter becomes 1, returns 0
        assert_eq!(computed.get(), 0);
        let initial_version = computed.signal_id().version().unwrap();
        assert_eq!(initial_version, 1);

        // Invalidate and re-run
        computed.invalidate();
        crate::effect::flush_effects();

        // Value changed (0 -> 1), so version should increment
        assert_eq!(computed.get(), 1);
        let new_version = computed.signal_id().version().unwrap();
        assert_eq!(
            new_version, 2,
            "Version should increment when value changes"
        );
    }

    #[test]
    fn test_computed_lazy_defers_computation() {
        // Verify lazy computed doesn't run until first get()
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        // Create lazy computed
        let computed = Computed::lazy(move || {
            call_count_clone.fetch_add(1, Ordering::Relaxed);
            42
        });

        // Should not have run yet
        assert_eq!(
            call_count.load(Ordering::Relaxed),
            0,
            "Lazy computed should not run during creation"
        );

        // First get() triggers computation
        let v1 = computed.get();
        assert_eq!(v1, 42);
        assert_eq!(
            call_count.load(Ordering::Relaxed),
            1,
            "Lazy computed should run on first get()"
        );

        // Second get() uses cached value
        let v2 = computed.get();
        assert_eq!(v2, 42);
        assert_eq!(
            call_count.load(Ordering::Relaxed),
            1,
            "Lazy computed should cache value after first get()"
        );
    }

    #[test]
    fn test_computed_auto_recompute_on_stale_access() {
        // Verify stale computed recomputes when accessed outside effect
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0));
        let counter_clone = counter.clone();

        let computed = Computed::new(move || counter_clone.fetch_add(1, Ordering::Relaxed));

        // Initial computation
        assert_eq!(computed.get(), 0);
        assert_eq!(counter.load(Ordering::Relaxed), 1);

        // Invalidate the computed
        computed.invalidate();

        // DON'T call flush_effects() - we want to test auto-recompute

        // Access outside any effect should trigger recompute
        let v = computed.get();
        assert_eq!(v, 1, "Should have recomputed automatically");
        assert_eq!(
            counter.load(Ordering::Relaxed),
            2,
            "Should have run computation again"
        );

        // Accessing again should use cached value
        let v2 = computed.get();
        assert_eq!(v2, 1);
        assert_eq!(
            counter.load(Ordering::Relaxed),
            2,
            "Should not recompute if clean"
        );
    }

    #[test]
    fn test_computed_lazy_with_dependencies() {
        // Test that lazy computed works correctly with reactive dependencies
        use crate::Signal;

        let signal = Signal::new();
        let signal_id = signal.node_id();
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        // Create a lazy computed that depends on a signal
        let computed = Computed::lazy(move || {
            call_count_clone.fetch_add(1, Ordering::Relaxed);
            // Track dependency on the signal
            signal_id.track_dependency();
            42
        });

        // Should not have run yet
        assert_eq!(call_count.load(Ordering::Relaxed), 0);

        // First access triggers computation
        assert_eq!(computed.get(), 42);
        assert_eq!(call_count.load(Ordering::Relaxed), 1);

        // Emit signal to invalidate the computed
        signal.emit();
        crate::effect::flush_effects();

        // Accessing again after invalidation should recompute
        assert_eq!(computed.get(), 42);
        assert_eq!(call_count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn test_computed_recompute_inside_effect_first_access() {
        // Test that computed recomputes on first access within an effect
        // even if the effect doesn't yet depend on it
        use crate::Effect;

        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        let computed = Computed::new(move || {
            call_count_clone.fetch_add(1, Ordering::Relaxed);
            100
        });

        // Initial computation
        assert_eq!(computed.get(), 100);
        assert_eq!(call_count.load(Ordering::Relaxed), 1);

        // Invalidate the computed
        computed.invalidate();
        crate::effect::flush_effects();

        // Access inside an effect for the first time (effect doesn't yet depend on computed)
        let computed_clone = computed.clone();
        let effect_run_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let effect_count_clone = effect_run_count.clone();
        let _effect = Effect::new(move || {
            effect_count_clone.fetch_add(1, Ordering::Relaxed);
            // First access within this effect - should trigger recompute
            let _val = computed_clone.get();
        });

        // Effect ran once, and computed recomputed because effect didn't yet depend on it
        assert_eq!(effect_run_count.load(Ordering::Relaxed), 1);
        assert_eq!(call_count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn test_combined_lazy_and_auto_recompute() {
        // Comprehensive test combining both enhancements:
        // 1. Lazy initialization (Enhancement 2)
        // 2. Auto-recompute on stale access (Enhancement 1)

        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        // Create lazy computed
        let computed = Computed::lazy(move || {
            call_count_clone.fetch_add(1, Ordering::Relaxed);
            call_count_clone.load(Ordering::Relaxed) * 10
        });

        // Enhancement 2: Verify no computation during creation
        assert_eq!(call_count.load(Ordering::Relaxed), 0);

        // Enhancement 1: First access outside effect triggers computation
        let v1 = computed.get();
        assert_eq!(v1, 10); // call_count was 0, became 1, returns 1 * 10 = 10
        assert_eq!(call_count.load(Ordering::Relaxed), 1);

        // Subsequent accesses use cached value
        let v2 = computed.get();
        assert_eq!(v2, 10);
        assert_eq!(call_count.load(Ordering::Relaxed), 1);

        // Enhancement 1: Invalidate and access outside effect auto-recomputes
        computed.invalidate();
        // Don't call flush_effects() - test auto-recompute

        let v3 = computed.get();
        assert_eq!(v3, 20); // call_count was 1, became 2, returns 2 * 10 = 20
        assert_eq!(call_count.load(Ordering::Relaxed), 2);

        // Cache still works after auto-recompute
        let v4 = computed.get();
        assert_eq!(v4, 20);
        assert_eq!(call_count.load(Ordering::Relaxed), 2);
    }
}
