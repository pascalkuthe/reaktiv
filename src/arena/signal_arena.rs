// Signal arena - storage for signal metadata
//
// This module defines SignalMetadata, SignalId, and provides helper functions
// for working with the signal arena.
//
// PRINCIPLED MODEL:
// - Signal = only emits (subscribers, NO sources)
// - Effect = only tracks dependencies (sources + pending_run)
// - Computed = Signal + Effect (has BOTH SignalId AND EffectId)
//
// PUSH-PULL MODEL:
// - Signals track which effects write to them (SIGNAL_WRITERS map)
// - Effects track their outputs in addition to sources
// - When reading a signal, pull() ensures writer effects run first

use crate::hash::FastHashBuilder;
use papaya::HashMap as PapayaHashMap;
use parking_lot::{RwLock, RwLockReadGuard};
use slab::Slab;
use std::collections::HashSet;
use std::sync::LazyLock;

use super::EffectId;

/// Global signal arena - stores all signal metadata
static SIGNAL_ARENA: RwLock<Slab<SignalMetadata>> = RwLock::new(Slab::new());

/// Global map: SignalId -> effects that write to this signal
///
/// This is the core of the push-pull system. When an effect writes to a signal
/// (via emit()), we register the effect as a writer. When reading a signal,
/// we can pull() to ensure all pending writer effects run first.
///
/// Uses papaya's lock-free HashMap for efficient concurrent reads.
/// We use the default RandomState hasher from papaya (which uses foldhash internally).
static SIGNAL_WRITERS: LazyLock<
    PapayaHashMap<SignalId, RwLock<HashSet<EffectId, FastHashBuilder>>>,
> = LazyLock::new(PapayaHashMap::new);

/// Execute a closure with the writers of a signal (if any exist)
///
/// Returns None if no effects write to this signal.
/// The closure receives an iterator over the writer EffectIds.
pub fn with_signal_writers<F, R>(signal_id: SignalId, f: F) -> Option<R>
where
    F: FnOnce(&HashSet<EffectId, FastHashBuilder>) -> R,
{
    let guard = SIGNAL_WRITERS.pin();
    guard.get(&signal_id).map(|set| {
        let read_guard = set.read();
        f(&read_guard)
    })
}

/// Register an effect as a writer of a signal
///
/// Called from Signal::emit() when an effect is currently running.
/// This tracks which effects produce values for which signals.
pub fn add_signal_writer(signal_id: SignalId, effect_id: EffectId) {
    let guard = SIGNAL_WRITERS.pin();
    // Get or insert empty set, then add writer
    guard
        .get_or_insert_with(signal_id, || {
            RwLock::new(HashSet::with_hasher(FastHashBuilder))
        })
        .write()
        .insert(effect_id);
}

/// Remove an effect from a signal's writers
///
/// Called when an effect re-runs (before execution) to clear old outputs.
pub fn remove_signal_writer(signal_id: SignalId, effect_id: EffectId) {
    let guard = SIGNAL_WRITERS.pin();
    if let Some(set) = guard.get(&signal_id) {
        set.write().remove(&effect_id);
    }
}

/// Unique identifier for a signal node in the arena.
///
/// This is a zero-cost wrapper around a slab index.
/// When a Signal is dropped, it removes itself from the arena,
/// making this SignalId stale. Accessing a stale SignalId returns None.
#[repr(transparent)]
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct SignalId(u32);

impl SignalId {
    /// Create a new SignalId from a raw index
    pub fn new(index: u32) -> Self {
        Self(index)
    }

    /// Convert to usize for slab indexing
    pub fn index(self) -> usize {
        self.0 as usize
    }

    /// Access the signal metadata with a closure (read-only)
    ///
    /// Returns None if the signal has been removed (stale access).
    pub fn with<F, R>(self, f: F) -> Option<R>
    where
        F: FnOnce(&SignalMetadata) -> R,
    {
        let arena = SIGNAL_ARENA.read();
        arena.get(self.index()).map(f)
    }

    /// Track this signal as a dependency (if an effect is currently executing)
    ///
    /// This uses CURRENT_EFFECT to automatically subscribe the running effect
    /// to this signal. Called internally when reading reactive values.
    /// This is the same functionality as Signal::track_dependency() but can be
    /// called when you only have a SignalId.
    ///
    /// # Push-Pull System
    /// Before tracking the dependency, this method calls `pull()` to ensure
    /// the signal's value is fresh. This guarantees that any pending writer
    /// effects have run before we read the value.
    pub fn track_dependency(self) {
        use super::effect_arena::current_effect;

        // Pull first to ensure signal is fresh before reading
        // Use skip_skippable=false for backwards compatibility (don't skip any effects)
        self.pull(false);

        if let Some(effect_id) = current_effect() {
            // Subscribe the effect to this signal
            effect_id.add_source(self);
            self.add_subscriber(effect_id);
        }
    }

    /// Add a subscriber to this signal
    ///
    /// Uses HashSet to automatically deduplicate subscribers.
    pub fn add_subscriber(self, effect_id: EffectId) {
        self.with(|metadata| {
            let mut subscribers = metadata.subscribers.write();
            subscribers.insert(effect_id);
        });
    }

    /// Remove a subscriber from this signal
    pub fn remove_subscriber(self, effect_id: EffectId) {
        self.with(|metadata| {
            let mut subscribers = metadata.subscribers.write();
            subscribers.remove(&effect_id);
        });
    }

    /// Execute a closure with the subscribers of this signal
    ///
    /// This avoids allocation by passing a reference to the HashSet directly.
    /// Safe to call methods on other effects/signals within the closure since
    /// they have separate locks.
    pub fn with_subscribers<F, R>(self, f: F) -> Option<R>
    where
        F: FnOnce(&HashSet<EffectId, FastHashBuilder>) -> R,
    {
        self.with(|metadata| {
            let subscribers = metadata.subscribers.read();
            f(&subscribers)
        })
    }

    // =========================================================================
    // Three-state reactive system methods
    // =========================================================================

    /// Mark all subscriber effects as Check recursively.
    ///
    /// This is used for propagating the Check state through the dependency graph
    /// when an effect writes to a signal. The downstream dependents need to verify
    /// their sources before recomputing.
    pub fn mark_subscribers_check(self) {
        self.with_subscribers(|subscribers| {
            if !subscribers.is_empty() {
                cov_mark::hit!(signal_marking_subscribers_check);
            }
            for effect_id in subscribers {
                effect_id.mark_check_recursive();
            }
        });
    }

    /// Notify all subscribers that this signal has changed.
    ///
    /// This marks direct subscribers as Dirty (pending) and their downstream
    /// dependents as Check. Used by Computed when its value changes.
    ///
    /// Note: Unlike Signal::emit(), this does NOT register the current effect
    /// as a writer, since it's used for internal notification (e.g., computed
    /// values notifying their dependents).
    pub fn notify_subscribers(self) {
        use super::effect_arena::mark_effect_pending;

        self.with_subscribers(|subscribers| {
            for effect_id in subscribers {
                // Direct subscribers are Dirty (source definitely changed)
                mark_effect_pending(*effect_id);

                // Their downstream dependents are Check (might be affected)
                effect_id.with_outputs(|outputs| {
                    for output in outputs {
                        output.mark_subscribers_check();
                    }
                });
            }
        });
    }

    /// Ensure this signal's value is fresh by running any stale writer effects.
    ///
    /// This is the "pull" part of the push-pull reactive system. Before reading
    /// a signal's value, we verify that all effects which write to this signal
    /// have run if they are stale (state != Clean).
    ///
    /// Uses the three-state system: effects in Check state will verify their
    /// sources first, and only run if they become Dirty.
    ///
    /// # Algorithm
    /// 1. Get all effects that write to this signal
    /// 2. For each writer that needs work (state != Clean):
    ///    a. Call update_if_necessary() which handles Check vs Dirty
    ///    b. This recursively pulls the writer's sources if in Check state
    ///
    /// # Skippable Effects
    /// If `skip_skippable` is true, writers that are skippable are skipped.
    /// This allows consumers to avoid running producer effects when over budget.
    pub fn pull(self, skip_skippable: bool) {
        use super::effect_arena::current_effect;

        // Avoid infinite recursion - don't pull while we're in the middle of running an effect
        let current = current_effect();

        // SIGNAL_WRITERS is a lock-free papaya HashMap, so we can iterate directly
        with_signal_writers(self, |writers| {
            for &writer_id in writers {
                // Skip if this is the currently executing effect (avoid infinite recursion)
                if Some(writer_id) == current {
                    continue;
                }

                // Skip if we're told to skip skippable effects and this writer is skippable
                if skip_skippable && writer_id.is_skippable() {
                    continue;
                }

                // Use update_if_necessary for three-state handling
                writer_id.update_if_necessary(skip_skippable);
            }
        });
    }
}

/// Metadata for a signal stored in the arena.
///
/// This contains all the reactive tracking information for a signal:
/// - subscribers: effects/computeds that depend on this signal
///
/// PRINCIPLED MODEL: Signals do NOT track sources. Only Effects track sources.
/// Computed values use BOTH a SignalId (for output) and EffectId (for input tracking).
///
/// Note: The actual signal value lives outside the arena in the Signal struct.
/// This separation improves cache locality and keeps the arena lightweight.
#[derive(Debug)]
pub struct SignalMetadata {
    /// Observers subscribed to this signal.
    /// When the signal changes, all subscribers are notified.
    /// Uses HashSet for O(1) insertion/lookup and automatic deduplication.
    /// FastHashBuilder provides zero-sized hasher for memory efficiency.
    pub(crate) subscribers: RwLock<HashSet<EffectId, FastHashBuilder>>,
}

impl SignalMetadata {
    /// Create new signal metadata with default values
    pub fn new() -> Self {
        Self {
            subscribers: RwLock::new(HashSet::with_hasher(FastHashBuilder)),
        }
    }
}

impl Default for SignalMetadata {
    fn default() -> Self {
        Self::new()
    }
}

/// Insert a signal into the arena and return its ID
pub fn signal_arena_insert(metadata: SignalMetadata) -> SignalId {
    let mut arena = SIGNAL_ARENA.write();
    let entry = arena.vacant_entry();
    let key = entry.key();
    entry.insert(metadata);
    SignalId::new(key as u32)
}

/// Remove a signal from the arena
pub fn signal_arena_remove(id: SignalId) -> Option<SignalMetadata> {
    let mut arena = SIGNAL_ARENA.write();
    if arena.contains(id.index()) {
        Some(arena.remove(id.index()))
    } else {
        None
    }
}

/// Access the global signal arena directly (for low-level operations)
///
/// Use this for operations that need direct arena access, like iterating
/// or bulk operations. For single-signal operations, prefer SignalId methods.
pub fn with_signal_arena<F, R>(f: F) -> R
where
    F: FnOnce(RwLockReadGuard<'_, Slab<SignalMetadata>>) -> R,
{
    let arena = SIGNAL_ARENA.read();
    f(arena)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_access_returns_none() {
        let metadata = SignalMetadata::new();
        let id = signal_arena_insert(metadata);

        signal_arena_remove(id);

        // All operations should return None for stale ID
        assert!(id.with_subscribers(|_| ()).is_none());
    }
}
