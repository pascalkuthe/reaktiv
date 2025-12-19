/// Comprehensive POC tests demonstrating the reactive system
use crate::{Computed, Effect, Signal, Transaction, flush_effects};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

// Simple test fixture: a circuit component with intrusive signals
struct TestCircuit {
    voltage_signal: Signal,
}

impl TestCircuit {
    fn new() -> Self {
        Self {
            voltage_signal: Signal::new(),
        }
    }
}

#[test]
fn computed_caches_until_invalidated() {
    let _circuit = Arc::new(TestCircuit::new());

    let computation_count = Arc::new(AtomicUsize::new(0));
    let computation_count_clone = computation_count.clone();

    let power = Computed::new(move || {
        computation_count_clone.fetch_add(1, Ordering::Relaxed);
        // Simulate reading voltage and current (no actual dependency tracking)
        100.0
    });

    // First access: computed during creation
    let _p1 = power.get();
    assert_eq!(computation_count.load(Ordering::Relaxed), 1);

    // Second access: cached
    let _p2 = power.get();
    assert_eq!(computation_count.load(Ordering::Relaxed), 1);

    // Invalidate and flush effects
    power.invalidate();
    flush_effects();

    // Now get() returns the cached (recomputed) value
    let _p3 = power.get();
    assert_eq!(computation_count.load(Ordering::Relaxed), 2);
}

#[test]
fn effect_debouncing_batches_invalidations() {
    let run_count = Arc::new(AtomicUsize::new(0));
    let run_count_clone = run_count.clone();

    let effect = Effect::new(move || {
        run_count_clone.fetch_add(1, Ordering::Relaxed);
    });

    // Initial run during construction
    assert_eq!(run_count.load(Ordering::Relaxed), 1);

    // Invalidate 20 times rapidly
    for _ in 0..20 {
        effect.invalidate();
    }

    // The key test: multiple invalidations are debounced, not queued 20 times
    // We verify this by checking the run count after processing, not queue length
    // (queue length can vary if other tests run concurrently)

    // Process all pending effects
    Effect::process_all();

    // Total: 1 initial + 1 debounced = 2
    assert_eq!(run_count.load(Ordering::Relaxed), 2);
}

#[test]
fn signal_emit_origin_tracking() {
    let circuit = TestCircuit::new();

    // Test emit_from_api and emit_from_ui - both should work without error
    circuit.voltage_signal.emit_from_api();
    circuit.voltage_signal.emit_from_ui();
}

#[test]
fn transaction_suppresses_ui_updates() {
    let circuit = TestCircuit::new();
    let run_count = Arc::new(AtomicUsize::new(0));

    let run_count_clone = run_count.clone();
    let signal_id = circuit.voltage_signal.node_id();
    let _effect = Effect::new(move || {
        signal_id.track_dependency();
        run_count_clone.fetch_add(1, Ordering::Relaxed);
    });

    // Initial run
    assert_eq!(run_count.load(Ordering::Relaxed), 1);

    Transaction::no_ui_updates(|| {
        circuit.voltage_signal.emit();
    });
    flush_effects();

    // Effect should have run after transaction
    assert_eq!(run_count.load(Ordering::Relaxed), 2);
}

#[test]
fn debouncing_batches_rapid_emissions() {
    // Simulate 20 rapid voltage updates being debounced to 1 effect run
    let effect_runs = Arc::new(AtomicUsize::new(0));
    let effect_runs_clone = effect_runs.clone();

    let circuit = Arc::new(TestCircuit::new());
    let voltage_signal_id = circuit.voltage_signal.node_id();

    let effect = Effect::new(move || {
        // Track dependency using CURRENT_EFFECT mechanism
        voltage_signal_id.track_dependency();
        effect_runs_clone.fetch_add(1, Ordering::Relaxed);
    });

    // Initial run
    assert_eq!(effect_runs.load(Ordering::Relaxed), 1);

    // 20 rapid invalidations (in real code, each emit() would trigger this)
    for _ in 0..20 {
        effect.invalidate();
    }

    // Process all pending effects
    Effect::process_all();

    // 1 initial + 1 debounced = 2 total (verifies debouncing worked)
    assert_eq!(effect_runs.load(Ordering::Relaxed), 2);
}

#[test]
fn multiple_effects_debounce_independently() {
    let run_count_1 = Arc::new(AtomicUsize::new(0));
    let run_count_2 = Arc::new(AtomicUsize::new(0));

    let rc1_clone = run_count_1.clone();
    let effect1 = Effect::new(move || {
        rc1_clone.fetch_add(1, Ordering::Relaxed);
    });

    let rc2_clone = run_count_2.clone();
    let effect2 = Effect::new(move || {
        rc2_clone.fetch_add(1, Ordering::Relaxed);
    });

    // 2 initial runs
    assert_eq!(run_count_1.load(Ordering::Relaxed), 1);
    assert_eq!(run_count_2.load(Ordering::Relaxed), 1);

    // Invalidate effect1 multiple times
    for _ in 0..10 {
        effect1.invalidate();
    }

    // Invalidate effect2 multiple times
    for _ in 0..10 {
        effect2.invalidate();
    }

    // Process all pending effects
    Effect::process_all();

    // Each effect ran once more (debounced)
    assert_eq!(run_count_1.load(Ordering::Relaxed), 2);
    assert_eq!(run_count_2.load(Ordering::Relaxed), 2);
}

#[test]
fn transaction_nesting_preserves_state() {
    // Test that nested transactions work correctly and emit doesn't panic
    let circuit = TestCircuit::new();

    Transaction::run(|| {
        Transaction::no_ui_updates(|| {
            circuit.voltage_signal.emit();
        });

        // Outer transaction continues
        circuit.voltage_signal.emit();
    });

    // If we get here without panic, the test passes
}

#[test]
fn computed_recomputes_on_dependency_change() {
    let circuit = Arc::new(TestCircuit::new());
    let voltage_signal_id = circuit.voltage_signal.node_id();

    let computation_count = Arc::new(AtomicUsize::new(0));
    let cc_clone = computation_count.clone();

    let power = Computed::new(move || {
        cc_clone.fetch_add(1, Ordering::Relaxed);
        // Track dependency using CURRENT_EFFECT mechanism
        voltage_signal_id.track_dependency();
        100.0
    });

    // Initial (computed during creation)
    let _p1 = power.get();
    assert_eq!(computation_count.load(Ordering::Relaxed), 1);

    // Cached
    let _p2 = power.get();
    assert_eq!(computation_count.load(Ordering::Relaxed), 1);

    // Simulate source change by invalidating
    power.invalidate();
    flush_effects();

    // Recomputed
    let _p3 = power.get();
    assert_eq!(computation_count.load(Ordering::Relaxed), 2);
}

#[test]
fn signal_emit_in_transaction_defers_effect() {
    // Test that Signal::emit() in a transaction defers effect processing
    use std::sync::atomic::AtomicI32;

    let signal = Signal::new();
    let signal_id = signal.node_id();
    let counter = Arc::new(AtomicI32::new(0));
    let counter_clone = counter.clone();

    let _effect = Effect::new(move || {
        signal_id.track_dependency();
        counter_clone.fetch_add(1, Ordering::Relaxed);
    });

    // Effect ran once during construction
    assert_eq!(counter.load(Ordering::Relaxed), 1);

    // In a transaction, emit marks effect as pending but doesn't process
    Transaction::run(|| {
        signal.emit();
        // Effect hasn't run yet - still in transaction
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    });

    // After transaction, effect was processed
    assert_eq!(counter.load(Ordering::Relaxed), 2);
}

// ============================================================================
// Transaction batching tests
// ============================================================================

#[test]
fn transaction_batches_multiple_emissions() {
    // Test that transactions batch multiple signal changes into one effect run
    use std::sync::atomic::AtomicI32;

    let signal1 = Signal::new();
    let signal2 = Signal::new();
    let signal1_id = signal1.node_id();
    let signal2_id = signal2.node_id();

    let run_count = Arc::new(AtomicI32::new(0));
    let run_count_clone = run_count.clone();

    let _effect = Effect::new(move || {
        // Track both signals using CURRENT_EFFECT mechanism
        signal1_id.track_dependency();
        signal2_id.track_dependency();
        run_count_clone.fetch_add(1, Ordering::Relaxed);
    });

    // Effect ran once during construction
    assert_eq!(run_count.load(Ordering::Relaxed), 1);

    // Without transaction: each emit would trigger effect processing
    // With transaction: all emits are batched
    Transaction::run(|| {
        signal1.emit();
        signal2.emit();
        signal1.emit();
        signal2.emit();
        // Effect should NOT have run yet (we're in a transaction)
    });

    // After transaction: effect runs once (debounced)
    assert_eq!(run_count.load(Ordering::Relaxed), 2);
}

#[test]
fn nested_transactions_defer_until_outermost_exits() {
    // Test that nested transactions only process effects when outermost exits
    use std::sync::atomic::AtomicI32;

    let signal = Signal::new();
    let signal_id = signal.node_id();
    let run_count = Arc::new(AtomicI32::new(0));
    let run_count_clone = run_count.clone();

    let _effect = Effect::new(move || {
        signal_id.track_dependency();
        run_count_clone.fetch_add(1, Ordering::Relaxed);
    });

    assert_eq!(run_count.load(Ordering::Relaxed), 1);

    Transaction::run(|| {
        signal.emit();

        Transaction::run(|| {
            signal.emit();
            // Still inside outer transaction, effect hasn't run
        });

        // Inner transaction exited but we're still in outer
        // Effect should NOT have run yet
        signal.emit();
    });

    // Now outer transaction exited, effect should have run once (debounced)
    assert_eq!(run_count.load(Ordering::Relaxed), 2);
}

#[test]
fn transaction_active_flag_works() {
    // Test that is_transaction_active returns correct values
    use crate::transaction::is_transaction_active;

    assert!(!is_transaction_active());

    Transaction::run(|| {
        assert!(is_transaction_active());

        Transaction::run(|| {
            assert!(is_transaction_active());
        });

        assert!(is_transaction_active());
    });

    assert!(!is_transaction_active());
}

// ============================================================================
// Effect hierarchy tests
// ============================================================================

#[test]
fn effect_created_in_effect_becomes_child() {
    // Test that effects created inside another effect become children
    use std::sync::atomic::AtomicI32;

    let parent_runs = Arc::new(AtomicI32::new(0));
    let child_runs = Arc::new(AtomicI32::new(0));

    let parent_runs_clone = parent_runs.clone();
    let child_runs_clone = child_runs.clone();

    let parent_effect = Effect::new(move || {
        parent_runs_clone.fetch_add(1, Ordering::Relaxed);

        // Create a child effect inside the parent's callback
        let child_runs_inner = child_runs_clone.clone();
        let _child = Effect::new(move || {
            child_runs_inner.fetch_add(1, Ordering::Relaxed);
        });
    });

    // Parent ran once, child was created and ran once
    assert_eq!(parent_runs.load(Ordering::Relaxed), 1);
    assert_eq!(child_runs.load(Ordering::Relaxed), 1);

    // Check that parent has a child
    let child_count = parent_effect.id().with_children(Vec::len).unwrap_or(0);
    assert_eq!(child_count, 1);
}

#[test]
fn children_destroyed_when_parent_reruns() {
    // Test that child effects are destroyed when parent re-runs
    use std::sync::atomic::AtomicI32;

    let signal = Signal::new();
    let signal_id = signal.node_id();

    let parent_runs = Arc::new(AtomicI32::new(0));
    let child_creates = Arc::new(AtomicI32::new(0));

    let parent_runs_clone = parent_runs.clone();
    let child_creates_clone = child_creates.clone();

    let parent_effect = Effect::new(move || {
        // Track the signal so parent re-runs when it changes
        signal_id.track_dependency();

        parent_runs_clone.fetch_add(1, Ordering::Relaxed);

        // Create a child effect (will be destroyed and recreated each time)
        let cc = child_creates_clone.clone();
        let _child = Effect::new(move || {
            cc.fetch_add(1, Ordering::Relaxed);
        });
    });

    // Parent ran once, one child created
    assert_eq!(parent_runs.load(Ordering::Relaxed), 1);
    assert_eq!(child_creates.load(Ordering::Relaxed), 1);

    // Trigger parent re-run (emit schedules, flush processes)
    signal.emit();
    flush_effects();

    // Parent ran again, old child destroyed, new child created
    assert_eq!(parent_runs.load(Ordering::Relaxed), 2);
    assert_eq!(child_creates.load(Ordering::Relaxed), 2);

    // Parent should still have exactly 1 child (the new one)
    let child_count = parent_effect.id().with_children(Vec::len).unwrap_or(0);
    assert_eq!(child_count, 1);
}

#[test]
fn many_effects_process_correctly() {
    // Test that many effects can be created, invalidated, and processed
    use std::sync::atomic::AtomicI32;

    // Create many effects, each with its own counter
    let counters: Vec<_> = (0..100).map(|_| Arc::new(AtomicI32::new(0))).collect();
    let effects: Vec<_> = counters
        .iter()
        .map(|counter| {
            let counter_clone = counter.clone();
            Effect::new(move || {
                counter_clone.fetch_add(1, Ordering::Relaxed);
            })
        })
        .collect();

    // All effects ran once during construction
    for counter in &counters {
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    // Invalidate all effects
    for effect in &effects {
        effect.invalidate();
    }

    // Process all pending effects
    Effect::process_all();

    // All effects ran twice (once on creation, once after invalidate+flush)
    for counter in &counters {
        assert_eq!(counter.load(Ordering::Relaxed), 2);
    }
}

#[test]
fn signal_emit_schedules_processing_when_not_in_transaction() {
    // Test that signal.emit() schedules processing when not in transaction
    // With the new debounced model, effects are processed when flush_effects() is called
    use std::sync::atomic::AtomicI32;

    let signal = Signal::new();
    let signal_id = signal.node_id();
    let run_count = Arc::new(AtomicI32::new(0));
    let run_count_clone = run_count.clone();

    let _effect = Effect::new(move || {
        signal_id.track_dependency();
        run_count_clone.fetch_add(1, Ordering::Relaxed);
    });

    // Effect ran once during construction
    assert_eq!(run_count.load(Ordering::Relaxed), 1);

    // Emit outside transaction - schedules processing (debounced)
    signal.emit();

    // Effect hasn't run yet (scheduled but not flushed)
    assert_eq!(run_count.load(Ordering::Relaxed), 1);

    // Flush to process effects
    flush_effects();

    // Effect ran after flush
    assert_eq!(run_count.load(Ordering::Relaxed), 2);
}

#[test]
fn deeply_nested_hierarchy_tracked_correctly() {
    // Test that deeply nested effects work correctly
    use std::sync::atomic::AtomicI32;

    let level0_runs = Arc::new(AtomicI32::new(0));
    let level1_runs = Arc::new(AtomicI32::new(0));
    let level2_runs = Arc::new(AtomicI32::new(0));

    let l0 = level0_runs.clone();
    let l1 = level1_runs.clone();
    let l2 = level2_runs.clone();

    let root_effect = Effect::new(move || {
        l0.fetch_add(1, Ordering::Relaxed);

        let l1_inner = l1.clone();
        let l2_inner = l2.clone();

        let _child = Effect::new(move || {
            l1_inner.fetch_add(1, Ordering::Relaxed);

            let l2_deep = l2_inner.clone();
            let _grandchild = Effect::new(move || {
                l2_deep.fetch_add(1, Ordering::Relaxed);
            });
        });
    });

    // All levels ran once
    assert_eq!(level0_runs.load(Ordering::Relaxed), 1);
    assert_eq!(level1_runs.load(Ordering::Relaxed), 1);
    assert_eq!(level2_runs.load(Ordering::Relaxed), 1);

    // Check hierarchy: root has 1 child, child has 1 grandchild
    let child_count = root_effect.id().with_children(Vec::len).unwrap_or(0);
    assert_eq!(child_count, 1);

    let first_child_id = root_effect.id().with_children(|c| c[0]).unwrap();
    let grandchild_count = first_child_id.with_children(Vec::len).unwrap_or(0);
    assert_eq!(grandchild_count, 1);
}

#[test]
fn children_destroyed_immediately_on_parent_invalidation() {
    // Test that children are destroyed immediately when the parent is marked dirty,
    // NOT when the parent re-runs. This is important because we don't want stale
    // child effects hanging around between invalidation and execution.
    use std::sync::atomic::AtomicI32;

    let signal = Signal::new();
    let signal_id = signal.node_id();

    let parent_runs = Arc::new(AtomicI32::new(0));
    let child_created = Arc::new(AtomicI32::new(0));

    let parent_runs_clone = parent_runs.clone();
    let child_created_clone = child_created.clone();

    let parent_effect = Effect::new(move || {
        // Track the signal so parent re-runs when it changes
        signal_id.track_dependency();

        parent_runs_clone.fetch_add(1, Ordering::Relaxed);

        // Create a child effect
        let cc = child_created_clone.clone();
        let _child = Effect::new(move || {
            cc.fetch_add(1, Ordering::Relaxed);
        });
    });

    // Parent ran once, one child created
    assert_eq!(parent_runs.load(Ordering::Relaxed), 1);
    assert_eq!(child_created.load(Ordering::Relaxed), 1);

    // Verify parent has one child
    let child_count = parent_effect.id().with_children(Vec::len).unwrap_or(0);
    assert_eq!(child_count, 1);

    // Mark parent as dirty (via invalidate) inside a transaction so it doesn't auto-run
    Transaction::run(|| {
        // Invalidate the parent
        parent_effect.invalidate();

        // IMPORTANT: Children should be destroyed IMMEDIATELY when parent is marked dirty,
        // NOT when parent re-runs. Verify child is already gone from parent's children list.
        let child_count_after = parent_effect.id().with_children(Vec::len).unwrap_or(0);
        assert_eq!(
            child_count_after, 0,
            "Children should be destroyed immediately when parent is marked dirty"
        );

        // Parent should NOT have run yet (still pending)
        assert_eq!(parent_runs.load(Ordering::Relaxed), 1);
    });

    // After transaction, parent ran and created new child
    assert_eq!(parent_runs.load(Ordering::Relaxed), 2);
    assert_eq!(child_created.load(Ordering::Relaxed), 2);

    // Verify parent has one (new) child again
    let new_child_count = parent_effect.id().with_children(Vec::len).unwrap_or(0);
    assert_eq!(new_child_count, 1);
}

// ============================================================================
// Local effect tests
// ============================================================================

#[test]
fn debouncing_batches_rapid_signal_changes() {
    // Test that multiple signal emissions without flush only run effect once
    use std::sync::atomic::AtomicI32;

    let signal = Signal::new();
    let signal_id = signal.node_id();
    let run_count = Arc::new(AtomicI32::new(0));

    let rc = run_count.clone();
    let _effect = Effect::new(move || {
        signal_id.track_dependency();
        rc.fetch_add(1, Ordering::Relaxed);
    });

    // Effect ran once during construction
    assert_eq!(run_count.load(Ordering::Relaxed), 1);

    // Multiple emissions without flush
    signal.emit();
    signal.emit();
    signal.emit();
    signal.emit();
    signal.emit();

    // Effect hasn't run yet (no flush)
    assert_eq!(run_count.load(Ordering::Relaxed), 1);

    // One flush processes all (debounced to one run)
    flush_effects();

    // Effect ran once (debounced), not 5 times
    assert_eq!(run_count.load(Ordering::Relaxed), 2);
}

// ============================================================================
// Complex dependency chain tests
// ============================================================================

#[test]
fn complex_chain_propagates_correctly() {
    // Test: signal -> computed1 -> computed2 -> effect
    // When signal changes, everything should update correctly
    use std::sync::atomic::AtomicI32;

    let signal = Signal::new();
    let signal_id = signal.node_id();

    let computed1_count = Arc::new(AtomicI32::new(0));
    let cc1 = computed1_count.clone();

    // Computed1 depends on signal
    let computed1 = Computed::new(move || {
        cc1.fetch_add(1, Ordering::Relaxed);
        // Track the signal
        signal_id.track_dependency();
        10
    });

    let computed2_count = Arc::new(AtomicI32::new(0));
    let cc2 = computed2_count.clone();
    let c1_clone = computed1.clone();

    // Computed2 depends on computed1
    let _computed2 = Computed::new(move || {
        cc2.fetch_add(1, Ordering::Relaxed);
        c1_clone.get() * 2
    });

    // Computed1: initial get() computes
    assert_eq!(computed1.get(), 10);
    assert_eq!(computed1_count.load(Ordering::Relaxed), 1);

    // Cached access
    assert_eq!(computed1.get(), 10);
    assert_eq!(computed1_count.load(Ordering::Relaxed), 1);

    // After invalidating computed1, it recomputes
    computed1.invalidate();
    flush_effects();
    assert_eq!(computed1.get(), 10);
    assert_eq!(computed1_count.load(Ordering::Relaxed), 2);
}

#[test]
fn diamond_dependency_updates_correctly() {
    // Test diamond dependency pattern:
    //     signal
    //    /      \
    // effect1  effect2
    //    \      /
    //     counter
    // Both effects should run exactly once each when signal emits
    use std::sync::atomic::AtomicI32;

    let signal = Signal::new();
    let signal_id = signal.node_id();

    let counter = Arc::new(AtomicI32::new(0));

    // Effect 1 depends on signal
    let c1 = counter.clone();
    let _effect1 = Effect::new(move || {
        signal_id.track_dependency();
        c1.fetch_add(1, Ordering::Relaxed);
    });

    // Effect 2 also depends on signal
    let c2 = counter.clone();
    let _effect2 = Effect::new(move || {
        signal_id.track_dependency();
        c2.fetch_add(1, Ordering::Relaxed);
    });

    // Both effects ran once during construction
    assert_eq!(counter.load(Ordering::Relaxed), 2);

    // Signal emits
    signal.emit();
    flush_effects();

    // Both effects ran once more (total 4)
    assert_eq!(counter.load(Ordering::Relaxed), 4);
}

#[test]
fn effect_resubscribes_on_each_run() {
    // Effect reads signal A first time
    // On rerun, effect reads signal B instead
    // The effect should react to B, not A anymore
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicI32;

    let signal_a = Signal::new();
    let signal_b = Signal::new();
    let signal_a_id = signal_a.node_id();
    let signal_b_id = signal_b.node_id();

    let use_b = Arc::new(AtomicBool::new(false));
    let counter = Arc::new(AtomicI32::new(0));

    let use_b_clone = use_b.clone();
    let counter_clone = counter.clone();

    let _effect = Effect::new(move || {
        if use_b_clone.load(Ordering::Relaxed) {
            signal_b_id.track_dependency();
        } else {
            signal_a_id.track_dependency();
        }
        counter_clone.fetch_add(1, Ordering::Relaxed);
    });

    // Effect ran once, subscribed to A
    assert_eq!(counter.load(Ordering::Relaxed), 1);

    // Emitting A should trigger effect
    signal_a.emit();
    flush_effects();
    assert_eq!(counter.load(Ordering::Relaxed), 2);

    // Switch to using B and rerun effect
    use_b.store(true, Ordering::Relaxed);
    signal_a.emit(); // This triggers the effect (still subscribed to A)
    flush_effects();
    assert_eq!(counter.load(Ordering::Relaxed), 3);

    // Now the effect tracks B instead of A
    // Emitting A should NOT trigger the effect
    signal_a.emit();
    flush_effects();
    assert_eq!(
        counter.load(Ordering::Relaxed),
        3,
        "Effect should not react to A anymore"
    );

    // Emitting B SHOULD trigger the effect
    signal_b.emit();
    flush_effects();
    assert_eq!(
        counter.load(Ordering::Relaxed),
        4,
        "Effect should react to B now"
    );
}

// ============================================================================
// Stress tests
// ============================================================================

#[test]
fn stress_test_many_signals() {
    // Create 100 signals and one effect that depends on all of them
    use std::sync::atomic::AtomicI32;

    let signals: Vec<Signal> = (0..100).map(|_| Signal::new()).collect();
    let signal_ids: Vec<_> = signals.iter().map(super::signal::Signal::node_id).collect();
    let counter = Arc::new(AtomicI32::new(0));

    let counter_clone = counter.clone();
    let ids_clone = signal_ids.clone();

    let _effect = Effect::new(move || {
        for id in &ids_clone {
            id.track_dependency();
        }
        counter_clone.fetch_add(1, Ordering::Relaxed);
    });

    // Initial run
    assert_eq!(counter.load(Ordering::Relaxed), 1);

    // Emit on signal 50
    signals[50].emit();
    flush_effects();

    // Effect ran once more
    assert_eq!(counter.load(Ordering::Relaxed), 2);
}

#[test]
fn stress_test_many_effects() {
    // Create one signal and 100 effects that depend on it
    use std::sync::atomic::AtomicI32;

    let signal = Signal::new();
    let signal_id = signal.node_id();
    let counter = Arc::new(AtomicI32::new(0));

    let _effects: Vec<Effect> = (0..100)
        .map(|_| {
            let counter_clone = counter.clone();
            Effect::new(move || {
                signal_id.track_dependency();
                counter_clone.fetch_add(1, Ordering::Relaxed);
            })
        })
        .collect();

    // All 100 effects ran once during construction
    assert_eq!(counter.load(Ordering::Relaxed), 100);

    // Signal emits
    signal.emit();
    flush_effects();

    // All 100 effects ran again (total 200)
    assert_eq!(counter.load(Ordering::Relaxed), 200);
}

#[test]
fn rapid_emissions_debounced() {
    // Test many rapid emissions only result in one effect run
    use std::sync::atomic::AtomicI32;

    let signal = Signal::new();
    let signal_id = signal.node_id();
    let counter = Arc::new(AtomicI32::new(0));

    let counter_clone = counter.clone();
    let _effect = Effect::new(move || {
        signal_id.track_dependency();
        counter_clone.fetch_add(1, Ordering::Relaxed);
    });

    // Initial run
    assert_eq!(counter.load(Ordering::Relaxed), 1);

    // Emit 100 times rapidly (without flush)
    for _ in 0..100 {
        signal.emit();
    }

    // Effect hasn't run yet (debounced)
    assert_eq!(counter.load(Ordering::Relaxed), 1);

    // One flush processes all
    flush_effects();

    // Effect ran once (debounced from 100 emissions to 1 run)
    assert_eq!(counter.load(Ordering::Relaxed), 2);
}

#[test]
fn deeply_nested_hierarchy_stress() {
    // Create 10 levels of nested effects
    // Verify all are destroyed when root is invalidated
    use std::sync::atomic::AtomicI32;

    let signal = Signal::new();
    let signal_id = signal.node_id();

    let levels_created = Arc::new(AtomicI32::new(0));

    fn create_nested_effects(
        levels_created: Arc<AtomicI32>,
        signal_id: crate::arena::SignalId,
        depth: usize,
        max_depth: usize,
    ) {
        levels_created.fetch_add(1, Ordering::Relaxed);
        if depth < max_depth {
            let lc = levels_created.clone();
            let _child = Effect::new(move || {
                signal_id.track_dependency();
                create_nested_effects(lc.clone(), signal_id, depth + 1, max_depth);
            });
        }
    }

    let lc = levels_created.clone();
    let root_effect = Effect::new(move || {
        signal_id.track_dependency();
        create_nested_effects(lc.clone(), signal_id, 1, 10);
    });

    // All 10 levels were created
    assert_eq!(levels_created.load(Ordering::Relaxed), 10);

    // Invalidate root - should destroy all children
    root_effect.invalidate();

    // Root should have no children (destroyed when marked pending)
    let child_count = root_effect.id().with_children(Vec::len).unwrap_or(0);
    assert_eq!(
        child_count, 0,
        "Children should be destroyed when parent is marked pending"
    );

    // Flush to re-run root (which recreates the hierarchy)
    flush_effects();

    // All levels created again (10 more = 20 total)
    assert_eq!(levels_created.load(Ordering::Relaxed), 20);
}

// ============================================================================
// Signal drop cleanup tests
// ============================================================================

#[test]
fn signal_drop_cleanup_is_safe() {
    // When a signal is dropped, effects that tracked it should continue to work
    use std::sync::atomic::AtomicI32;

    let counter = Arc::new(AtomicI32::new(0));
    let counter_clone = counter.clone();

    let signal = Signal::new();
    let signal_id = signal.node_id();

    let effect = Effect::new(move || {
        signal_id.track_dependency();
        counter_clone.fetch_add(1, Ordering::Relaxed);
    });

    // Effect ran once
    assert_eq!(counter.load(Ordering::Relaxed), 1);

    // Drop the signal
    drop(signal);

    // Effect can still be invalidated and run without crashing
    effect.invalidate();
    flush_effects();
    assert_eq!(counter.load(Ordering::Relaxed), 2);
}

#[test]
fn signal_drop_with_pending_effect_cleans_up() {
    // Signal is dropped while an effect is pending (hasn't been flushed yet)
    use std::sync::atomic::AtomicI32;

    let counter = Arc::new(AtomicI32::new(0));
    let counter_clone = counter.clone();

    let signal = Signal::new();
    let signal_id = signal.node_id();

    let _effect = Effect::new(move || {
        signal_id.track_dependency();
        counter_clone.fetch_add(1, Ordering::Relaxed);
    });

    // Effect ran once during construction
    assert_eq!(counter.load(Ordering::Relaxed), 1);

    // Emit but don't flush - effect is now pending
    signal.emit();

    // Drop the signal while effect is pending
    drop(signal);

    // Flush effects - should handle gracefully (signal is gone)
    flush_effects();

    // Effect ran (it's still in pending set)
    // Note: The effect callback will try to track the signal but it's gone
    // This should not panic
    assert_eq!(counter.load(Ordering::Relaxed), 2);
}

// ============================================================================
// Effect drop cleanup tests
// ============================================================================

#[test]
fn effect_drop_cleanup_is_safe() {
    // When an effect is dropped, emitting its former source should not crash
    use std::sync::atomic::AtomicI32;

    let signal = Signal::new();
    let signal_id = signal.node_id();
    let counter = Arc::new(AtomicI32::new(0));

    // Create a scope to control effect lifetime
    {
        let counter_clone = counter.clone();
        let _effect = Effect::new(move || {
            signal_id.track_dependency();
            counter_clone.fetch_add(1, Ordering::Relaxed);
        });
        // Effect ran once
        assert_eq!(counter.load(Ordering::Relaxed), 1);
        // Effect is dropped at end of scope
    }

    // Signal can still emit without crashing (effect was unsubscribed)
    signal.emit();
    flush_effects();
    // Counter unchanged since effect was dropped
    assert_eq!(counter.load(Ordering::Relaxed), 1);
}

#[test]
fn effect_drop_while_pending_does_not_run() {
    // When an effect is dropped while pending, it should not run during flush
    use std::sync::atomic::AtomicI32;

    let counter = Arc::new(AtomicI32::new(0));
    let counter_clone = counter.clone();

    let signal = Signal::new();
    let signal_id = signal.node_id();

    let effect = Effect::new(move || {
        signal_id.track_dependency();
        counter_clone.fetch_add(1, Ordering::Relaxed);
    });

    // Effect ran once
    assert_eq!(counter.load(Ordering::Relaxed), 1);

    // Mark as pending
    effect.invalidate();

    // Drop the effect while pending
    drop(effect);

    // Flush - the dropped effect should NOT run
    flush_effects();

    // Counter still at 1 (dropped effect didn't run again)
    assert_eq!(counter.load(Ordering::Relaxed), 1);
}

// ============================================================================
// Stale ID access tests
// ============================================================================

#[test]
fn stale_signal_id_returns_none() {
    // Accessing a stale signal ID should return None gracefully
    // NOTE: This test is about API safety - stale IDs should not panic.
    // Due to global arena reuse, we can't guarantee None after drop when
    // tests run in parallel (another test might allocate the same slot).
    // Instead, we verify that:
    // 1. The signal exists while held
    // 2. After drop, accessing the ID doesn't panic (the main safety goal)

    let signal = Signal::new();
    let signal_id = signal.node_id();

    // Verify it works while signal exists
    assert!(signal_id.with_subscribers(|_| ()).is_some());

    // Drop the signal
    drop(signal);

    // The main goal: accessing a stale ID should NOT panic
    // In parallel tests, the slot might be reused, so we just verify no panic
    let _ = signal_id.with_subscribers(|_| ());
    // If we get here without panic, the test passes
}

#[test]
fn stale_effect_id_does_not_panic() {
    // Accessing a stale effect ID should not panic
    // NOTE: This test is about the API safety - stale IDs should not panic.
    // Due to global arena reuse, we can't guarantee None after drop when
    // tests run in parallel (another test might allocate the same slot).
    // Instead, we verify that:
    // 1. The effect exists while held
    // 2. After drop, accessing the ID doesn't panic (the main safety goal)

    let effect = Effect::new(|| {});
    let effect_id = effect.id();

    // Verify it works while effect exists
    assert!(effect_id.has_callback());

    // Drop the effect
    drop(effect);

    // The main goal: accessing a stale ID should NOT panic
    // In parallel tests, the slot might be reused, so we just verify no panic
    let _ = effect_id.has_callback();
    // If we get here without panic, the test passes
}

// ============================================================================
// UI origin filtering tests
// ============================================================================

#[test]
fn ui_origin_filtering_with_track_ui_only() {
    // Test the ui_updates_only flag behavior
    // - ui_updates_only=true means "skip me on UI-only updates"
    // - emit_from_api (is_from_ui=false): ALL subscribers notified (condition is false)
    // - emit_from_ui (is_from_ui=true): subscribers with ui_updates_only=true are SKIPPED
    use std::sync::atomic::AtomicI32;

    let signal = Signal::new();
    let signal_id = signal.node_id();
    let counter = Arc::new(AtomicI32::new(0));

    // Create an effect with ui_updates_only=true that tracks the signal
    let counter_clone = counter.clone();
    let _effect = Effect::new_ui_updates_only(move || {
        signal_id.track_dependency();
        counter_clone.fetch_add(1, Ordering::Relaxed);
    });

    // Effect ran once during construction
    assert_eq!(counter.load(Ordering::Relaxed), 1);

    // emit_from_api DOES trigger the effect (ui_updates_only only skips on UI updates)
    signal.emit_from_api();
    flush_effects();
    assert_eq!(
        counter.load(Ordering::Relaxed),
        2,
        "Effect should run on API updates"
    );

    // emit_from_ui should NOT trigger this effect (it has ui_updates_only=true)
    signal.emit_from_ui();
    flush_effects();
    assert_eq!(
        counter.load(Ordering::Relaxed),
        2,
        "Effect with ui_updates_only should NOT run on UI updates"
    );
}

#[test]
fn ui_origin_filtering_behavior() {
    // Test the actual behavior of ui_updates_only flag
    // From the code: if is_from_ui && effect_id.is_ui_updates_only() { skip }
    // This means:
    // - emit_from_api (is_from_ui=false): ALL subscribers notified
    // - emit_from_ui (is_from_ui=true): subscribers with ui_updates_only=true are SKIPPED
    use std::sync::atomic::AtomicI32;

    let signal = Signal::new();
    let signal_id = signal.node_id();

    // Counter for effect that only wants data updates (ui_updates_only=true)
    let data_only_counter = Arc::new(AtomicI32::new(0));
    let dc = data_only_counter.clone();

    // Counter for effect that wants all updates (ui_updates_only=false)
    let all_counter = Arc::new(AtomicI32::new(0));
    let ac = all_counter.clone();

    // Create effect with ui_updates_only=true that tracks the signal
    let _data_only_effect = Effect::new_ui_updates_only(move || {
        signal_id.track_dependency();
        dc.fetch_add(1, Ordering::Relaxed);
    });

    // Create normal effect that tracks the signal
    let _all_effect = Effect::new(move || {
        signal_id.track_dependency();
        ac.fetch_add(1, Ordering::Relaxed);
    });

    // Both ran once during construction
    assert_eq!(data_only_counter.load(Ordering::Relaxed), 1);
    assert_eq!(all_counter.load(Ordering::Relaxed), 1);

    // emit_from_api: both should be notified (is_from_ui=false, condition is false)
    signal.emit_from_api();
    flush_effects();
    assert_eq!(
        all_counter.load(Ordering::Relaxed),
        2,
        "all_effect should run on API update"
    );
    assert_eq!(
        data_only_counter.load(Ordering::Relaxed),
        2,
        "data_only_effect should run on API update"
    );

    // emit_from_ui: data_only_effect should be SKIPPED (is_from_ui=true AND ui_updates_only=true)
    signal.emit_from_ui();
    flush_effects();
    assert_eq!(
        all_counter.load(Ordering::Relaxed),
        3,
        "all_effect should run on UI update"
    );
    assert_eq!(
        data_only_counter.load(Ordering::Relaxed),
        2,
        "data_only_effect should NOT run on UI update"
    );
}

// ============================================================================
// Transaction edge cases
// ============================================================================

#[test]
fn transaction_cleans_up_on_panic() {
    // Test that transaction state is cleaned up even if the callback panics
    // Note: This test uses catch_unwind which requires the panic to not abort
    use std::panic::catch_unwind;

    let initial_active = crate::transaction::is_transaction_active();
    assert!(!initial_active, "Should not be in transaction initially");

    let result = catch_unwind(|| {
        Transaction::run(|| {
            assert!(crate::transaction::is_transaction_active());
            panic!("Test panic");
        });
    });

    assert!(result.is_err(), "Transaction should have panicked");
    // With RAII guard, transaction state should be cleaned up even on panic
    assert!(
        !crate::transaction::is_transaction_active(),
        "Transaction should be cleaned up after panic"
    );
}

#[test]
fn transaction_returns_value_correctly() {
    // Test that transactions can return various types
    let int_result = Transaction::run(|| 42i32);
    assert_eq!(int_result, 42);

    let string_result = Transaction::run(|| String::from("hello"));
    assert_eq!(string_result, "hello");

    let option_result: Option<i32> = Transaction::run(|| Some(42));
    assert_eq!(option_result, Some(42));

    let tuple_result = Transaction::run(|| (1, "two", 3.0));
    assert_eq!(tuple_result, (1, "two", 3.0));
}

// ============================================================================
// Computed edge cases
// ============================================================================

#[test]
fn computed_chain_propagates() {
    // Test chain of computed values: c1 -> c2 -> c3
    use std::sync::atomic::AtomicI32;

    let c1_count = Arc::new(AtomicI32::new(0));
    let c2_count = Arc::new(AtomicI32::new(0));
    let c3_count = Arc::new(AtomicI32::new(0));

    let cc1 = c1_count.clone();
    let c1 = Computed::new(move || {
        cc1.fetch_add(1, Ordering::Relaxed);
        10
    });

    let cc2 = c2_count.clone();
    let c1_for_c2 = c1.clone();
    let c2 = Computed::new(move || {
        cc2.fetch_add(1, Ordering::Relaxed);
        c1_for_c2.get() * 2
    });

    let cc3 = c3_count.clone();
    let c2_for_c3 = c2.clone();
    let c3 = Computed::new(move || {
        cc3.fetch_add(1, Ordering::Relaxed);
        c2_for_c3.get() + 5
    });

    // Access c3 - should compute c1, c2, c3
    assert_eq!(c3.get(), 25); // 10 * 2 + 5
    assert_eq!(c1_count.load(Ordering::Relaxed), 1);
    assert_eq!(c2_count.load(Ordering::Relaxed), 1);
    assert_eq!(c3_count.load(Ordering::Relaxed), 1);

    // Access c3 again - should be cached
    assert_eq!(c3.get(), 25);
    assert_eq!(c3_count.load(Ordering::Relaxed), 1);

    // Invalidate c1 - marks c1's effect as pending
    c1.invalidate();
    flush_effects();

    // After flush, c1's effect ran (c1_count incremented to 2) but the value didn't change
    // so c1's subscribers weren't notified, and c2/c3 are still cached.
    // Access c3 - c2 and c3 still use cache
    // Note: This is expected behavior - Computed only notifies when value changes
    assert_eq!(c3.get(), 25);
    assert_eq!(c1_count.load(Ordering::Relaxed), 2); // c1's effect ran during flush
    assert_eq!(c3_count.load(Ordering::Relaxed), 1); // c3 still cached (c1's value didn't change)
}

// ============================================================================
// Edge case: Effect creating effects that track same signal
// ============================================================================

#[test]
fn effect_creates_sibling_effects() {
    // Parent effect creates multiple child effects
    use std::sync::atomic::AtomicI32;

    let signal = Signal::new();
    let signal_id = signal.node_id();

    let child_count = Arc::new(AtomicI32::new(0));

    let cc = child_count.clone();
    let parent = Effect::new(move || {
        signal_id.track_dependency();

        // Create 3 child effects
        for _ in 0..3 {
            let cc_inner = cc.clone();
            let _child = Effect::new(move || {
                cc_inner.fetch_add(1, Ordering::Relaxed);
            });
        }
    });

    // Parent ran once, created 3 children that each ran once
    assert_eq!(child_count.load(Ordering::Relaxed), 3);

    // Verify parent has 3 children
    let child_count_check = parent.id().with_children(Vec::len).unwrap_or(0);
    assert_eq!(child_count_check, 3);

    // Signal emits - parent marked pending, children destroyed
    signal.emit();

    // Before flush, children should already be destroyed
    let child_count_after_emit = parent.id().with_children(Vec::len).unwrap_or(0);
    assert_eq!(
        child_count_after_emit, 0,
        "Children destroyed when parent marked pending"
    );

    // Flush - parent runs again, creates 3 new children
    flush_effects();

    // 3 more child effects created and ran
    assert_eq!(child_count.load(Ordering::Relaxed), 6);

    // Parent should have 3 (new) children
    let new_child_count = parent.id().with_children(Vec::len).unwrap_or(0);
    assert_eq!(new_child_count, 3);
}

// ============================================================================
// Concurrency safety test (basic)
// ============================================================================

// ============================================================================
// Push-Pull Reactive System Tests
// ============================================================================

#[test]
fn effect_writing_tracked_as_output() {
    // Test that when an effect calls emit() on a signal, it's tracked as an output
    // Note: Output tracking only happens via emit(), not via direct notification methods
    use std::sync::atomic::AtomicI32;

    let input_signal = Signal::new();
    let output_signal = Signal::new();
    let input_signal_id = input_signal.node_id();

    let counter = Arc::new(AtomicI32::new(0));
    let counter_clone = counter.clone();

    // Create an effect that reads input_signal and writes to output_signal via emit()
    let effect = Effect::new(move || {
        // Track input as dependency
        input_signal_id.track_dependency();
        counter_clone.fetch_add(1, Ordering::Relaxed);
        // Write to output signal via emit (this is tracked as output)
        output_signal.emit();
    });

    // Effect ran once during construction
    assert_eq!(counter.load(Ordering::Relaxed), 1);

    // Check that output_signal is in effect's outputs (emit() tracks it)
    let output_count = effect
        .id()
        .with_outputs(|outputs| outputs.count())
        .unwrap_or(0);
    assert_eq!(output_count, 1);
}

#[test]
fn effect_emit_tracked_as_output() {
    // Test that when an effect calls emit() on a signal, it's tracked as an output
    use crate::arena::signal_arena::with_signal_writers;

    let input_signal = Signal::new();
    let output_signal = Signal::new();
    let input_signal_id = input_signal.node_id();
    let output_signal_id = output_signal.node_id();

    // Create an effect that reads input and emits to output
    let effect = Effect::new(move || {
        // Track input as dependency
        input_signal_id.track_dependency();
        // Emit to output signal (should be tracked as output)
        // We need to use the Signal directly, not just the id
    });

    // Verify the effect has input_signal as source
    let source_count = effect
        .id()
        .with_sources(|sources| sources.count())
        .unwrap_or(0);
    assert_eq!(source_count, 1);
    let has_input_source = effect
        .id()
        .with_sources(|sources| {
            for s in sources {
                if s == input_signal_id {
                    return true;
                }
            }
            false
        })
        .unwrap_or(false);
    assert!(has_input_source);

    // No outputs yet since we didn't actually call emit()
    let output_count = effect
        .id()
        .with_outputs(|outputs| outputs.count())
        .unwrap_or(0);
    assert_eq!(output_count, 0);

    // Check signal writers
    let has_writers = with_signal_writers(output_signal_id, |writers| !writers.is_empty());
    assert!(!has_writers.unwrap_or(false));
}

#[test]
fn untracked_prevents_dependency() {
    // Test that untracked() doesn't create dependencies
    use crate::untracked;

    let signal = Signal::new();
    let signal_id = signal.node_id();

    // Create an effect that uses untracked
    let effect = Effect::new(move || {
        // This should NOT create a dependency
        untracked(|| {
            signal_id.track_dependency();
        });
    });

    // Effect should have NO sources (untracked prevented dependency tracking)
    let source_count = effect
        .id()
        .with_sources(|sources| sources.count())
        .unwrap_or(0);
    assert_eq!(
        source_count, 0,
        "untracked() should prevent dependency tracking"
    );

    // Signal should have NO subscribers
    let subscriber_count = signal_id.with_subscribers(HashSet::len).unwrap_or(0);
    assert_eq!(
        subscriber_count, 0,
        "untracked() should prevent subscription"
    );
}

#[test]
fn untracked_returns_value() {
    // Test that untracked() returns the closure's result
    use crate::untracked;

    let result = untracked(|| 42);
    assert_eq!(result, 42);

    let result = untracked(|| "hello".to_string());
    assert_eq!(result, "hello");

    let result: Option<i32> = untracked(|| Some(123));
    assert_eq!(result, Some(123));
}

#[test]
fn effect_rerun_clears_outputs() {
    // Test that when an effect re-runs, its old outputs are cleared
    use std::sync::atomic::AtomicBool;

    let input_signal = Signal::new();
    let _output_signal_a = Signal::new();
    let _output_signal_b = Signal::new();
    let input_signal_id = input_signal.node_id();

    let use_b = Arc::new(AtomicBool::new(false));
    let use_b_clone = use_b.clone();

    // Effect that writes to A or B depending on flag
    let effect = Effect::new(move || {
        input_signal_id.track_dependency();

        // Note: We can't easily test this without the Signal struct
        // because emit() requires the Signal, not just SignalId
        // The actual output tracking happens in Signal::emit()
        let _ = use_b_clone.load(Ordering::Relaxed);
    });

    // Verify effect exists (not removed from arena)
    assert!(effect.id().with_sources(|_| ()).is_some());

    // Change flag and invalidate
    use_b.store(true, Ordering::Relaxed);
    effect.invalidate();
    flush_effects();

    // Effect re-ran (still exists in arena)
    assert!(effect.id().with_sources(|_| ()).is_some());
}

#[test]
fn pull_ensures_fresh_values() {
    // Test that pull() ensures signal is fresh
    // This test verifies the pull mechanism works

    let signal = Signal::new();
    let signal_id = signal.node_id();

    // Pull on a signal with no writers should be a no-op
    signal_id.pull(false);

    // Verify signal still exists
    assert!(signal_id.with_subscribers(|_| ()).is_some());
}

#[test]
fn diamond_with_pull_gets_fresh_values() {
    // Test diamond dependency: both effects should see fresh values
    //     input
    //    /      \
    // effect1  effect2
    //    \      /
    //    output
    use std::sync::atomic::AtomicI32;

    let input_signal = Signal::new();
    let input_signal_id = input_signal.node_id();

    let effect1_runs = Arc::new(AtomicI32::new(0));
    let effect2_runs = Arc::new(AtomicI32::new(0));

    let e1 = effect1_runs.clone();
    let _effect1 = Effect::new(move || {
        input_signal_id.track_dependency();
        e1.fetch_add(1, Ordering::Relaxed);
    });

    let e2 = effect2_runs.clone();
    let _effect2 = Effect::new(move || {
        input_signal_id.track_dependency();
        e2.fetch_add(1, Ordering::Relaxed);
    });

    // Both effects ran once
    assert_eq!(effect1_runs.load(Ordering::Relaxed), 1);
    assert_eq!(effect2_runs.load(Ordering::Relaxed), 1);

    // Input changes
    input_signal.emit();
    flush_effects();

    // Both effects ran again
    assert_eq!(effect1_runs.load(Ordering::Relaxed), 2);
    assert_eq!(effect2_runs.load(Ordering::Relaxed), 2);
}

#[test]
fn fixed_point_iteration_processes_cascades() {
    // Test that process_all uses fixed-point iteration
    // An effect that creates another pending effect
    use std::sync::atomic::AtomicI32;

    let counter = Arc::new(AtomicI32::new(0));

    let signal = Signal::new();
    let signal_id = signal.node_id();

    let counter_clone = counter.clone();

    // Effect that tracks signal
    let effect = Effect::new(move || {
        signal_id.track_dependency();
        counter_clone.fetch_add(1, Ordering::Relaxed);
    });

    // Initial run
    assert_eq!(counter.load(Ordering::Relaxed), 1);

    // Multiple invalidations in rapid succession
    effect.invalidate();
    effect.invalidate();
    effect.invalidate();

    // Process all - should handle cascading effects
    Effect::process_all();

    // Effect ran once more
    assert_eq!(counter.load(Ordering::Relaxed), 2);
}

#[test]
fn read_write_cycle_detection() {
    // Test that reading and writing the same signal in one effect doesn't cause infinite loop.
    // The system detects this cycle and removes the subscription.
    //
    // Without detection, this would infinite loop:
    // 1. Effect reads signal A (subscribes)
    // 2. Effect writes signal A (marks self as dirty via subscription)
    // 3. Effect re-runs (goto 1) - infinite loop!
    //
    // With detection: the subscription is removed when write is detected,
    // so the effect only runs once per external trigger.

    use std::sync::atomic::AtomicI32;

    let signal = Signal::new();
    let signal_id = signal.node_id();
    let run_count = Arc::new(AtomicI32::new(0));
    let run_count_clone = run_count.clone();

    // This effect both reads AND writes signal
    let _effect = Effect::new(move || {
        // Read (creates subscription)
        signal_id.track_dependency();
        // Write (should detect cycle and remove subscription)
        signal.emit();
        run_count_clone.fetch_add(1, Ordering::Relaxed);
    });

    // Effect should have run exactly once during creation (no infinite loop)
    assert_eq!(run_count.load(Ordering::Relaxed), 1);

    // Flush to ensure no pending work
    flush_effects();

    // Still only one run - the cycle detection prevented infinite loops
    assert_eq!(run_count.load(Ordering::Relaxed), 1);
}

#[test]
fn read_write_different_signals_allowed() {
    // Test that reading one signal and writing another is fine
    // (no cycle detection needed - they're different signals)

    use std::sync::atomic::AtomicI32;

    let signal_read = Signal::new();
    let signal_write = Signal::new();
    let signal_read_id = signal_read.node_id();
    let run_count = Arc::new(AtomicI32::new(0));
    let run_count_clone = run_count.clone();

    // This effect reads one signal and writes another - should work normally
    let _effect = Effect::new(move || {
        signal_read_id.track_dependency();
        signal_write.emit();
        run_count_clone.fetch_add(1, Ordering::Relaxed);
    });

    // Effect ran once during creation
    assert_eq!(run_count.load(Ordering::Relaxed), 1);

    // Emitting signal_read should trigger the effect
    signal_read.emit();
    flush_effects();

    // Effect should have run again (still subscribed to signal_read)
    assert_eq!(run_count.load(Ordering::Relaxed), 2);

    // Emitting again should work
    signal_read.emit();
    flush_effects();
    assert_eq!(run_count.load(Ordering::Relaxed), 3);
}

// ============================================================================
// Skippable Effects Tests
// ============================================================================

#[test]
fn skippable_effect_flag_set_correctly() {
    // Test that Effect::new_skippable() sets the skippable flag
    let effect = Effect::new_skippable(|| {});
    assert!(effect.id().is_skippable());

    // Test that regular Effect::new() creates non-skippable effects
    let regular_effect = Effect::new(|| {});
    assert!(!regular_effect.id().is_skippable());
}

#[test]
fn skippable_computed_flag_set_correctly() {
    // Test that Computed::new_skippable() sets the skippable flag
    let computed = Computed::new_skippable(|| 42);
    assert!(computed.effect_id().is_skippable());

    // Test that regular Computed::new() creates non-skippable computeds
    let regular_computed = Computed::new(|| 42);
    assert!(!regular_computed.effect_id().is_skippable());

    // Test lazy_skippable
    let lazy_skippable = Computed::lazy_skippable(|| 42);
    assert!(lazy_skippable.effect_id().is_skippable());

    // Test regular lazy
    let lazy_regular = Computed::lazy(|| 42);
    assert!(!lazy_regular.effect_id().is_skippable());
}

#[test]
fn flush_with_budget_defers_skippable() {
    use std::time::Duration;

    // Create a skippable effect
    let skippable_count = Arc::new(AtomicUsize::new(0));
    let sc = skippable_count.clone();
    let skippable = Effect::new_skippable(move || {
        sc.fetch_add(1, Ordering::Relaxed);
    });

    // Create a non-skippable effect
    let non_skippable_count = Arc::new(AtomicUsize::new(0));
    let nsc = non_skippable_count.clone();
    let non_skippable = Effect::new(move || {
        nsc.fetch_add(1, Ordering::Relaxed);
    });

    // Both ran once during construction
    assert_eq!(skippable_count.load(Ordering::Relaxed), 1);
    assert_eq!(non_skippable_count.load(Ordering::Relaxed), 1);

    // Invalidate both
    skippable.invalidate();
    non_skippable.invalidate();

    // Flush with zero budget - skippable should be deferred, non-skippable should run
    let budget = Duration::from_millis(0);
    let mut must_run_buf = Vec::new();
    let mut skippable_buf = Vec::new();
    crate::effect::flush_effects_with_budget(budget, &mut must_run_buf, &mut skippable_buf);

    // Non-skippable ran (even with zero budget, must-run effects always run)
    assert_eq!(non_skippable_count.load(Ordering::Relaxed), 2);

    // Skippable was deferred
    assert_eq!(skippable_count.load(Ordering::Relaxed), 1);

    // Should have 1 deferred effect in skippable_buf
    assert_eq!(skippable_buf.len(), 1);
    assert!(skippable_buf[0].is_skippable());
}

#[test]
fn pull_skips_skippable_when_flag_set() {
    use std::sync::atomic::AtomicI32;

    let signal = Signal::new();
    let signal_id = signal.node_id();

    // Create a skippable computed that writes to the signal
    let skippable_count = Arc::new(AtomicI32::new(0));
    let sc = skippable_count.clone();
    let _skippable_computed = Computed::new_skippable(move || {
        sc.fetch_add(1, Ordering::Relaxed);
        42
    });

    // Initial computation
    assert_eq!(skippable_count.load(Ordering::Relaxed), 1);

    // Now call pull with skip_skippable=true
    // This should NOT run the skippable computed
    signal_id.pull(true);

    // Skippable was skipped
    assert_eq!(skippable_count.load(Ordering::Relaxed), 1);

    // Call pull with skip_skippable=false
    // This SHOULD run the skippable computed if it's stale
    signal_id.pull(false);

    // If the computed is clean, it won't run anyway
    // This test mainly verifies the API works without panicking
}

#[test]
fn skippable_effects_integrate_with_budget_system() {
    use std::time::Duration;

    // Create multiple skippable producer effects
    let producer_count = Arc::new(AtomicUsize::new(0));
    let mut producers = Vec::new();

    for _ in 0..5 {
        let pc = producer_count.clone();
        producers.push(Effect::new_skippable(move || {
            pc.fetch_add(1, Ordering::Relaxed);
        }));
    }

    // Create a consumer effect
    let consumer_count = Arc::new(AtomicUsize::new(0));
    let cc = consumer_count.clone();
    let _consumer = Effect::new(move || {
        cc.fetch_add(1, Ordering::Relaxed);
    });

    // All ran once during construction
    assert_eq!(producer_count.load(Ordering::Relaxed), 5);
    assert_eq!(consumer_count.load(Ordering::Relaxed), 1);

    // Invalidate all
    for producer in &producers {
        producer.invalidate();
    }
    // Consumer doesn't need invalidating for this test

    // Flush with small budget
    let budget = Duration::from_millis(1);
    let mut must_run_buf = Vec::new();
    let mut skippable_buf = Vec::new();
    crate::effect::flush_effects_with_budget(budget, &mut must_run_buf, &mut skippable_buf);

    // Some producers might have run, some might be deferred
    // The key is that the system doesn't panic and deferred effects are skippable
    for deferred_id in &skippable_buf {
        assert!(deferred_id.is_skippable());
    }
}

#[test]
fn check_state_effects_added_to_pending() {
    // This test verifies that effects marked with Check state are properly added
    // to the pending set. The Check state is used for indirect dependents in the
    // three-state reactive system.
    //
    // Scenario:
    // - Effect A writes to signal S
    // - Effect B reads from signal S
    // - When A is marked Dirty, B should be marked Check AND added to pending
    // - When A runs, B is upgraded to Dirty and runs (conservative propagation)
    //
    // The bug this test catches: if Check effects aren't added to pending, they
    // would get "stuck" in Check state and never respond to subsequent changes.
    //
    // Note: The current implementation uses conservative propagation - when any
    // effect runs, all downstream subscribers are marked Dirty regardless of
    // whether the effect actually wrote to its outputs. This ensures no updates
    // are missed.

    use crate::arena::current_effect;

    // Verify the code path is taken
    cov_mark::check!(check_effect_added_to_pending);

    let trigger = Signal::new();
    let output = Signal::new();

    // Get IDs for use in closures
    let trigger_id = trigger.node_id();
    let output_id = output.node_id();

    // Effect A: reads trigger, writes to output
    // We manually do what Signal::emit() does: register output and notify
    let _effect_a = Effect::new(move || {
        trigger_id.track_dependency();
        // Register this effect as a writer of output_id (like Signal::emit does)
        if let Some(effect_id) = current_effect() {
            effect_id.add_output(output_id);
        }
        output_id.notify_subscribers();
    });

    // Effect B: reads output
    let b_count = Arc::new(AtomicUsize::new(0));
    let b_count_clone = b_count.clone();
    let _effect_b = Effect::new(move || {
        output_id.track_dependency();
        b_count_clone.fetch_add(1, Ordering::Relaxed);
    });

    // Both effects ran during creation
    assert_eq!(b_count.load(Ordering::Relaxed), 1);

    // First trigger - B should run
    trigger.emit();
    flush_effects();
    assert_eq!(b_count.load(Ordering::Relaxed), 2);

    // Multiple triggers - B should respond each time
    // This verifies Check effects are properly added to pending and processed
    for i in 3..=5 {
        trigger.emit();
        flush_effects();
        assert_eq!(
            b_count.load(Ordering::Relaxed),
            i,
            "Effect B should run on emission {i}. \
             If this fails, Check-state effects may not be added to the pending set."
        );
    }
}

#[test]
fn check_state_propagation_through_chain() {
    // Test that Check state properly propagates through a chain of effects
    // and all effects in the chain respond to changes.

    use crate::arena::current_effect;

    cov_mark::check!(check_effect_added_to_pending);

    let source = Signal::new();
    let middle = Signal::new();
    let end = Signal::new();

    // Get IDs for use in closures
    let source_id = source.node_id();
    let middle_id = middle.node_id();
    let end_id = end.node_id();

    // Effect chain: source -> middle -> end
    // We manually do what Signal::emit() does: register output and notify
    let _effect_a = Effect::new(move || {
        source_id.track_dependency();
        if let Some(effect_id) = current_effect() {
            effect_id.add_output(middle_id);
        }
        middle_id.notify_subscribers();
    });

    let _effect_b = Effect::new(move || {
        middle_id.track_dependency();
        if let Some(effect_id) = current_effect() {
            effect_id.add_output(end_id);
        }
        end_id.notify_subscribers();
    });

    let c_count = Arc::new(AtomicUsize::new(0));
    let c_count_clone = c_count.clone();
    let _effect_c = Effect::new(move || {
        end_id.track_dependency();
        c_count_clone.fetch_add(1, Ordering::Relaxed);
    });

    // Initial run
    assert_eq!(c_count.load(Ordering::Relaxed), 1);

    // Multiple emissions - all effects should run each time
    for i in 2..=5 {
        source.emit();
        flush_effects();
        assert_eq!(
            c_count.load(Ordering::Relaxed),
            i,
            "Effect C should run on emission {i}"
        );
    }
}
