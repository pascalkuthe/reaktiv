// Arena-based storage for reactive node metadata
//
// This module provides two separate arenas:
// - Signal arena: stores SignalMetadata (version, subscribers)
// - Effect arena: stores EffectMetadata (unified struct for effects and computeds)
//
// The arenas use global static storage with RwLock for thread-safe access.
// SignalId and EffectId types are lightweight newtypes that index into the slabs.

// Note: We need to declare effect_arena first because signal_arena depends on EffectId
pub mod effect_arena;
pub mod signal_arena;

// Re-export types from effect_arena
pub use effect_arena::{
    CurrentEffectGuard, EffectId, EffectMetadata, ReactiveState, current_effect, destroy_children,
    effect_arena_insert, effect_arena_remove, mark_effect_pending, pending_effects_count,
    remove_from_pending_set, take_pending_effects,
};

// Re-export types from signal_arena
pub use signal_arena::{
    AnySubscriber, SignalId, SignalMetadata, signal_arena_insert, signal_arena_remove,
    with_signal_arena,
};
