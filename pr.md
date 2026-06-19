# feat(gossip): QoS message priority with relay-proximity weighting (#103)

## Summary

Implements the full QoS message prioritisation spec from issue #103.

- **Urgency scoring** — `MessagePriority::urgency_score()` assigns a numeric priority to each `TransactionEnvelope` based on two signals: remaining TTL (lower TTL → higher score, so near-expiry envelopes are forwarded first) and age (older envelopes gain up to 10 extra points, capped at 10 minutes). Non-transaction messages score 0 because they are already dispatched through the High tier.

- **Priority-aware batch drain** — `PriorityQueue::drain_batch(n)` replaces the FIFO pop loop in `execute_fanout_round`. It drains the entire Normal tier, sorts by urgency score descending, returns the top `n` messages, and retains the rest for the next round. Within a single BLE gossip round the most urgent transactions are always forwarded first.

- **Low-tier routing for distant peers** — `MessagePriority::for_topology_update(hops_to_relay)` demotes `TopologyUpdate` messages from peers more than 5 hops from a relay to the Low tier. These updates are less valuable for routing decisions and should not displace urgent transactions on constrained BLE links. `for_message()` now delegates to this constructor, activating the previously-unused Low tier.

- **Per-tier capacity caps** — Normal tier is capped at 10 000 envelopes; Low tier at 500 messages. On overflow the oldest entry is evicted and a `warn!` is emitted. `PriorityQueue::push()` now returns `Option<ProtocolMessage>` (the dropped message if any), and `GossipState::add_envelope()` surfaces `GossipError::NormalQueueOverflow` to callers.

## Files changed

| File | Change |
|------|--------|
| `src/gossip/queue.rs` | `urgency_score`, `for_topology_update`, `drain_batch`, per-tier caps, 4 new tests |
| `src/gossip/protocol.rs` | Use `drain_batch` in `execute_fanout_round`; propagate `NormalQueueOverflow` from `add_envelope` |

## Tests

All four required acceptance tests are in `src/gossip/queue.rs::tests`:

| Test | Assertion |
|------|-----------|
| `test_urgency_score_increases_with_low_ttl` | `score(TTL=1) > score(TTL=15)` |
| `test_drain_batch_sorted_by_urgency` | TTLs `[10,2,8,1,5]` drain as `[1,2,5,8,10]` |
| `test_topology_update_uses_low_tier_for_far_peers` | `TopologyUpdate(hops=8)` is popped after a Normal-tier transaction |
| `test_normal_tier_cap_drops_oldest` | After 10 001 pushes, `queue.normal.len() == 10_000` |

`cargo test --lib` — **180 passed, 0 failed**.

## Test plan

- [ ] `cargo test --lib gossip::queue` passes (5 tests including 4 new ones)
- [ ] `cargo test --lib` passes (180 tests, no regressions)
- [ ] Manually push envelopes with mixed TTLs and confirm log output shows urgency-ordered forwarding
- [ ] Push a `TopologyUpdate` with `hops_to_relay > 5` and confirm it appears in the Low tier (after Normal-tier transactions in `pop()` order)
- [ ] Push 10 001 envelopes and confirm `warn!` log fires and queue length stays at 10 000

🤖 Generated with [Claude Code](https://claude.com/claude-code)
