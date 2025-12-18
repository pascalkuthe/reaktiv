# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2025-XX-XX

Initial release.

### Added

- `Signal` - Lightweight reactive marker (4 bytes) for change notification
- `Effect` - Side-effectful computations that auto-run when dependencies change
- `Computed<T>` - Memoized derived values with lazy evaluation support
- `Transaction` - Batch multiple signal changes into single effect runs
- Three-state (Clean/Check/Dirty) change propagation for minimal recomputation
- Hierarchical effects with automatic child cleanup
- Origin tracking to distinguish API vs UI updates
- Async executor integration with configurable debounce
- `untracked()` for reading signals without creating dependencies
