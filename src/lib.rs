#![deny(missing_docs)]

//! Standalone, flexible fine-grained reactivity
//!
//! This crate brings the benefits of fine-grained reactivity to contexts beyond traditional
//! web UI. Signals are lightweight metadata (4 bytes) - your values stay in your structs.
//!
//! # Quick Start
//!
//! ```ignore
//! use reaktiv::{Signal, Effect, Computed, Transaction, flush_effects};
//!
//! struct Component {
//!     value: f64,
//!     signal: Signal,  // Just 4 bytes of metadata
//! }
//!
//! impl Component {
//!     fn set(&mut self, v: f64) {
//!         self.value = v;
//!         self.signal.emit();  // Notify subscribers
//!     }
//!
//!     fn get(&self) -> f64 {
//!         self.signal.track_dependency();  // Auto-track in effects
//!         self.value
//!     }
//! }
//!
//! // Effects auto-track dependencies and run immediately
//! let effect = Effect::new(|| {
//!     let value = component.get();  // Tracks dependency automatically
//!     println!("Value: {}", value);
//! });
//!
//! // Batch changes with transactions - effects run once at the end
//! Transaction::run(|| {
//!     component.set(1.0);
//!     component.set(2.0);
//! });  // Effect runs once with final value
//!
//! flush_effects();
//! ```
//!
//! # Core Types
//!
//! - [`Signal`] - Lightweight reactive marker (4 bytes). Call [`emit()`](Signal::emit) when value changes.
//! - [`Effect`] - Auto-runs when tracked signals change. Runs immediately on creation.
//! - [`Computed<T>`] - Cached derived value. Only recomputes when dependencies change.
//! - [`Transaction`] - Batch multiple changes into one effect run.
//!
//! # Working with Signals
//!
//! ```ignore
//! let signal = Signal::new();
//!
//! // Inside an effect, track this signal as a dependency
//! signal.track_dependency();
//!
//! // Notify all subscribers when value changes
//! signal.emit();
//!
//! // Skip UI-only subscribers (useful for preventing update loops)
//! signal.emit_from_ui();
//! ```
//!
//! # Creating Effects
//!
//! ```ignore
//! // Effect runs immediately and tracks dependencies automatically
//! let effect = Effect::new(|| {
//!     signal.track_dependency();
//!     println!("Signal changed!");
//! });
//!
//! // Skippable effects can be deferred under load
//! let skippable = Effect::new_skippable(|| {
//!     // Non-critical UI updates
//! });
//! ```
//!
//! # Computed Values
//!
//! ```ignore
//! // Runs immediately and caches the result
//! let doubled = Computed::new(|| {
//!     input_signal.track_dependency();
//!     input_value * 2
//! });
//!
//! // Defers computation until first access
//! let lazy = Computed::lazy(|| expensive_computation());
//!
//! let value = doubled.get();  // Returns cached value
//! ```
//!
//! # Batching Updates
//!
//! ```ignore
//! // Effects run once at the end instead of 3 times
//! Transaction::run(|| {
//!     signal_a.emit();
//!     signal_b.emit();
//!     signal_c.emit();
//! });  // All pending effects run here
//!
//! // Suppress UI-triggered effects during internal updates
//! Transaction::no_ui_updates(|| {
//!     internal_recalculation();
//! });
//! ```
//!
//! # Processing Effects
//!
//! ```ignore
//! flush_effects();              // Process all pending effects synchronously
//! is_processing_scheduled();    // Check if effects are waiting to run
//! untracked(|| { ... });        // Read signals without tracking dependencies
//!
//! // For async/event loop integration
//! spawn_effect_loop();          // Start async effect processing
//! ```

// Internal modules
pub(crate) mod arena;
mod computed;
mod effect;
mod executor;
mod hash;
mod signal;
mod transaction;

// Core types
pub use computed::Computed;
pub use effect::Effect;
pub use signal::Signal;
pub use transaction::Transaction;

// Key functions
pub use effect::{flush_effects, is_processing_scheduled, untracked};

// Executor integration (for custom event loops)
pub use executor::{
    DEFAULT_BUDGET, DEFAULT_DEBOUNCE, DEFAULT_MAX_DEBOUNCE, EffectLoop, spawn_effect_loop,
};

#[cfg(test)]
mod tests;
