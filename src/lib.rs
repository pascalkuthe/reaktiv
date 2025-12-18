#![deny(missing_docs)]

//! Standalone, flexible fine-grained reactivity.
//!
//! This crate brings the benefits of fine-grained reactivity to contexts beyond traditional
//! web UI. Signals are lightweight metadata (4 bytes) while your values stay in your structs.
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
//! }
//!
//! // Effects auto-track dependencies
//! let effect = Effect::new(|| {
//!     component.signal.track_dependency();
//!     println!("Value: {}", component.value);
//! });
//!
//! // Batch changes with transactions
//! Transaction::run(|| {
//!     component.set(1.0);
//!     component.set(2.0);
//! });
//! // Effect runs once, not twice
//!
//! flush_effects();
//! ```
//!
//! # Core Types
//!
//! - [`Signal`] - Lightweight reactive marker. Call [`emit()`](Signal::emit) when value changes.
//! - [`Effect`] - Side-effectful computation. Auto-runs when dependencies change.
//! - [`Computed<T>`] - Memoized value. Recomputes only when dependencies change.
//! - [`Transaction`] - Batch multiple signal changes into one effect run.
//!
//! # Signal
//!
//! ```ignore
//! let signal = Signal::new();
//! signal.track_dependency();  // Register current effect as subscriber
//! signal.emit();              // Notify all subscribers
//! signal.emit_from_ui();      // Skip UI-only subscribers (prevents loops)
//! ```
//!
//! # Effect
//!
//! ```ignore
//! // Runs immediately, then re-runs when dependencies change
//! let effect = Effect::new(|| {
//!     signal.track_dependency();
//!     println!("Signal changed!");
//! });
//!
//! // For callbacks that must run on main thread (e.g., Python GIL)
//! let local = Effect::new_local(|| { ... });
//! ```
//!
//! # Computed
//!
//! ```ignore
//! // Evaluates immediately
//! let doubled = Computed::new(|| {
//!     input_signal.track_dependency();
//!     input_value * 2
//! });
//!
//! // Defers evaluation until first get()
//! let lazy = Computed::lazy(|| expensive_computation());
//!
//! let value = doubled.get();  // Returns cached value, recomputes if stale
//! ```
//!
//! # Transaction
//!
//! ```ignore
//! // Batch changes - effects run once at end
//! Transaction::run(|| {
//!     signal_a.emit();
//!     signal_b.emit();
//! });
//!
//! // Also suppress UI-triggered effects
//! Transaction::run_no_ui_updates(|| { ... });
//! ```
//!
//! # Processing Effects
//!
//! ```ignore
//! flush_effects();              // Process all pending effects now
//! is_processing_scheduled();    // Check if effects are pending
//! untracked(|| { ... });        // Run without tracking dependencies
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

// Async executor integration (for custom event loops)
pub use executor::{DEFAULT_DEBOUNCE, Executor, run_effect_loop_with_debounce};

#[cfg(test)]
mod tests;
