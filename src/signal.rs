use crate::arena::signal_arena::add_signal_writer;
use crate::arena::{
    EffectId, SignalId, SignalMetadata, current_effect, mark_effect_pending, signal_arena_insert,
    signal_arena_remove,
};
use crate::effect::schedule_effect_processing;
use crate::transaction::is_transaction_active;

/// Lightweight reactive marker that tracks dependencies without owning data
///
/// A Signal is just 4 bytes of metadata - your actual values stay in your structs.
/// This is different from wrapping signals like `Signal<T>` that own the data.
///
/// # Usage
/// ```ignore
/// struct MyComponent {
///     value: f64,
///     signal: Signal,  // Just metadata, doesn't own the value
/// }
///
/// impl MyComponent {
///     fn set(&mut self, v: f64) {
///         self.value = v;
///         self.signal.emit();  // Notify subscribers
///     }
///
///     fn get(&self) -> f64 {
///         self.signal.track_dependency();  // Track if inside an effect
///         self.value
///     }
/// }
/// ```
pub struct Signal {
    node_id: SignalId,
}

impl Signal {
    /// Create a new signal and allocate it in the arena
    pub fn new() -> Self {
        let metadata = SignalMetadata::new();
        let node_id = signal_arena_insert(metadata);
        Signal { node_id }
    }

    /// Get the node ID for this signal (internal use only)
    pub(crate) fn node_id(&self) -> SignalId {
        self.node_id
    }

    /// Create a Signal wrapper from an existing SignalId (internal use only)
    ///
    /// Use this when you've already created the SignalId externally
    /// (e.g., in Computed) and just want Signal to handle cleanup via Drop.
    pub(crate) fn from_raw(id: SignalId) -> Self {
        Self { node_id: id }
    }

    /// Track this signal as a dependency (if an effect is currently executing)
    ///
    /// This uses CURRENT_EFFECT to automatically subscribe the running effect
    /// to this signal. Called internally when reading reactive values.
    pub fn track_dependency(&self) {
        if let Some(effect_id) = current_effect() {
            // Subscribe the effect to this signal
            effect_id.add_source(self.node_id);
            self.node_id.add_subscriber(effect_id);
        }
    }

    /// Emit a change notification (default: from API, all subscribers notified)
    ///
    /// This is equivalent to emit_from_api().
    /// Use this for regular data updates from user/API.
    pub fn emit(&self) {
        self.emit_from_api();
    }

    /// Emit from API: signal changed due to data update
    ///
    /// All subscribers are notified (both data-dependent and UI-dependent effects).
    /// This is the default behavior for most signal changes.
    pub fn emit_from_api(&self) {
        self.emit_with_origin(false);
    }

    /// Emit from UI: signal changed due to UI internal state
    ///
    /// Only subscribers that care about everything are notified.
    /// Subscribers with track_ui_updates_only=true are skipped.
    pub fn emit_from_ui(&self) {
        self.emit_with_origin(true);
    }

    /// Internal: emit with origin tracking
    fn emit_with_origin(&self, is_from_ui: bool) {
        // Track this signal as an output of current effect (for push-pull system)
        // This allows pull() to know which effects produce values for which signals.
        if let Some(effect_id) = current_effect() {
            // Check for read-write cycle: effect reads AND writes the same signal
            // This would cause an infinite loop, so we treat the read as untracked
            let is_source = effect_id.has_source(self.node_id);
            if is_source {
                eprintln!(
                    "Warning: Effect {:?} both reads and writes signal {:?}. \
                     This would cause an infinite loop. \
                     The read is being treated as untracked.",
                    effect_id, self.node_id
                );
                // Remove the subscription (treat as untracked)
                self.node_id.remove_subscriber(effect_id);
                effect_id.remove_source(self.node_id);
            }

            // Add to effect's outputs
            effect_id.add_output(self.node_id);
            // Add effect to signal's writers
            add_signal_writer(self.node_id, effect_id);
        }

        // Notify subscribers (filtered by origin)
        self.notify_subscribers(is_from_ui);
    }

    /// Notify all subscribers using the three-state Leptos-style system.
    ///
    /// When a signal changes:
    /// 1. Direct subscribers are marked Dirty (they definitely need to recompute)
    /// 2. Their downstream dependents are marked Check (they might need to recompute)
    ///
    /// If not in a transaction or effect callback, schedules effect processing.
    /// Actual processing is deferred until `flush_effects()` is called or when
    /// a transaction ends, enabling efficient batching of multiple signal changes.
    fn notify_subscribers(&self, is_from_ui: bool) {
        self.node_id.with_subscribers(|subscribers| {
            for effect_id in subscribers {
                // Filter: if this is a UI-only update and subscriber only wants data updates, skip
                if is_from_ui && effect_id.is_ui_updates_only() {
                    continue;
                }

                // Direct subscribers are Dirty (source definitely changed)
                // This uses mark_effect_pending which sets state to Dirty and adds to pending set
                mark_effect_pending(*effect_id);

                // Their downstream dependents are Check (might be affected)
                // Recursively mark effects that depend on this subscriber's outputs
                effect_id.with_outputs(|outputs| {
                    for output in outputs {
                        output.mark_subscribers_check();
                    }
                });
            }
        });

        // If not in a transaction AND not inside an effect callback, schedule processing.
        // When inside an effect callback (current_effect is Some), we defer processing to avoid
        // infinite recursion when effects create child effects or trigger signal changes.
        // Note: We schedule processing rather than processing immediately to enable debouncing.
        // The actual processing happens when flush_effects() is called.
        if !is_transaction_active() && current_effect().is_none() {
            schedule_effect_processing();
        }
    }

    /// Subscribe an observer to this signal
    pub fn subscribe(&self, effect_id: EffectId) {
        self.node_id.add_subscriber(effect_id);
    }

    /// Unsubscribe an effect from this signal
    pub fn unsubscribe(&self, effect_id: EffectId) {
        self.node_id.remove_subscriber(effect_id);
    }
}

impl Drop for Signal {
    fn drop(&mut self) {
        // Notify all subscribers to remove this signal from their sources
        self.node_id.with_subscribers(|subscribers| {
            for effect_id in subscribers {
                // Remove this signal from the effect's sources list
                effect_id.remove_source(self.node_id);
            }
        });

        // Deallocate from arena
        signal_arena_remove(self.node_id);
    }
}

// NOTE: Signal intentionally does NOT implement Clone.
// This is a single-ownership model - cloning would create double-free risk
// when the signal is dropped and tries to deallocate from the arena twice.
// If you need multiple references to a signal, use Arc<Signal> or share the SignalId.

impl Default for Signal {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Effect;

    #[test]
    fn signal_tracks_multiple_accesses_deduplicated() {
        let signal = Signal::new();
        let signal_id = signal.node_id();

        // Create an effect that tracks the signal multiple times
        let effect = Effect::new(move || {
            // Read same signal multiple times
            signal_id.track_dependency();
            signal_id.track_dependency();
            signal_id.track_dependency();
        });

        // Duplicate subscriptions are handled - the effect sees only one source
        let source_count = effect
            .id()
            .with_sources(|sources| sources.count())
            .unwrap_or(0);
        assert_eq!(source_count, 1);
    }
}
