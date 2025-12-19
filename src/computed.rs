use crate::arena::{
    EffectMetadata, SignalMetadata, current_effect, effect_arena_insert, set_effect_parent,
    signal_arena_insert,
};
use crate::{Effect, Signal};
use parking_lot::{Mutex, RwLock};
use std::sync::Arc;

/// Memoized derived value that automatically updates when dependencies change
///
/// A Computed is like an Effect that caches its result. It only recomputes when
/// its dependencies change, and only notifies subscribers when the output value
/// actually changes (requires `T: PartialEq`).
///
/// # How it works
/// Internally, a Computed is composed of:
/// - An Effect that tracks dependencies and recomputes the value
/// - A Signal that other effects/computeds can subscribe to
/// - A cached value that's only recomputed when dependencies change
///
/// When a dependency changes:
/// 1. The internal Effect re-runs and computes a new value
/// 2. If the new value differs from the cached value, the Signal notifies subscribers
/// 3. If the value is the same, subscribers are NOT notified (avoids unnecessary work)
///
/// # Example
/// ```ignore
/// let voltage_signal = Signal::new();
/// let current_signal = Signal::new();
///
/// // Computed runs immediately and caches the result
/// let power = Computed::new(|| {
///     voltage_signal.track_dependency();
///     current_signal.track_dependency();
///     voltage * current
/// });
///
/// let p1 = power.get();  // Returns cached value (no recomputation)
/// let p2 = power.get();  // Still cached
///
/// voltage_signal.emit();
/// flush_effects();       // Recomputes power
///
/// let p3 = power.get();  // Returns new cached value
/// ```
///
/// # Lazy evaluation
/// Use `Computed::lazy()` to defer the initial computation until first access:
/// ```ignore
/// let expensive = Computed::lazy(|| heavy_computation());
/// // Computation hasn't run yet
/// let result = expensive.get();  // Runs now
/// ```
pub struct Computed<T> {
    /// Signal for output - others subscribe to this to get notified when this computed changes.
    /// Owned by Computed so Signal::Drop handles cleanup automatically.
    signal: Signal,

    /// Effect that handles subscription logic and recomputation.
    /// Owned by Computed so Effect::Drop handles cleanup automatically.
    effect: Effect,

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
        Self::new_internal(f, false, false)
    }

    /// Create a new skippable computed value
    ///
    /// Skippable computeds can be deferred under load. They are typically producer
    /// computations (like validation) that can be skipped when consumers need to
    /// run within a time budget.
    pub fn new_skippable<F>(f: F) -> Arc<Self>
    where
        F: FnMut() -> T + Send + 'static,
    {
        Self::new_internal(f, false, true)
    }

    /// Internal constructor for computed values
    fn new_internal<F>(f: F, lazy: bool, skippable: bool) -> Arc<Self>
    where
        F: FnMut() -> T + Send + 'static,
    {
        // Allocate signal metadata for output (subscribers)
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

            // Only notify subscribers if value actually changed
            if changed {
                signal_id.notify_subscribers();
            }
        });

        let metadata =
            EffectMetadata::new_with_callback_parent_and_skippable(callback, parent, skippable);

        // Set initial state to Dirty for lazy evaluation
        if lazy {
            metadata.set_state(crate::arena::ReactiveState::Dirty);
        }

        let effect_id = effect_arena_insert(metadata);

        // Register as child of parent (if any)
        if let Some(parent_id) = parent {
            set_effect_parent(effect_id, parent_id);
            parent_id.add_child(effect_id);
        }

        // Run initial computation unless lazy
        if !lazy {
            // Run initial computation with CURRENT_EFFECT set to track dependencies
            // Signal::track_dependency() will check current_effect() and subscribe automatically
            // The guard ensures CURRENT_EFFECT is restored even if callback panics
            let _guard = crate::arena::CurrentEffectGuard::new(Some(effect_id));
            effect_id.run_callback();
            // Guard automatically restores previous current effect when dropped
        }

        // Wrap IDs in owning types for automatic cleanup via Drop
        Arc::new(Self {
            signal: Signal::from_raw(signal_id),
            effect: Effect::from_raw(effect_id),
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
        Self::new_internal(f, true, false)
    }

    /// Create a lazy skippable computed value
    ///
    /// Combines lazy evaluation with skippable behavior. Defers computation until
    /// first access, and can be deferred under load.
    pub fn lazy_skippable<F>(f: F) -> Arc<Self>
    where
        F: FnMut() -> T + Send + 'static,
    {
        Self::new_internal(f, true, true)
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
        let effect_id = self.effect.id();
        let signal_id = self.signal.id();

        // Recompute if stale and not already being tracked by current effect
        let should_recompute = effect_id.needs_work() && {
            match current_effect() {
                None => true, // No effect active, always recompute if stale
                Some(current) => {
                    // Check if current effect already depends on us (O(1) lookup)
                    !current.has_source(signal_id)
                }
            }
        };

        if should_recompute {
            effect_id.update_if_necessary(false);
        }

        // Track this computed as a dependency if we're inside an effect
        // This uses the same CURRENT_EFFECT mechanism as Signal::track_dependency()
        if let Some(current_effect_id) = current_effect() {
            // Subscribe the effect to this computed's output signal
            current_effect_id.add_source(signal_id);
            signal_id.add_subscriber(current_effect_id);
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
        crate::arena::mark_effect_pending(self.effect.id());
    }
}

// Note: No custom Drop needed - Effect and Signal handle their own cleanup

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
        // Test that subscribers aren't notified when value doesn't actually change
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0));
        let counter_clone = counter.clone();

        let computed = Computed::new(move || {
            // Always returns the same value
            counter_clone.fetch_add(1, Ordering::Relaxed);
            42
        });

        // Create an effect that depends on the computed
        let effect_run_count = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0));
        let effect_run_count_clone = effect_run_count.clone();
        let computed_signal_id = computed.signal.id();
        let _effect = crate::Effect::new(move || {
            computed_signal_id.track_dependency();
            effect_run_count_clone.fetch_add(1, Ordering::Relaxed);
        });

        // Effect ran once on creation
        assert_eq!(effect_run_count.load(Ordering::Relaxed), 1);

        // Invalidate computed and re-run
        computed.invalidate();
        crate::effect::flush_effects();

        // Computed ran again (counter incremented)
        assert_eq!(counter.load(Ordering::Relaxed), 2);

        // But effect should NOT have run again since value didn't change
        assert_eq!(
            effect_run_count.load(Ordering::Relaxed),
            1,
            "Effect should not re-run when computed value doesn't change"
        );
    }

    #[test]
    fn computed_notifies_on_actual_change() {
        // Test that subscribers are notified when value actually changes
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0));
        let counter_clone = counter.clone();

        let computed = Computed::new(move || {
            // Returns incrementing values
            counter_clone.fetch_add(1, Ordering::Relaxed)
        });

        // Initial computation: counter becomes 1, returns 0
        assert_eq!(computed.get(), 0);

        // Create an effect that depends on the computed
        let effect_run_count = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0));
        let effect_run_count_clone = effect_run_count.clone();
        let computed_signal_id = computed.signal.id();
        let _effect = crate::Effect::new(move || {
            computed_signal_id.track_dependency();
            effect_run_count_clone.fetch_add(1, Ordering::Relaxed);
        });

        // Effect ran once on creation
        assert_eq!(effect_run_count.load(Ordering::Relaxed), 1);

        // Invalidate and re-run
        computed.invalidate();
        crate::effect::flush_effects();

        // Value changed (0 -> 1), so effect should have run again
        assert_eq!(computed.get(), 1);
        assert_eq!(
            effect_run_count.load(Ordering::Relaxed),
            2,
            "Effect should re-run when computed value changes"
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
        let signal_id = signal.id();
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

    #[test]
    fn skippable_computed_flag_set_correctly() {
        // Test that Computed::new_skippable() sets the skippable flag
        let computed = Computed::new_skippable(|| 42);
        assert!(computed.effect.id().is_skippable());

        // Test that regular Computed::new() creates non-skippable computeds
        let regular_computed = Computed::new(|| 42);
        assert!(!regular_computed.effect.id().is_skippable());

        // Test lazy_skippable
        let lazy_skippable = Computed::lazy_skippable(|| 42);
        assert!(lazy_skippable.effect.id().is_skippable());

        // Test regular lazy
        let lazy_regular = Computed::lazy(|| 42);
        assert!(!lazy_regular.effect.id().is_skippable());
    }
}
