# Contributing to reaktiv

## Running Tests

Tests must run single-threaded:

```bash
cargo test
```

This is automatically configured in `.cargo/config.toml` via `RUST_TEST_THREADS = "1"`.

Alternatively, use [cargo-nextest](https://nexte.st/) which runs each test in a separate process, providing natural isolation:

```bash
cargo nextest run
```

### Why Single-Threaded Tests?

This crate uses a **global reactive runtime** with shared arenas for signals and effects. This is a deliberate design choice common in reactive systems (similar to Leptos, SolidJS, etc.) that provides:

- Zero-cost signal/effect creation (no runtime parameter passing)
- Simple API without explicit context management
- Efficient global deduplication and batching

However, this means `flush_effects()` processes **all** pending effects globally, not just those from the current test. When tests run in parallel:

1. Test A creates an effect and calls `signal.emit()`
2. Test B creates an effect and calls `signal.emit()`
3. Test A calls `flush_effects()` — this processes effects from **both** tests
4. Test B's assertions may fail because its effects already ran

This is correct runtime behavior (one global reactive system), but causes test pollution when tests assume isolation.

### Alternative Designs Considered

- **Thread-local runtime**: Would isolate tests but add overhead and complexity to the API
- **Explicit context**: Would require passing a `Runtime` to every signal/effect creation
- **Test-specific isolation**: Would add complexity without benefiting production use

The current design prioritizes a clean, efficient API for the common case (single application runtime).
