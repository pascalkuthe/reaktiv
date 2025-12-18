// Effect arena - storage for effect and computed metadata
//
// This module defines EffectMetadata, a unified struct for both effects and
// computeds, and provides helper functions for working with the effect arena.
//
// The EffectMetadata struct contains:
// - sources: the signals this effect/computed depends on (stored as IndexSet)
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
use parking_lot::{Mutex, RwLock};
use slab::Slab;
use std::cell::RefCell;
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

/// Global effect arena - stores all effect and computed metadata
static EFFECT_ARENA: RwLock<Slab<EffectMetadata>> = RwLock::new(Slab::new());

// Global set of pending effect IDs for O(k) processing where k is pending effects.
// This avoids iterating the entire arena to find pending effects.
// Uses RwLock for thread-safe access since EFFECT_ARENA is also global.
static PENDING_EFFECTS: LazyLock<RwLock<IndexSet<EffectId, FastHashBuilder>>> =
    LazyLock::new(|| RwLock::new(IndexSet::default()));

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

    /// Get the sources of this effect
    pub fn sources(self) -> Option<Vec<SignalId>> {
        self.with(EffectMetadata::get_sources)
    }

    /// Add a source to this effect's dependency list (no duplicates via IndexSet)
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

    /// Get the parent effect ID
    pub fn parent(self) -> Option<EffectId> {
        self.with(EffectMetadata::get_parent).flatten()
    }

    /// Get all child effect IDs
    pub fn children(self) -> Option<Vec<EffectId>> {
        self.with(EffectMetadata::get_children)
    }

    /// Add a child to this effect
    pub fn add_child(self, child: EffectId) {
        self.with(|metadata| metadata.add_child(child));
    }

    /// Remove a child from this effect
    pub fn remove_child(self, child: EffectId) {
        self.with(|metadata| metadata.remove_child(child));
    }

    /// Clear all children (does not destroy them)
    pub fn clear_children(self) {
        self.with(EffectMetadata::clear_children);
    }

    /// Check if this effect is local (must run on main thread)
    ///
    /// Local effects are used for Python callbacks that need the GIL.
    /// Returns true if the effect is local, or true (conservative default) if effect doesn't exist.
    pub fn is_local(self) -> bool {
        self.with(EffectMetadata::is_local).unwrap_or(true)
    }

    // =========================================================================
    // Output tracking methods (for push-pull system)
    // =========================================================================

    /// Get the signals this effect writes to (outputs)
    ///
    /// Returns None if the effect has been removed (stale access).
    pub fn outputs(self) -> Option<Vec<SignalId>> {
        self.with(EffectMetadata::get_outputs)
    }

    /// Add a signal to this effect's output list
    ///
    /// Called when an effect writes to a signal via emit().
    /// Uses IndexSet so duplicates are automatically ignored.
    pub fn add_output(self, signal_id: SignalId) -> Option<()> {
        self.with(|metadata| {
            metadata.add_output(signal_id);
        })
    }

    /// Clear all outputs of this effect
    ///
    /// Called before re-running an effect to clear old output tracking.
    pub fn clear_outputs(self) -> Option<()> {
        self.with(|metadata| {
            metadata.clear_outputs();
        })
    }

    /// Take all outputs from this effect (returns them and clears the list)
    ///
    /// This is useful for atomically getting and clearing the outputs,
    /// typically done before re-running an effect.
    pub fn take_outputs(self) -> Option<Vec<SignalId>> {
        self.with(EffectMetadata::take_outputs)
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
            if let Some(outputs) = self.outputs() {
                for output_signal in outputs {
                    output_signal.mark_subscribers_check();
                }
            }
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
    pub fn update_if_necessary(self) -> bool {
        match self.state() {
            ReactiveState::Clean => false,

            ReactiveState::Check => {
                // Verify by pulling parents (sources)
                if let Some(sources) = self.sources() {
                    for source_id in sources {
                        // Pull the source (might run writer effects)
                        source_id.pull();

                        // If pulling made us Dirty, stop checking
                        if self.state() == ReactiveState::Dirty {
                            cov_mark::hit!(check_upgraded_to_dirty_by_pull);
                            break;
                        }
                    }
                }

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
        if let Some(outputs) = self.outputs() {
            for output in outputs {
                if let Some(subs) = output.subscribers() {
                    for sub in subs {
                        if sub.effect_id.state() == ReactiveState::Check {
                            cov_mark::hit!(child_upgraded_check_to_dirty);
                        }
                        sub.effect_id.upgrade_state(ReactiveState::Dirty);
                    }
                }
            }
        }

        true
    }
}

/// Unified metadata for both effects and computeds stored in the arena.
///
/// This struct replaces the previous enum (EffectMetadata::Effect/Computed) since
/// both types have similar structure:
/// - sources: signals this node depends on (inputs)
/// - outputs: signals this effect writes to (push-pull system)
/// - state: three-state reactive state (Clean/Check/Dirty) for Leptos-style propagation
/// - callback: optional effect function (only for effects, not computeds)
/// - parent/children: hierarchy for proper cleanup when parent re-runs
/// - local: whether this effect must run on the main thread (for Python/GIL)
///
/// Using a single struct reduces code duplication and simplifies the API.
/// The sources and outputs fields use IndexSet for O(1) lookup and automatic deduplication.
///
/// The callback is stored directly in the arena, making Effect a thin wrapper
/// around EffectId. This eliminates the need for external registries like
/// EFFECT_REGISTRY.
pub struct EffectMetadata {
    /// Child effects created during this effect's callback execution.
    /// When this effect re-runs, all children are destroyed first.
    pub(crate) children: RwLock<Vec<EffectId>>,

    /// Signals this effect/computed depends on (inputs).
    /// When any of these change, the node is invalidated.
    /// Uses IndexSet for O(1) lookup and automatic deduplication.
    /// FastHashBuilder provides zero-sized hasher for memory efficiency.
    pub(crate) sources: RwLock<IndexSet<SignalId, FastHashBuilder>>,

    /// Signals this effect writes to (outputs for push-pull system).
    /// When this effect runs and calls emit() on a signal, that signal
    /// is tracked as an output. This allows pull() to know which effects
    /// produce values for which signals.
    pub(crate) outputs: RwLock<IndexSet<SignalId, FastHashBuilder>>,

    /// The effect callback function (None for computeds, Some for effects).
    /// Using Mutex for thread-safety with dyn FnMut.
    /// The callback must be Send to be stored in a global static.
    pub(crate) callback: Mutex<Option<Box<dyn FnMut() + Send>>>,

    /// Parent effect (if this effect was created inside another effect's callback)
    /// When the parent re-runs, this child effect will be destroyed.
    pub(crate) parent: Option<EffectId>,

    /// Three-state reactive state for Leptos-style propagation:
    /// - Clean (0): Value is current, use cached
    /// - Check (1): Might be stale, verify parents first
    /// - Dirty (2): Definitely stale, must recompute
    pub(crate) state: AtomicU8,

    /// Whether this effect must run on the main thread (local effects).
    /// Local effects are used for Python callbacks that need the GIL.
    /// When true, this effect runs sequentially on the current thread.
    /// When false (default), this effect can run in parallel with others.
    pub(crate) local: bool,
}

impl EffectMetadata {
    /// Create new effect metadata with default values (state = Clean, no callback)
    ///
    /// Use this for Computeds which don't have a callback stored in the arena.
    /// (Computed keeps its own function since it returns a value)
    pub fn new() -> Self {
        Self {
            sources: RwLock::new(IndexSet::default()),
            outputs: RwLock::new(IndexSet::default()),
            state: AtomicU8::new(ReactiveState::Clean as u8),
            callback: Mutex::new(None),
            parent: None,
            children: RwLock::new(Vec::new()),
            local: false,
        }
    }

    /// Create new effect metadata with a callback (state = Clean)
    ///
    /// Use this for Effects which store their callback directly in the arena.
    pub fn new_with_callback(callback: Box<dyn FnMut() + Send>) -> Self {
        Self {
            sources: RwLock::new(IndexSet::default()),
            outputs: RwLock::new(IndexSet::default()),
            state: AtomicU8::new(ReactiveState::Clean as u8),
            callback: Mutex::new(Some(callback)),
            parent: None,
            children: RwLock::new(Vec::new()),
            local: false,
        }
    }

    /// Create new effect metadata with a parent (for hierarchical effects)
    ///
    /// Use this for Effects created inside another effect's callback.
    pub fn new_with_callback_and_parent(
        callback: Box<dyn FnMut() + Send>,
        parent: Option<EffectId>,
    ) -> Self {
        Self {
            sources: RwLock::new(IndexSet::default()),
            outputs: RwLock::new(IndexSet::default()),
            state: AtomicU8::new(ReactiveState::Clean as u8),
            callback: Mutex::new(Some(callback)),
            parent,
            children: RwLock::new(Vec::new()),
            local: false,
        }
    }

    /// Create new effect metadata with a parent and local flag (for hierarchical effects)
    ///
    /// Use this for Effects created inside another effect's callback that need
    /// to specify whether they should run locally (on main thread) or in parallel.
    pub fn new_with_callback_parent_and_local(
        callback: Box<dyn FnMut() + Send>,
        parent: Option<EffectId>,
        local: bool,
    ) -> Self {
        Self {
            sources: RwLock::new(IndexSet::default()),
            outputs: RwLock::new(IndexSet::default()),
            state: AtomicU8::new(ReactiveState::Clean as u8),
            callback: Mutex::new(Some(callback)),
            parent,
            children: RwLock::new(Vec::new()),
            local,
        }
    }

    /// Create new effect metadata with state = Dirty
    ///
    /// Use this for Computeds which start dirty so first access computes.
    pub fn new_dirty() -> Self {
        Self {
            sources: RwLock::new(IndexSet::default()),
            outputs: RwLock::new(IndexSet::default()),
            state: AtomicU8::new(ReactiveState::Dirty as u8),
            callback: Mutex::new(None),
            parent: None,
            children: RwLock::new(Vec::new()),
            local: false,
        }
    }

    // =========================================================================
    // Three-state reactive system methods
    // =========================================================================

    /// Get the current reactive state
    pub fn get_state(&self) -> ReactiveState {
        ReactiveState::from_u8(self.state.load(Ordering::Acquire))
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
    /// Uses IndexSet so duplicates are automatically ignored.
    pub fn add_source(&self, source_id: SignalId) {
        let mut sources = self.sources.write();
        sources.insert(source_id);
    }

    /// Remove a source from this node's dependency list
    pub fn remove_source(&self, source_id: SignalId) {
        let mut sources = self.sources.write();
        sources.swap_remove(&source_id);
    }

    /// Get all sources as a Vec
    pub fn get_sources(&self) -> Vec<SignalId> {
        self.sources.read().iter().copied().collect()
    }

    /// Clear all sources
    pub fn clear_sources(&self) {
        self.sources.write().clear();
    }

    /// Set new sources (replaces existing)
    pub fn set_sources(&self, new_sources: impl IntoIterator<Item = SignalId>) {
        let mut sources = self.sources.write();
        sources.clear();
        sources.extend(new_sources);
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

    /// Get the parent effect ID (if this effect was created inside another effect)
    pub fn get_parent(&self) -> Option<EffectId> {
        self.parent
    }

    /// Set the parent effect ID
    pub fn set_parent(&mut self, parent: Option<EffectId>) {
        self.parent = parent;
    }

    /// Add a child effect
    pub fn add_child(&self, child: EffectId) {
        self.children.write().push(child);
    }

    /// Get all child effect IDs
    pub fn get_children(&self) -> Vec<EffectId> {
        self.children.read().clone()
    }

    /// Clear all children (does not destroy them, just clears the list)
    pub fn clear_children(&self) {
        self.children.write().clear();
    }

    /// Remove a specific child from the children list
    pub fn remove_child(&self, child: EffectId) {
        self.children.write().retain(|c| *c != child);
    }

    /// Check if this effect is local (must run on main thread)
    ///
    /// Local effects are used for Python callbacks that need the GIL.
    /// They run sequentially on the current thread instead of in parallel.
    pub fn is_local(&self) -> bool {
        self.local
    }

    // =========================================================================
    // Output methods (for push-pull system)
    // =========================================================================

    /// Add an output signal to this effect's output list
    ///
    /// Called when an effect writes to a signal via emit().
    /// Uses IndexSet so duplicates are automatically ignored.
    pub fn add_output(&self, signal_id: SignalId) {
        let mut outputs = self.outputs.write();
        outputs.insert(signal_id);
    }

    /// Get all outputs as a Vec
    pub fn get_outputs(&self) -> Vec<SignalId> {
        self.outputs.read().iter().copied().collect()
    }

    /// Clear all outputs
    pub fn clear_outputs(&self) {
        self.outputs.write().clear();
    }

    /// Take all outputs (returns them and clears the list)
    ///
    /// This is an atomic operation to get and clear outputs.
    pub fn take_outputs(&self) -> Vec<SignalId> {
        let mut outputs = self.outputs.write();
        std::mem::take(&mut *outputs).into_iter().collect()
    }
}

impl Default for EffectMetadata {
    fn default() -> Self {
        Self::new()
    }
}

// Type aliases for backwards compatibility and documentation
// These are no longer separate types but the names help document intent

/// Alias for EffectMetadata - documents that this is used for Effect nodes
#[allow(dead_code)]
pub type EffectData = EffectMetadata;

/// Alias for EffectMetadata - documents that this is used for Computed nodes
#[allow(dead_code)]
pub type ComputedData = EffectMetadata;

// Arena manipulation functions

/// Insert effect metadata into the arena and return its ID
pub fn effect_arena_insert(metadata: EffectMetadata) -> EffectId {
    let mut arena = EFFECT_ARENA.write();
    let entry = arena.vacant_entry();
    let key = entry.key();
    entry.insert(metadata);
    EffectId::new(key as u32)
}

/// Remove an effect from the arena
pub fn effect_arena_remove(id: EffectId) -> Option<EffectMetadata> {
    let mut arena = EFFECT_ARENA.write();
    if arena.contains(id.index()) {
        Some(arena.remove(id.index()))
    } else {
        None
    }
}

/// Access the global effect arena directly (for low-level operations)
///
/// Use this for operations that need direct arena access, like iterating
/// or bulk operations. For single-effect operations, prefer EffectId methods.
#[allow(dead_code)]
pub fn with_effect_arena<F, R>(f: F) -> R
where
    F: FnOnce(&RwLock<Slab<EffectMetadata>>) -> R,
{
    f(&EFFECT_ARENA)
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
    // Get children (if any)
    let children = effect_id.children().unwrap_or_default();

    // Clear the children list first
    effect_id.clear_children();

    // Recursively destroy each child
    for child_id in children {
        // Recursively destroy grandchildren first
        destroy_children(child_id);

        // Remove from pending set
        remove_from_pending_set(child_id);

        // Unsubscribe from all sources
        if let Some(sources) = child_id.sources() {
            for source_id in sources {
                source_id.remove_subscriber(child_id);
            }
        }

        // Remove from arena
        effect_arena_remove(child_id);
    }
}

/// Take all pending effect IDs from the pending set.
///
/// This atomically drains the pending set and returns all effect IDs.
/// Used by Effect::process_all() for efficient O(k) processing.
pub fn take_pending_effects() -> Vec<EffectId> {
    let pending = std::mem::take(&mut *PENDING_EFFECTS.write());
    pending.into_iter().collect()
}

/// Get count of pending effects (from the pending set).
///
/// This is O(1) instead of iterating the entire arena.
pub fn pending_effects_count() -> usize {
    PENDING_EFFECTS.read().len()
}

/// Clear the pending flag for an effect and remove from pending set.
///
/// Called when an effect has been processed.
#[allow(dead_code)]
pub fn clear_effect_pending(effect_id: EffectId) {
    effect_id.set_state(ReactiveState::Clean);
    PENDING_EFFECTS.write().swap_remove(&effect_id);
}

/// Remove an effect from the pending set (used when effect is destroyed).
pub fn remove_from_pending_set(effect_id: EffectId) {
    PENDING_EFFECTS.write().swap_remove(&effect_id);
}

// ============================================================================
// Three-state reactive system functions
// ============================================================================

/// Mark an effect as Check and add it to the pending set for eventual processing.
///
/// This is used for indirect dependents in the three-state system. When a signal
/// changes, direct subscribers are marked Dirty, but their downstream dependents
/// are only marked Check (they might not actually need to recompute).
///
/// Returns true if the effect was newly added to the pending set.
pub fn mark_effect_pending_for_check(effect_id: EffectId) -> bool {
    // Only add to pending set if the effect now needs work
    // (was previously Clean)
    let was_clean = effect_id.state() == ReactiveState::Clean;

    // The state upgrade is already done by the caller (mark_check_recursive)
    // We just need to add to pending set
    if was_clean {
        PENDING_EFFECTS.write().insert(effect_id);
    }

    was_clean
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
        assert_eq!(id.sources(), None);
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
