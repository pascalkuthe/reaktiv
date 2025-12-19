// Effect arena - storage for effect and computed metadata
//
// This module defines EffectMetadata, a unified struct for both effects and
// computeds, and provides helper functions for working with the effect arena.
//
// The EffectMetadata struct contains:
// - sources: the signals this effect/computed depends on (stored as HashSet)
// - state: a three-state reactive state (Clean/Check/Dirty) for Leptos-style propagation
// - callback: the effect function stored directly in the arena (for effects)
//
// THREE-STATE REACTIVE SYSTEM (Leptos-style):
// - Clean (0): Value is current, use cached
// - Check (1): Might be stale, verify parents first (lazy propagation)
// - Dirty (2): Definitely stale, must recompute
//
// This enables efficient invalidation: instead of eagerly recomputing all
// downstream effects, we mark them as Check and verify lazily during pull().

use crate::hash::FastHashBuilder;
use indexmap::IndexSet;
use papaya::HashMap as PapayaHashMap;
use parking_lot::{Mutex, RwLock};
use slab::Slab;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU8, Ordering};

/// Reactive node states - uses u8 for AtomicU8 compatibility
///
/// The three-state system enables efficient propagation:
/// - Clean: Node's value is current, can use cached
/// - Check: Node might be stale, need to verify parents first
/// - Dirty: Node is definitely stale, must recompute
///
/// States only upgrade (Clean -> Check -> Dirty), never downgrade during
/// a single propagation phase. After recomputation, state resets to Clean.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReactiveState {
    /// Value is current, use cached
    Clean = 0,
    /// Might be stale, verify parents first
    Check = 1,
    /// Definitely stale, must recompute
    Dirty = 2,
}

impl ReactiveState {
    /// Convert from u8 to ReactiveState
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => ReactiveState::Clean,
            1 => ReactiveState::Check,
            _ => ReactiveState::Dirty,
        }
    }
}

use super::SignalId;

/// Tracks whether a signal is a source (read) and/or output (written) for this effect
#[derive(Clone, Copy, Default)]
pub(crate) struct SignalRelation {
    pub(crate) is_source: bool,
    pub(crate) is_output: bool,
}

/// Type alias for the inner filter map iterator over signal relations
type SignalFilterMapIter<'a> = std::iter::FilterMap<
    std::collections::hash_map::Iter<'a, SignalId, SignalRelation>,
    fn((&'a SignalId, &'a SignalRelation)) -> Option<SignalId>,
>;

/// Iterator over source SignalIds for an effect
pub struct SourcesIter<'a> {
    inner: SignalFilterMapIter<'a>,
}

impl Iterator for SourcesIter<'_> {
    type Item = SignalId;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

/// Iterator over output SignalIds for an effect
pub struct OutputsIter<'a> {
    inner: SignalFilterMapIter<'a>,
}

impl Iterator for OutputsIter<'_> {
    type Item = SignalId;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

/// Global effect arena - stores all effect and computed metadata
static EFFECT_ARENA: RwLock<Slab<EffectMetadata>> = RwLock::new(Slab::new());

// Global set of pending effect IDs for O(k) processing where k is pending effects.
// This avoids iterating the entire arena to find pending effects.
// Uses RwLock for thread-safe access since EFFECT_ARENA is also global.
static PENDING_EFFECTS: LazyLock<RwLock<IndexSet<EffectId, FastHashBuilder>>> =
    LazyLock::new(|| RwLock::new(IndexSet::default()));

// Global map: EffectId -> parent EffectId
// Stores parent-child relationships for effect hierarchy.
// Uses papaya's lock-free HashMap for efficient concurrent reads.
static EFFECT_PARENT: LazyLock<PapayaHashMap<EffectId, EffectId>> =
    LazyLock::new(PapayaHashMap::new);

// Global map: EffectId -> children Vec<EffectId>
// Stores child effects for each parent effect.
// Uses papaya's lock-free HashMap for efficient concurrent reads.
// The Vec is wrapped in RwLock for safe mutation.
static EFFECT_CHILDREN: LazyLock<PapayaHashMap<EffectId, RwLock<Vec<EffectId>>>> =
    LazyLock::new(PapayaHashMap::new);

// Thread-local current effect being executed.
// Used to establish parent/child relationships when effects create other effects.
thread_local! {
    static CURRENT_EFFECT: RefCell<Option<EffectId>> = const { RefCell::new(None) };
}

/// Get the currently executing effect (if any)
pub fn current_effect() -> Option<EffectId> {
    CURRENT_EFFECT.with(|c| *c.borrow())
}

/// Set the currently executing effect
pub fn set_current_effect(effect_id: Option<EffectId>) -> Option<EffectId> {
    CURRENT_EFFECT.with(|c| c.replace(effect_id))
}

/// RAII guard that restores CURRENT_EFFECT when dropped.
/// This ensures CURRENT_EFFECT is always restored even if callback panics.
pub struct CurrentEffectGuard {
    previous: Option<EffectId>,
}

impl CurrentEffectGuard {
    /// Create a guard that will restore the given previous value when dropped.
    /// Sets CURRENT_EFFECT to `new_value` immediately.
    pub fn new(new_value: Option<EffectId>) -> Self {
        let previous = set_current_effect(new_value);
        Self { previous }
    }
}

impl Drop for CurrentEffectGuard {
    fn drop(&mut self) {
        set_current_effect(self.previous);
    }
}

/// Unique identifier for an effect/computed node in the arena.
///
/// This is a zero-cost wrapper around a slab index.
/// When an Effect or Computed is dropped, it removes itself from the arena,
/// making this EffectId stale. Accessing a stale EffectId returns None.
#[repr(transparent)]
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct EffectId(u32);

impl EffectId {
    /// Create a new EffectId from a raw index
    pub fn new(index: u32) -> Self {
        Self(index)
    }

    /// Convert to usize for slab indexing
    pub fn index(self) -> usize {
        self.0 as usize
    }

    /// Access the effect metadata with a closure (read-only)
    ///
    /// Returns None if the effect has been removed (stale access).
    pub fn with<F, R>(self, f: F) -> Option<R>
    where
        F: FnOnce(&EffectMetadata) -> R,
    {
        let arena = EFFECT_ARENA.read();
        arena.get(self.index()).map(f)
    }

    /// Execute a closure with an iterator over sources
    pub fn with_sources<F, R>(self, f: F) -> Option<R>
    where
        F: FnOnce(SourcesIter<'_>) -> R,
    {
        self.with(|metadata| metadata.with_sources(f))
    }

    /// Add a source to this effect's dependency list (no duplicates via HashSet)
    pub fn add_source(self, source: SignalId) -> Option<()> {
        self.with(|metadata| {
            metadata.add_source(source);
        })
    }

    /// Clear all sources of this effect
    pub fn clear_sources(self) -> Option<()> {
        self.with(|metadata| {
            metadata.clear_sources();
        })
    }

    /// Remove a specific source from this effect's dependency list
    ///
    /// Called when a signal is dropped to clean up its subscriptions
    pub fn remove_source(self, source: SignalId) -> Option<()> {
        self.with(|metadata| {
            metadata.remove_source(source);
        })
    }

    /// Check if a signal is a source (dependency) of this effect
    ///
    /// O(1) lookup using the underlying HashMap. Returns false if the effect
    /// has been removed (stale access).
    pub fn has_source(self, signal_id: SignalId) -> bool {
        self.with(|metadata| metadata.has_source(signal_id))
            .unwrap_or(false)
    }

    // =========================================================================
    // Three-state reactive system methods
    // =========================================================================

    /// Get the current reactive state of this effect
    ///
    /// Returns Clean if the effect has been removed (stale access).
    pub fn state(self) -> ReactiveState {
        self.with(EffectMetadata::get_state)
            .unwrap_or(ReactiveState::Clean)
    }

    /// Set the reactive state
    pub fn set_state(self, state: ReactiveState) {
        self.with(|metadata| metadata.set_state(state));
    }

    /// Upgrade state only (Clean->Check->Dirty, never downgrade)
    ///
    /// Returns true if the state was actually upgraded.
    pub fn upgrade_state(self, new_state: ReactiveState) -> bool {
        self.with(|metadata| metadata.upgrade_state(new_state))
            .unwrap_or(false)
    }

    /// Check if this effect needs any work (state != Clean)
    pub fn needs_work(self) -> bool {
        self.state() != ReactiveState::Clean
    }

    /// Run the callback stored in the arena for this effect.
    ///
    /// This is the core method for executing effects. The callback is stored
    /// directly in the arena's EffectMetadata, eliminating the need for
    /// external registries like EFFECT_REGISTRY.
    ///
    /// IMPORTANT: We release the arena read lock before running the callback
    /// because the callback might create child effects, which requires acquiring
    /// a write lock on EFFECT_ARENA.
    ///
    /// We temporarily take the callback out of the arena, run it, then put it back.
    /// A drop guard ensures the callback is restored even if it panics.
    pub fn run_callback(self) {
        /// Guard that restores a callback to the arena on drop (even on panic)
        struct CallbackGuard {
            effect_id: EffectId,
            callback: Option<Box<dyn FnMut() + Send>>,
        }

        impl CallbackGuard {
            fn run(&mut self) {
                if let Some(ref mut cb) = self.callback {
                    cb();
                }
            }
        }

        impl Drop for CallbackGuard {
            fn drop(&mut self) {
                if let Some(cb) = self.callback.take() {
                    let arena = EFFECT_ARENA.read();
                    if let Some(meta) = arena.get(self.effect_id.index()) {
                        *meta.callback.lock() = Some(cb);
                    }
                }
            }
        }

        // Take the callback out of the arena
        let callback = {
            let arena = EFFECT_ARENA.read();
            if let Some(meta) = arena.get(self.index()) {
                meta.callback.lock().take()
            } else {
                None
            }
        };
        // Arena lock released - child effects can be created

        if let Some(cb) = callback {
            let mut guard = CallbackGuard {
                effect_id: self,
                callback: Some(cb),
            };
            guard.run();
            // Guard drops here, restoring callback to arena
        }
    }

    /// Check if this effect has a callback (effects have callbacks, computeds don't)
    pub fn has_callback(self) -> bool {
        self.with(EffectMetadata::has_callback).unwrap_or(false)
    }

    /// Get the parent effect ID from the global EFFECT_PARENT map
    pub fn parent(self) -> Option<EffectId> {
        let guard = EFFECT_PARENT.pin();
        guard.get(&self).copied()
    }

    /// Execute a closure with the children of this effect
    ///
    /// This avoids allocation by passing a reference to the Vec directly.
    /// Returns None if this effect has no children registered.
    pub fn with_children<F, R>(self, f: F) -> Option<R>
    where
        F: FnOnce(&Vec<EffectId>) -> R,
    {
        let guard = EFFECT_CHILDREN.pin();
        guard.get(&self).map(|children| {
            let read_guard = children.read();
            f(&read_guard)
        })
    }

    /// Add a child to this effect in the global EFFECT_CHILDREN map
    pub fn add_child(self, child: EffectId) {
        let guard = EFFECT_CHILDREN.pin();
        guard
            .get_or_insert_with(self, || RwLock::new(Vec::new()))
            .write()
            .push(child);
    }

    /// Remove a child from this effect in the global EFFECT_CHILDREN map
    pub fn remove_child(self, child: EffectId) {
        let guard = EFFECT_CHILDREN.pin();
        if let Some(children) = guard.get(&self) {
            children.write().retain(|c| *c != child);
        }
    }

    /// Clear all children (does not destroy them) from the global EFFECT_CHILDREN map
    pub fn clear_children(self) {
        let guard = EFFECT_CHILDREN.pin();
        if let Some(children) = guard.get(&self) {
            children.write().clear();
        }
    }

    // =========================================================================
    // Output tracking methods (for push-pull system)
    // =========================================================================

    /// Execute a closure with an iterator over outputs
    pub fn with_outputs<F, R>(self, f: F) -> Option<R>
    where
        F: FnOnce(OutputsIter<'_>) -> R,
    {
        self.with(|metadata| metadata.with_outputs(f))
    }

    /// Add a signal to this effect's output list
    ///
    /// Called when an effect writes to a signal via emit().
    /// Uses HashSet so duplicates are automatically ignored.
    pub fn add_output(self, signal_id: SignalId) -> Option<()> {
        self.with(|metadata| {
            metadata.add_output(signal_id);
        })
    }

    /// Clear all sources and outputs, calling callbacks for each
    ///
    /// Used when an effect re-runs to unsubscribe from old sources
    /// and unregister from old outputs in a single pass.
    pub fn clear_signals<F1, F2>(self, on_source: F1, on_output: F2)
    where
        F1: FnMut(SignalId),
        F2: FnMut(SignalId),
    {
        self.with(|metadata| metadata.clear_signals(on_source, on_output));
    }

    /// Check if this effect is skippable (can be deferred under load)
    pub fn is_skippable(self) -> bool {
        self.with(EffectMetadata::is_skippable).unwrap_or(false)
    }

    /// Check if this effect only wants UI updates (skips internal state updates)
    pub fn is_ui_updates_only(self) -> bool {
        self.with(EffectMetadata::is_ui_updates_only)
            .unwrap_or(false)
    }

    // =========================================================================
    // Three-state recursive marking methods
    // =========================================================================

    /// Recursively mark this effect and all downstream effects as Check.
    ///
    /// This is the "push" part of the Leptos-style push-pull reactive system.
    /// When a signal changes:
    /// 1. Direct subscribers are marked Dirty (they definitely need to recompute)
    /// 2. Their downstream dependents are marked Check (they might need to recompute)
    ///
    /// The Check state propagates recursively through the dependency graph, but
    /// we don't actually recompute anything yet - that happens lazily during pull().
    pub fn mark_check_recursive(self) {
        // Only upgrade if currently Clean
        let current = self.state();
        if current == ReactiveState::Clean {
            cov_mark::hit!(marking_effect_check_recursive);
            self.set_state(ReactiveState::Check);

            // Add to pending effects set for eventual processing
            mark_effect_pending_for_check(self);

            // Recursively mark dependents of signals this effect writes
            self.with_outputs(|outputs| {
                for output_signal in outputs {
                    output_signal.mark_subscribers_check();
                }
            });
        }
        // If already Check or Dirty, no need to propagate further
    }

    /// Mark this effect as Dirty (definitely needs recompute).
    ///
    /// This also destroys children and adds to the pending set.
    pub fn mark_dirty(self) {
        mark_effect_dirty(self);
    }

    /// Run this effect if necessary based on state.
    ///
    /// This is the "pull" verification for the three-state system.
    /// Returns true if the effect actually ran (value may have changed).
    ///
    /// # Skippable Effects
    /// If `skip_skippable` is true, skippable effects are skipped when pulling sources.
    /// This allows consumers to avoid running producer effects when over budget.
    pub fn update_if_necessary(self, skip_skippable: bool) -> bool {
        match self.state() {
            ReactiveState::Clean => false,

            ReactiveState::Check => {
                // Verify by pulling parents (sources)
                // Each effect has its own signals RwLock, so no deadlock concern
                self.with_sources(|sources| {
                    for source_id in sources {
                        // Pull the source (might run writer effects)
                        // Pass skip_skippable through to avoid running skippable producers
                        source_id.pull(skip_skippable);

                        // If pulling made us Dirty, stop checking
                        if self.state() == ReactiveState::Dirty {
                            cov_mark::hit!(check_upgraded_to_dirty_by_pull);
                            break;
                        }
                    }
                });

                // If still Check after verifying all parents, we're Clean
                if self.state() == ReactiveState::Check {
                    cov_mark::hit!(check_verified_clean);
                    self.set_state(ReactiveState::Clean);
                    return false;
                }

                // Fall through to run (we're now Dirty)
                cov_mark::hit!(check_became_dirty_running);
                self.run_and_mark_children_if_changed()
            }

            ReactiveState::Dirty => {
                cov_mark::hit!(dirty_running);
                self.run_and_mark_children_if_changed()
            }
        }
    }

    /// Run this effect and mark children Dirty if output changed.
    ///
    /// Returns true (effects don't have comparable values like computeds).
    fn run_and_mark_children_if_changed(self) -> bool {
        // Run the effect
        run_single_effect(self);

        // Mark direct children of our outputs as Dirty (value changed)
        // Note: run_single_effect clears and re-establishes outputs, so
        // we need to get the new outputs after running
        self.with_outputs(|outputs| {
            for output in outputs {
                output.with_subscribers(|subs| {
                    for effect_id in subs {
                        if effect_id.state() == ReactiveState::Check {
                            cov_mark::hit!(child_upgraded_check_to_dirty);
                        }
                        effect_id.upgrade_state(ReactiveState::Dirty);
                    }
                });
            }
        });

        true
    }
}

/// Unified metadata for both effects and computeds stored in the arena.
///
/// This struct replaces the previous enum (EffectMetadata::Effect/Computed) since
/// both types have similar structure:
/// - signals: signals related to this effect (both sources and outputs)
/// - state: three-state reactive state (Clean/Check/Dirty) for Leptos-style propagation
/// - callback: optional effect function (only for effects, not computeds)
/// - flags: bitset for local/skippable flags
///
/// Parent/child relationships are stored in separate global maps (EFFECT_PARENT/EFFECT_CHILDREN)
/// to keep EffectMetadata smaller and improve cache locality.
///
/// Using a single struct reduces code duplication and simplifies the API.
/// The signals field uses HashMap to track both sources (reads) and outputs (writes),
/// reducing memory overhead when a signal is both read and written.
///
/// The callback is stored directly in the arena, making Effect a thin wrapper
/// around EffectId. This eliminates the need for external registries like
/// EFFECT_REGISTRY.
pub struct EffectMetadata {
    /// Three-state reactive state for Leptos-style propagation:
    /// - Clean (0): Value is current, use cached
    /// - Check (1): Might be stale, verify parents first
    /// - Dirty (2): Definitely stale, must recompute
    pub(crate) state: AtomicU8,

    /// Flags bitset:
    /// - bit 0: skippable (can be deferred under load)
    ///   Note: The 'local' flag (for Python/GIL) is mentioned in comments but not currently used
    pub(crate) flags: u8,

    /// The effect callback function (None for computeds, Some for effects).
    /// Using Mutex for thread-safety with dyn FnMut.
    /// The callback must be Send to be stored in a global static.
    pub(crate) callback: Mutex<Option<Box<dyn FnMut() + Send>>>,

    /// Signals related to this effect - maps SignalId to whether it's a source/output.
    /// This unified approach reduces memory overhead when signals are both read and written.
    /// FastHashBuilder provides zero-sized hasher for memory efficiency.
    pub(crate) signals: RwLock<HashMap<SignalId, SignalRelation, FastHashBuilder>>,
}

// Flag bit positions
const FLAG_SKIPPABLE: u8 = 1 << 0;
const FLAG_UI_UPDATES_ONLY: u8 = 1 << 1;

impl EffectMetadata {
    /// Create new effect metadata with default values (state = Clean, no callback)
    ///
    /// Use this for Computeds which don't have a callback stored in the arena.
    /// (Computed keeps its own function since it returns a value)
    pub fn new() -> Self {
        Self {
            state: AtomicU8::new(ReactiveState::Clean as u8),
            flags: 0,
            callback: Mutex::new(None),
            signals: RwLock::new(HashMap::with_hasher(FastHashBuilder)),
        }
    }

    /// Create new effect metadata with a callback (state = Clean)
    ///
    /// Use this for Effects which store their callback directly in the arena.
    pub fn new_with_callback(callback: Box<dyn FnMut() + Send>) -> Self {
        Self {
            state: AtomicU8::new(ReactiveState::Clean as u8),
            flags: 0,
            callback: Mutex::new(Some(callback)),
            signals: RwLock::new(HashMap::with_hasher(FastHashBuilder)),
        }
    }

    /// Create new effect metadata with a parent (for hierarchical effects)
    ///
    /// Use this for Effects created inside another effect's callback.
    /// Parent is stored in the global EFFECT_PARENT map, not in the metadata.
    pub fn new_with_callback_and_parent(
        callback: Box<dyn FnMut() + Send>,
        _parent: Option<EffectId>,
    ) -> Self {
        // Parent is stored in EFFECT_PARENT map, not in metadata
        Self {
            state: AtomicU8::new(ReactiveState::Clean as u8),
            flags: 0,
            callback: Mutex::new(Some(callback)),
            signals: RwLock::new(HashMap::with_hasher(FastHashBuilder)),
        }
    }

    /// Create new effect metadata with state = Dirty
    ///
    /// Use this for Computeds which start dirty so first access computes.
    pub fn new_dirty() -> Self {
        Self {
            state: AtomicU8::new(ReactiveState::Dirty as u8),
            flags: 0,
            callback: Mutex::new(None),
            signals: RwLock::new(HashMap::with_hasher(FastHashBuilder)),
        }
    }

    /// Create new effect metadata with callback, parent, and flags
    ///
    /// Use this for Effects that can be deferred under load (producers).
    /// Parent is stored in the global EFFECT_PARENT map, not in the metadata.
    ///
    /// Flags:
    /// - `skippable`: Effect can be deferred under load
    /// - `ui_updates_only`: Effect only wants UI updates (skipped on data-only updates)
    pub fn new_with_callback_parent_and_flags(
        callback: Box<dyn FnMut() + Send>,
        _parent: Option<EffectId>,
        skippable: bool,
        ui_updates_only: bool,
    ) -> Self {
        // Parent is stored in EFFECT_PARENT map, not in metadata
        let mut flags = 0;
        if skippable {
            flags |= FLAG_SKIPPABLE;
        }
        if ui_updates_only {
            flags |= FLAG_UI_UPDATES_ONLY;
        }
        Self {
            state: AtomicU8::new(ReactiveState::Clean as u8),
            flags,
            callback: Mutex::new(Some(callback)),
            signals: RwLock::new(HashMap::with_hasher(FastHashBuilder)),
        }
    }

    /// Create new effect metadata with callback, parent, and skippable flag
    ///
    /// Use this for Effects that can be deferred under load (producers).
    /// Parent is stored in the global EFFECT_PARENT map, not in the metadata.
    pub fn new_with_callback_parent_and_skippable(
        callback: Box<dyn FnMut() + Send>,
        parent: Option<EffectId>,
        skippable: bool,
    ) -> Self {
        Self::new_with_callback_parent_and_flags(callback, parent, skippable, false)
    }

    // =========================================================================
    // Three-state reactive system methods
    // =========================================================================

    /// Get the current reactive state
    pub fn get_state(&self) -> ReactiveState {
        ReactiveState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// Get whether this effect is skippable
    pub fn is_skippable(&self) -> bool {
        (self.flags & FLAG_SKIPPABLE) != 0
    }

    /// Get whether this effect only wants UI updates (skips internal state updates)
    pub fn is_ui_updates_only(&self) -> bool {
        (self.flags & FLAG_UI_UPDATES_ONLY) != 0
    }

    /// Set the reactive state
    pub fn set_state(&self, state: ReactiveState) {
        self.state.store(state as u8, Ordering::Release);
    }

    /// Set the reactive state
    pub fn replace_state(&self, state: ReactiveState) -> ReactiveState {
        ReactiveState::from_u8(self.state.swap(state as u8, Ordering::Release))
    }

    /// Upgrade state only (Clean->Check->Dirty, never downgrade)
    ///
    /// Returns true if the state was actually upgraded.
    ///
    /// Uses compare_exchange loop for thread-safety (avoids TOCTOU race that could
    /// cause an accidental downgrade if two threads race to upgrade the same state).
    pub fn upgrade_state(&self, new_state: ReactiveState) -> bool {
        let new_state_u8 = new_state as u8;
        loop {
            let current = self.state.load(Ordering::Acquire);
            if current >= new_state_u8 {
                // Already at or above the target state, no upgrade needed
                return false;
            }
            // Try to atomically upgrade from current to new_state.
            // Loop is guaranteed to terminate: state can only increase (never downgrade),
            // so after at most 2 iterations (Clean->Check->Dirty), we'll succeed or give up.
            match self.state.compare_exchange_weak(
                current,
                new_state_u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(_) => continue, // State changed, retry
            }
        }
    }

    /// Check if this effect needs any work (state != Clean)
    pub fn needs_work(&self) -> bool {
        self.get_state() != ReactiveState::Clean
    }

    /// Add a source to this node's dependency list
    ///
    /// Uses HashMap so duplicates are automatically ignored.
    pub fn add_source(&self, source_id: SignalId) {
        let mut signals = self.signals.write();
        signals.entry(source_id).or_default().is_source = true;
    }

    /// Remove a source from this node's dependency list
    pub fn remove_source(&self, source_id: SignalId) {
        let mut signals = self.signals.write();
        if let Some(relation) = signals.get_mut(&source_id) {
            relation.is_source = false;
            // Remove entry if both flags are false
            if !relation.is_source && !relation.is_output {
                signals.remove(&source_id);
            }
        }
    }

    /// Check if a signal is a source (dependency) of this effect
    ///
    /// O(1) lookup using the underlying HashMap.
    pub fn has_source(&self, signal_id: SignalId) -> bool {
        self.signals
            .read()
            .get(&signal_id)
            .is_some_and(|r| r.is_source)
    }

    /// Execute a closure with an iterator over sources
    pub fn with_sources<F, R>(&self, f: F) -> R
    where
        F: FnOnce(SourcesIter<'_>) -> R,
    {
        fn filter_sources((id, relation): (&SignalId, &SignalRelation)) -> Option<SignalId> {
            if relation.is_source { Some(*id) } else { None }
        }
        let signals = self.signals.read();
        let iter = SourcesIter {
            inner: signals.iter().filter_map(filter_sources as fn(_) -> _),
        };
        f(iter)
    }

    /// Clear all sources
    pub fn clear_sources(&self) {
        let mut signals = self.signals.write();
        // Set all is_source to false, remove entries where both flags are false
        signals.retain(|_, relation| {
            relation.is_source = false;
            relation.is_output // Keep if is_output is still true
        });
    }

    /// Run the callback if present (for effects)
    ///
    /// This is safe to call for computeds (they have no callback, so nothing happens).
    pub fn run_callback(&self) {
        let mut cb = self.callback.lock();
        if let Some(ref mut f) = *cb {
            (*f)();
        }
    }

    /// Check if this metadata has a callback (effects have callbacks, computeds don't)
    pub fn has_callback(&self) -> bool {
        self.callback.lock().is_some()
    }

    // =========================================================================
    // Output methods (for push-pull system)
    // =========================================================================

    /// Add an output signal to this effect's output list
    ///
    /// Called when an effect writes to a signal via emit().
    /// Uses HashMap so duplicates are automatically ignored.
    pub fn add_output(&self, signal_id: SignalId) {
        let mut signals = self.signals.write();
        signals.entry(signal_id).or_default().is_output = true;
    }

    /// Execute a closure with an iterator over outputs
    pub fn with_outputs<F, R>(&self, f: F) -> R
    where
        F: FnOnce(OutputsIter<'_>) -> R,
    {
        fn filter_outputs((id, relation): (&SignalId, &SignalRelation)) -> Option<SignalId> {
            if relation.is_output { Some(*id) } else { None }
        }
        let signals = self.signals.read();
        let iter = OutputsIter {
            inner: signals.iter().filter_map(filter_outputs as fn(_) -> _),
        };
        f(iter)
    }

    /// Clear all sources and outputs, calling callbacks for each
    ///
    /// This is used when an effect re-runs to unsubscribe from old sources
    /// and unregister from old outputs in a single pass.
    pub fn clear_signals<F1, F2>(&self, mut on_source: F1, mut on_output: F2)
    where
        F1: FnMut(SignalId),
        F2: FnMut(SignalId),
    {
        let mut signals = self.signals.write();
        for (id, relation) in signals.drain() {
            if relation.is_source {
                on_source(id);
            }
            if relation.is_output {
                on_output(id);
            }
        }
    }
}

impl Default for EffectMetadata {
    fn default() -> Self {
        Self::new()
    }
}

// Arena manipulation functions

/// Insert effect metadata into the arena and return its ID
pub fn effect_arena_insert(metadata: EffectMetadata) -> EffectId {
    let mut arena = EFFECT_ARENA.write();
    let entry = arena.vacant_entry();
    let key = entry.key();
    entry.insert(metadata);
    EffectId::new(key as u32)
}

/// Set the parent of an effect in the global EFFECT_PARENT map
pub fn set_effect_parent(effect_id: EffectId, parent_id: EffectId) {
    let guard = EFFECT_PARENT.pin();
    guard.insert(effect_id, parent_id);
}

/// Remove an effect from the arena and clean up parent/children relationships
pub fn effect_arena_remove(id: EffectId) -> Option<EffectMetadata> {
    // Remove from parent map
    {
        let guard = EFFECT_PARENT.pin();
        guard.remove(&id);
    }

    // Remove from children map
    {
        let guard = EFFECT_CHILDREN.pin();
        guard.remove(&id);
    }

    // Remove from arena
    let mut arena = EFFECT_ARENA.write();
    if arena.contains(id.index()) {
        Some(arena.remove(id.index()))
    } else {
        None
    }
}

/// Mark an effect as pending and add it to the pending set.
///
/// This is the efficient way to mark effects as pending - it sets the flag
/// AND adds to the pending set for O(k) processing.
///
/// When an effect is marked pending, its children are immediately destroyed
/// because they are now invalid (parent state has changed). This prevents
/// stale child effects from hanging around between invalidation and execution.
///
/// Returns true if the effect was newly marked pending (wasn't already pending).
pub fn mark_effect_pending(effect_id: EffectId) -> bool {
    let was_not_dirty = effect_id
        .with(|metadata| metadata.replace_state(ReactiveState::Dirty) != ReactiveState::Dirty)
        .unwrap_or(false);

    if was_not_dirty {
        // Destroy all children immediately when parent is marked dirty.
        // Children are invalid once the parent needs to re-run.
        destroy_children(effect_id);

        PENDING_EFFECTS.write().insert(effect_id);
    }

    was_not_dirty
}

/// Destroy all child effects of the given effect recursively.
///
/// Called when an effect is marked pending to immediately clean up stale children.
/// This prevents child effects from hanging around between invalidation and execution.
pub fn destroy_children(effect_id: EffectId) {
    // Take ownership of children by draining, then remove the empty entry
    let children: Vec<EffectId> = {
        let guard = EFFECT_CHILDREN.pin();
        if let Some(rw) = guard.get(&effect_id) {
            let children = std::mem::take(&mut *rw.write());
            guard.remove(&effect_id);
            children
        } else {
            return;
        }
    };

    // Recursively destroy each child
    for child_id in children {
        // Recursively destroy grandchildren first
        destroy_children(child_id);

        // Remove from pending set
        remove_from_pending_set(child_id);

        // Unsubscribe from all sources
        child_id.with_sources(|sources| {
            for source_id in sources {
                source_id.remove_subscriber(child_id);
            }
        });

        // Remove from arena
        effect_arena_remove(child_id);
    }
}

/// Take all pending effect IDs from the pending set.
///
/// This atomically drains the pending set and returns all effect IDs.
/// Used by Effect::process_all() for efficient O(k) processing.
///
/// This implementation uses drain(..) instead of take() to preserve the
/// IndexSet's allocation, reducing allocations across effect flushes.
pub fn take_pending_effects() -> Vec<EffectId> {
    PENDING_EFFECTS.write().drain(..).collect()
}

pub fn take_pending_effects_split(must_run: &mut Vec<EffectId>, skippable: &mut Vec<EffectId>) {
    let mut pending = PENDING_EFFECTS.write();
    for effect in pending.drain(..) {
        if effect.is_skippable() {
            skippable.push(effect);
        } else {
            must_run.push(effect);
        }
    }
}

/// Remove an effect from the pending set (used when effect is destroyed).
pub fn remove_from_pending_set(effect_id: EffectId) {
    PENDING_EFFECTS.write().swap_remove(&effect_id);
}

// ============================================================================
// Three-state reactive system functions
// ============================================================================

/// Add a Check-state effect to the pending set for eventual processing.
///
/// This is used for indirect dependents in the three-state system. When a signal
/// changes, direct subscribers are marked Dirty, but their downstream dependents
/// are only marked Check (they might not actually need to recompute).
///
/// Note: The caller (mark_check_recursive) is responsible for:
/// 1. Verifying the effect was previously Clean
/// 2. Setting the state to Check before calling this function
pub fn mark_effect_pending_for_check(effect_id: EffectId) {
    cov_mark::hit!(check_effect_added_to_pending);
    // The caller has already verified this was Clean and set it to Check.
    // We just need to add to the pending set.
    PENDING_EFFECTS.write().insert(effect_id);
}

/// Mark an effect as Dirty and add to pending set (convenience wrapper).
///
/// This sets the state to Dirty (not just pending=true) and adds to the pending set.
/// Also destroys children as they are now invalid.
pub fn mark_effect_dirty(effect_id: EffectId) -> bool {
    let was_needs_work = effect_id.needs_work();

    if was_needs_work {
        // Already needs work - upgrade to Dirty if not already
        effect_id.upgrade_state(ReactiveState::Dirty);
    } else {
        effect_id.set_state(ReactiveState::Dirty);

        // Destroy all children immediately when parent is marked dirty.
        // Children are invalid once the parent needs to re-run.
        destroy_children(effect_id);

        PENDING_EFFECTS.write().insert(effect_id);
    }

    !was_needs_work
}

// Import run_single_effect for internal use in EffectId::run_and_mark_children_if_changed
use crate::effect::run_single_effect;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_access_returns_none() {
        let metadata = EffectMetadata::new();
        let id = effect_arena_insert(metadata);

        effect_arena_remove(id);

        // All operations should return None for stale ID or defaults
        assert!(id.with_sources(|_| ()).is_none());
        assert_eq!(id.state(), ReactiveState::Clean); // Returns default for stale access
        assert_eq!(id.add_source(SignalId::new(1)), None);
    }

    #[test]
    fn effect_callback_restored_on_panic() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicI32, Ordering};

        let run_count = Arc::new(AtomicI32::new(0));
        let run_count_clone = run_count.clone();

        let callback = Box::new(move || {
            let count = run_count_clone.fetch_add(1, Ordering::Relaxed);
            if count == 0 {
                // First call panics
                panic!("test panic in callback");
            }
            // Second call succeeds
        });

        let metadata = EffectMetadata::new_with_callback(callback);
        let id = effect_arena_insert(metadata);

        assert!(id.has_callback());
        assert_eq!(run_count.load(Ordering::Relaxed), 0);

        // First run should panic, but callback should be restored
        let result = std::panic::catch_unwind(|| {
            id.run_callback();
        });
        assert!(result.is_err());
        assert_eq!(run_count.load(Ordering::Relaxed), 1);

        // Verify callback is still present after panic
        assert!(id.has_callback());

        // Second run should succeed (callback was restored)
        id.run_callback();
        assert_eq!(run_count.load(Ordering::Relaxed), 2);

        // Cleanup
        effect_arena_remove(id);
    }

    #[test]
    fn current_effect_guard_restores_on_panic() {
        // Test that guard restores even when panic occurs
        let effect1 = EffectId::new(10);
        let effect2 = EffectId::new(20);

        // Set initial effect
        set_current_effect(Some(effect1));
        assert_eq!(current_effect(), Some(effect1));

        // Guard should restore even if panic occurs
        let result = std::panic::catch_unwind(|| {
            let _guard = CurrentEffectGuard::new(Some(effect2));
            assert_eq!(current_effect(), Some(effect2));
            panic!("test panic");
        });

        assert!(result.is_err());
        // After panic, guard should have restored to effect1
        assert_eq!(current_effect(), Some(effect1));

        // Clean up
        set_current_effect(None);
    }
}
