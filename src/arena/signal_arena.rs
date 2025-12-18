// Signal arena - storage for signal metadata
//
// This module defines SignalMetadata, SignalId, and provides helper functions
// for working with the signal arena.
//
// PRINCIPLED MODEL:
// - Signal = only emits (version + subscribers, NO sources)
// - Effect = only tracks dependencies (sources + pending_run)
// - Computed = Signal + Effect (has BOTH SignalId AND EffectId)
//
// PUSH-PULL MODEL:
// - Signals track which effects write to them (SIGNAL_WRITERS map)
// - Effects track their outputs in addition to sources
// - When reading a signal, pull() ensures writer effects run first

use crate::hash::FastHashBuilder;
use indexmap::IndexSet;
use papaya::HashMap as PapayaHashMap;
use parking_lot::{RwLock, RwLockReadGuard};
use slab::Slab;
use smallvec::SmallVec;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

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
    PapayaHashMap<SignalId, RwLock<IndexSet<EffectId, FastHashBuilder>>>,
> = LazyLock::new(PapayaHashMap::new);

/// Execute a closure with the writers of a signal (if any exist)
///
/// Returns None if no effects write to this signal.
/// The closure receives an iterator over the writer EffectIds.
pub fn with_signal_writers<F, R>(signal_id: SignalId, f: F) -> Option<R>
where
    F: FnOnce(&IndexSet<EffectId, FastHashBuilder>) -> R,
{
    let guard = SIGNAL_WRITERS.pin();
    guard.get(&signal_id).map(|set| {
        let read_guard = set.read();
        f(&read_guard)
    })
}

/// Get a read guard to the writers of a signal
///
/// Returns None if no effects write to this signal.
/// The returned guard provides access to the IndexSet of writer EffectIds.
///
/// # Note
/// This function returns a guard that holds a read lock. The caller is responsible
/// for releasing the guard in a timely manner to avoid blocking other operations.
/// For most use cases, prefer `with_signal_writers()` which automatically manages
/// the guard lifetime.
pub fn get_signal_writers(signal_id: SignalId) -> Option<SmallVec<[EffectId; 4]>> {
    with_signal_writers(signal_id, |writers| writers.iter().copied().collect())
}

/// Register an effect as a writer of a signal
///
/// Called from Signal::emit() when an effect is currently running.
/// This tracks which effects produce values for which signals.
pub fn add_signal_writer(signal_id: SignalId, effect_id: EffectId) {
    let guard = SIGNAL_WRITERS.pin();
    // Get or insert empty set, then add writer
    guard
        .get_or_insert_with(signal_id, || RwLock::new(IndexSet::default()))
        .write()
        .insert(effect_id);
}

/// Remove an effect from a signal's writers
///
/// Called when an effect re-runs (before execution) to clear old outputs.
pub fn remove_signal_writer(signal_id: SignalId, effect_id: EffectId) {
    let guard = SIGNAL_WRITERS.pin();
    if let Some(set) = guard.get(&signal_id) {
        set.write().swap_remove(&effect_id);
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
        self.pull();

        if let Some(effect_id) = current_effect() {
            // Subscribe the effect to this signal
            effect_id.add_source(self);
            let subscriber = AnySubscriber::new(effect_id, false);
            self.add_subscriber(subscriber);
        }
    }

    /// Get the current version of this signal
    pub fn version(self) -> Option<u64> {
        self.with(SignalMetadata::get_version)
    }

    /// Increment the version of this signal (called when signal changes)
    pub fn increment_version(self) -> Option<u64> {
        self.with(SignalMetadata::increment_version)
    }

    /// Add a subscriber to this signal
    ///
    /// Uses IndexSet to automatically deduplicate subscribers.
    pub fn add_subscriber(self, subscriber: AnySubscriber) -> Option<()> {
        self.with(|metadata| {
            let mut subscribers = metadata.subscribers.write();
            subscribers.insert(subscriber);
        })
    }

    /// Remove a subscriber from this signal
    pub fn remove_subscriber(self, effect_id: EffectId) -> Option<()> {
        self.with(|metadata| {
            let mut subscribers = metadata.subscribers.write();
            subscribers.retain(|sub| sub.effect_id != effect_id);
        })
    }

    /// Get all subscribers of this signal
    pub fn subscribers(self) -> Option<Vec<AnySubscriber>> {
        self.with(|metadata| {
            let subscribers = metadata.subscribers.read();
            subscribers.iter().copied().collect()
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
        if let Some(subscribers) = self.subscribers() {
            if !subscribers.is_empty() {
                cov_mark::hit!(signal_marking_subscribers_check);
            }
            for sub in subscribers {
                sub.effect_id.mark_check_recursive();
            }
        }
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
    /// # Implementation Note
    /// We collect writer IDs to release the lock before calling update_if_necessary(),
    /// which might recursively call pull() on other signals.
    pub fn pull(self) {
        use super::effect_arena::current_effect;

        // Avoid infinite recursion - don't pull while we're in the middle of running an effect
        let current = current_effect();

        // Collect writers while holding lock, then release before processing
        if let Some(writers) = get_signal_writers(self) {
            for writer_id in writers {
                // Skip if this is the currently executing effect (avoid infinite recursion)
                if Some(writer_id) == current {
                    continue;
                }

                // Use update_if_necessary for three-state handling
                writer_id.update_if_necessary();
            }
        }
    }
}

/// Metadata for a signal stored in the arena.
///
/// This contains all the reactive tracking information for a signal:
/// - version: incremented on each change to detect staleness
/// - subscribers: effects/computeds that depend on this signal
///
/// PRINCIPLED MODEL: Signals do NOT track sources. Only Effects track sources.
/// Computed values use BOTH a SignalId (for output) and EffectId (for input tracking).
///
/// Note: The actual signal value lives outside the arena in the Signal struct.
/// This separation improves cache locality and keeps the arena lightweight.
#[derive(Debug)]
pub struct SignalMetadata {
    /// Version counter incremented on each change.
    /// Used by effects and computeds to detect if they need to recompute.
    pub(crate) version: AtomicU64,

    /// Observers subscribed to this signal.
    /// When the signal changes, all subscribers are notified.
    /// Uses IndexSet for O(1) insertion/lookup and automatic deduplication.
    /// FastHashBuilder provides zero-sized hasher for memory efficiency.
    pub(crate) subscribers: RwLock<IndexSet<AnySubscriber, FastHashBuilder>>,
}

impl SignalMetadata {
    /// Create new signal metadata with default values
    pub fn new() -> Self {
        Self {
            version: AtomicU64::new(0),
            subscribers: RwLock::new(IndexSet::default()),
        }
    }

    /// Get the current version
    pub fn get_version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    /// Increment the version (called when signal changes)
    pub fn increment_version(&self) -> u64 {
        self.version.fetch_add(1, Ordering::Release)
    }
}

impl Default for SignalMetadata {
    fn default() -> Self {
        Self::new()
    }
}

/// A subscriber (effect or computed) that depends on a signal.
///
/// This is stored in SignalMetadata.subscribers to track what needs
/// to be notified when the signal changes.
///
/// New design: instead of holding a `Weak<dyn Subscriber>`, we just store
/// the EffectId. When the effect is dropped, it unsubscribes itself.
///
/// Hash and Eq are implemented to allow storage in IndexSet for efficient
/// deduplication and lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AnySubscriber {
    /// The effect/computed node that's subscribed
    pub effect_id: EffectId,

    /// If true, this subscriber only cares about UI updates.
    /// Used to implement the update_ui tracking: effects can choose to
    /// ignore internal updates and only react to user-visible changes.
    pub track_ui_updates_only: bool,
}

impl AnySubscriber {
    /// Create a new subscriber
    pub fn new(effect_id: EffectId, track_ui_updates_only: bool) -> Self {
        Self {
            effect_id,
            track_ui_updates_only,
        }
    }
}

// Arena manipulation functions

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
    F: FnOnce(RwLockReadGuard<Slab<SignalMetadata>>) -> R,
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
        assert_eq!(id.version(), None);
        assert_eq!(id.increment_version(), None);
        assert_eq!(id.subscribers(), None);
    }
}
