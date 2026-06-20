# feat(observability): implement protocol-level metrics registry (#100)

## Summary

- **Creates `src/metrics.rs`** with a global `Metrics` struct holding 21 `AtomicU64` counters across gossip, rate-limiting, anti-entropy, routing, peer management, and relay domains. Includes `MetricsSnapshot` (useful for before/after test assertions) and `Metrics::log_summary()` which emits all counters at `info!` level.

- **Wires `Arc<Metrics>` into every integration point** specified by the issue:
  - `execute_fanout_round` / `run_gossip_loop` — tracks `gossip_rounds_fired`, `envelopes_forwarded`, `envelopes_dropped_ttl`, `envelopes_dropped_bloom`, anti-entropy sync counters, and `peers_unbanned`.
  - `process_transaction_envelope` — tracks `peers_banned`.
  - `src/security/rate_limit.rs` (new) — `RateLimiter` increments `messages_rate_limited` and `peer_violations_recorded` on rejection.
  - `src/router/table.rs` (new) — `RoutingTable` increments `routing_table_hits`, `routing_table_misses`, `routing_table_refreshes` via an LRU cache.
  - `src/relay/mod.rs` — `RelayNode` increments `transactions_submitted` / `transactions_rejected` in `process_envelope`.

- **Periodic summary logging**: every 100 gossip rounds, `metrics.log_summary()` fires via `rounds.is_multiple_of(100)`.

- **Exposes the router module**: `pub mod router` added to `src/lib.rs`; dead code in `relay_router.rs` cleaned up to satisfy clippy `-D warnings`.

## Files changed

| File | Change |
|---|---|
| `src/metrics.rs` | **New** — `Metrics`, `MetricsSnapshot`, `log_summary()` |
| `src/lib.rs` | Add `pub mod metrics; pub mod router;` |
| `src/gossip/mod.rs` | Remove re-export of deleted `GossipLoopMetrics` |
| `src/gossip/protocol.rs` | Replace `GossipLoopMetrics` with `Arc<Metrics>`; add 3 acceptance-criteria tests |
| `src/security/rate_limit.rs` | **New** — `RateLimiter` with per-peer sliding window + metrics |
| `src/security/mod.rs` | Add `pub mod rate_limit;` |
| `src/router/table.rs` | **New** — `RoutingTable` (LRU) with hit/miss/refresh metrics |
| `src/router/mod.rs` | Add `pub mod table;` |
| `src/router/relay_router.rs` | Remove dead `path_finder` field and unused imports |
| `src/relay/mod.rs` | Add `metrics: Arc<Metrics>` to `RelayNode`; track submitted/rejected |
| `tests/peer_banning_test.rs` | Pass `&metrics` to `process_transaction_envelope` |
| `tests/relay_test.rs` | Pass `Metrics::new()` to `RelayNode::new` |
| `tests/integration/double_spend_simulation_test.rs` | Pass `Metrics::new()` to `RelayNode::new` |
| `examples/relay_node_submission.rs` | Pass `Metrics::new()` to `RelayNode::new` |

## Acceptance criteria

- [x] `src/metrics.rs` exists and is declared in `src/lib.rs`
- [x] All 21 counters increment correctly in their respective modules
- [x] `Metrics::log_summary()` emits a readable summary at `info!` level every 100 gossip rounds
- [x] `MetricsSnapshot` can be created and compared in tests

## Required tests — all passing

| Test | Location |
|---|---|
| `test_gossip_metrics_rounds_accumulate` | `src/gossip/protocol.rs` |
| `test_metrics_envelopes_forwarded` | `src/gossip/protocol.rs` |
| `test_metrics_envelopes_dropped_ttl` | `src/gossip/protocol.rs` |
| `test_metrics_rate_limit_counter` | `src/security/rate_limit.rs` |

## Test plan

- [x] `cargo fmt --all -- --check` passes
- [x] `cargo clippy --all-targets --all-features -- -D warnings` passes (0 errors, 0 warnings)
- [x] `cargo test --lib` — 201 tests pass
- [x] `cargo test --workspace` — all integration tests pass
