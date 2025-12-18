# reaktiv

A standalone, flexible fine-grained reactivity library designed to be unintrusive.

[![Crates.io](https://img.shields.io/crates/v/reaktiv.svg)](https://crates.io/crates/reaktiv)
[![Documentation](https://docs.rs/reaktiv/badge.svg)](https://docs.rs/reaktiv)
[![CI](https://github.com/pascalkuthe/reaktiv/actions/workflows/ci.yml/badge.svg)](https://github.com/pascalkuthe/reaktiv/actions)

## Philosophy

This crate aims to bring the benefits of fine-grained reactivity to contexts beyond traditional web UI. Reactivity is a powerful pattern for managing dependencies and propagating changes - it doesn't have to be tied to a specific framework or runtime.

**Standalone**: No framework assumptions. No runtime requirements. Just a library you call.

**Flexible**: Works with any event loop (Qt, GTK, game loops) or no event loop at all. You control when effects run via explicit `flush_effects()`.

**Unintrusive**: Signals don't wrap your data - they're just lightweight metadata (4 bytes). Your values stay where they belong, in your structs.

## Example Use Case: Python API Hook System

A compelling use case is building a subscription/hook system for a Python API. When users change parameters through the API, downstream computations need to update automatically:

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

## vs `reactive_graph` (from Leptos)

**More low-level.** `reactive_graph` is optimized for WASM and web microtask scheduling. This crate is a primitive building block - no microtask integration, no framework. You control scheduling via explicit `flush_effects()`.

**Intrusive design.** Leptos signals wrap and own your data (`Signal<T>`). Here, signals are just metadata - your values stay in your structs. Lower memory overhead (4 bytes vs 16+).

## vs `futures-signals` / `dioxus-signals`

These are not fine-grained reactive systems in the same sense. They don't provide automatic dependency tracking with push-pull propagation. This crate implements true fine-grained reactivity with three-state (Clean/Check/Dirty) change propagation.

## Quick Start

```rust
use reaktiv::{Signal, Effect, Computed, Transaction, flush_effects};

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
// Effect runs once, not twice

flush_effects();
```

## Core Concepts

- **Signal** - Lightweight reactive marker (4 bytes). Call `emit()` when value changes.
- **Effect** - Side-effectful computation. Auto-runs when dependencies change.
- **Computed** - Memoized derived value. Recomputes only when inputs change.
- **Transaction** - Batch multiple signal changes into one effect run.

## Key Features

- **Fine-grained tracking**: Automatic dependency detection
- **Three-state propagation**: Clean/Check/Dirty minimizes unnecessary work
- **Debounced execution**: Multiple invalidations batch into single run
- **Hierarchical effects**: Parent effects clean up children automatically
- **Origin tracking**: Distinguish API vs UI updates to prevent loops
- **Explicit scheduling**: `flush_effects()` for predictable timing

## When to Use This

- Building reactive systems outside web browsers
- FFI/Python bindings where intrusive design avoids wrapper overhead
- Desktop applications with Qt/GTK event loops
- Game engines needing predictable update timing
- Any context where you want reactivity without framework lock-in

## When NOT to Use This

- Building web applications (use Leptos or Dioxus instead)
- Need signals that wrap and own values (`Signal<T>`)

## Minimum Supported Rust Version (MSRV)

The current MSRV is **1.85** (required for Rust 2024 edition).

This crate tracks the Rust version available on the previous Ubuntu LTS release (currently Ubuntu 22.04). Since Canonical updates Firefox and its Rust toolchain for security reasons, the MSRV follows what's available through Ubuntu's package ecosystem. MSRV bumps that comply with this policy are not considered breaking changes.

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.
