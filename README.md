<h1>
  <img src="docs/logo.svg" alt="reaktiv logo" width="40" height="33" valign="middle">
  reaktiv
</h1>

A standalone, flexible fine-grained reactivity library designed to be unintrusive.

[![Crates.io](https://img.shields.io/crates/v/reaktiv.svg)](https://crates.io/crates/reaktiv)
[![Documentation](https://docs.rs/reaktiv/badge.svg)](https://docs.rs/reaktiv)
[![CI](https://github.com/pascalkuthe/reaktiv/actions/workflows/ci.yml/badge.svg)](https://github.com/pascalkuthe/reaktiv/actions)

## Motivation

This crate aims to bring the benefits of fine-grained reactivity to contexts beyond traditional web UI. Reactivity is a useful pattern for managing dependencies and propagating changes - it doesn't have to be tied to a specific framework or runtime. At the same time I had performance concerns
so this crate is also more optimized compared to other crates I found.

## Example Use Case: Python API Hook System

The particular use case that motivated this crate was building a subscription/hook system for a Python API (which also has a UI that uses the same reactivity system). When users change parameters through the API, downstream computations need to update automatically:

```rust
// Python bindings (via PyO3)
#[pyclass]
struct CircuitParameter {
    value: f64,
    signal: Signal,  // Just 4 bytes of metadata
}

#[pymethods]
impl CircuitParameter {
    fn set(&mut self, value: f64) {
        self.value = value;
        self.signal.emit();  // Triggers dependent computations
    }
}

// Rust side: derived values auto-update
let power = Computed::new(|| {
    voltage_signal.track_dependency();
    current_signal.track_dependency();
    voltage * current
});
```

Users get reactive behavior without knowing about signals - the API just works.

## What is Fine-Grained Reactivity?

Fine-grained reactivity automatically tracks dependencies between data and computations, updating only what changed. When you read a reactive value inside an effect, the system records that dependency. When that value changes, only the effects that depend on it re-run.

This is distinct from coarse-grained approaches (like virtual DOM diffing) where changes trigger broad re-renders that must be narrowed down afterward.

## Compared to `reactive_graph` (from Leptos)

`reactive_graph` is a good implementation of fine-grained reactivity (and inspired this crate) but ultimately did not fit what I was looking for:

* **Less intrusive design**: Leptos signals wrap and own your data (`Signal<T>`). I wanted signals to be just metadata - your values stay in your structs.
  - The approach Leptos uses can be good for usability as it avoids bugs caused by forgetting to notify signals
  - But for my use cases I was looking for more control over data ownership

* **Less overhead**: Reaktiv is carefully optimized and uses ~50% less memory compared to reactive_graph.
  - Leptos wraps all signal values in `Arc<RwLock<T>>` which adds overhead, especially for simple types
  - Leptos also uses `Arc<>` for the graph metadata itself
  - Reaktiv avoids wrapping values entirely and uses an arena allocator with a freelist for fast allocation/deallocation
  - Less memory means better cache locality and speed, particularly when working with many small reactive values

* **More flexible intermediate computations**: In reactive_graph, intermediate computations are a special case. If an effect simply writes to another signal, that's not the same as a computed (and can lead to duplicate updates). Reaktiv doesn't have such a special case - a single effect can update multiple signals without adverse effects.

* **Built to scale**: Reaktiv has the concept of skippable effects for handling large-scale UIs.
  - These are dependent UI computations (like validation) that can be processed opportunistically
  - If skippable effects exceed the frame time budget, reaktiv moves them to a background thread
  - Only "must-run" effects execute on the main thread, keeping the UI snappy
  - This is useful for UIs with large collections (thousands of elements) that update frequently

## Quick Start

```rust
use reaktiv::{Signal, Effect, Computed, Transaction, spawn_effect_loop};

struct Component {
    value: f64,
    signal: Signal,
}

impl Component {
    fn set(&mut self, v: f64) {
        self.value = v;
        self.signal.emit();
    }
}

spawn_effect_loop();

// Effects auto-track dependencies
let effect = Effect::new(|| {
    component.signal.track_dependency();
    println!("Value: {}", component.value);
});

// Batch multiple changes
Transaction::run(|| {
    component.set(1.0);
    component.set(2.0);
});
```

## When to Use This

- Building reactive systems outside web browsers
- FFI/Python bindings where intrusive design avoids wrapper overhead
- Desktop applications with Qt/GTK event loops
- Game engines needing predictable update timing

## When NOT to Use This

- Building web UIs (use more mature all-in-one options like Leptos or Dioxus instead)
- Need signals that wrap and own values (`Signal<T>`). These can be built on top of reaktiv but that's not its aim
- If you need a single-source of truth graph of signals like reactive-store provides for Leptos (again could be built on top but doesn't fit the core philosophy)

## Minimum Supported Rust Version (MSRV)

The current MSRV is **1.85** (required for Rust 2024 edition).

This crate tracks the Rust version available on the previous Ubuntu LTS release (currently Ubuntu 22.04). Since Canonical updates Firefox and its Rust toolchain for security reasons, the MSRV follows what's available through Ubuntu's package ecosystem. MSRV bumps that comply with this policy are not considered breaking changes.
