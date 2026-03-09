# feat(relay): Implement transaction deduplication at the relay node

## Summary

Implements a relay-side cache to prevent the same `TransactionEnvelope` from being submitted to the Stellar network multiple times. In a mesh network, the same transaction may arrive via multiple different hop paths nearly simultaneously, and without deduplication, this would result in duplicate submissions causing `txBadSeq` errors.

## Problem

Without deduplication, it is very likely that a Relay Node will receive the same transaction via 3 different hop paths at nearly the same time, all within a 200ms window. Naively submitting all three to the Stellar RPC would result in two `txBadSeq` errors and possible confusion about whether the payment went through. The Relay must see-only-once for each unique `message_id`.

## Solution

Implemented a `RelayDeduplicator` that combines:
- **SlidingBloomFilter**: Efficient probabilistic data structure for fast duplicate detection
- **LruCache**: Exact mapping cache to store `message_id → tx_hash` for immediate hash retrieval

The deduplicator is integrated into the `process_envelope()` function, which checks for duplicates before making any RPC calls. If a duplicate is detected, it immediately returns the cached transaction hash without calling the Stellar RPC.

## Changes

### New Files
- `src/relay/dedup.rs`: Implements `RelayDeduplicator` with `check()` and `mark_submitted()` methods
- `src/relay/mod.rs`: Implements `RelayNode` with `process_envelope()` that integrates deduplication

### Modified Files
- `src/gossip/bloom.rs`: Added `check()` and `add()` methods to `SlidingBloomFilter` for separate check/add operations. Fixed test to handle bloom filter false positives.
- `src/lib.rs`: Added `relay` module export

## Implementation Details

### RelayDeduplicator API

```rust
pub struct RelayDeduplicator {
    seen_ids: SlidingBloomFilter,
    result_cache: LruCache<[u8; 32], String>, // message_id -> stellar tx hash
}

impl RelayDeduplicator {
    pub fn new(capacity: usize) -> Self;
    
    /// Check if we've already processed this message_id.
    /// Returns Some(tx_hash) if already submitted, None if new.
    pub fn check(&mut self, message_id: &[u8; 32]) -> Option<String>;
    
    /// Record that a message_id has been successfully submitted with the given tx hash.
    pub fn mark_submitted(&mut self, message_id: [u8; 32], tx_hash: String);
}
```

### Integration Flow

1. When `process_envelope()` is called, it first checks `deduplicator.check(envelope.message_id)`
2. If `Some(hash)` is returned, log and return early without calling RPC
3. If `None`, proceed with `rpc_client.submit_transaction()`
4. On success, call `deduplicator.mark_submitted(...)` to cache the result

## Testing

All acceptance criteria are met with comprehensive unit tests:

- ✅ `check()` returns `None` for a new `message_id`
- ✅ `check()` returns `Some(hash)` after `mark_submitted()` is called
- ✅ Identical envelopes arriving within a 2-second window result in only one HTTP call to the relay RPC mock
- ✅ Unit tests cover the bloom filter rotation scenario under load

### Test Coverage

- `test_check_returns_none_for_new_message_id`: Verifies new messages are not cached
- `test_check_returns_hash_after_mark_submitted`: Verifies caching works correctly
- `test_multiple_message_ids`: Verifies multiple distinct messages are handled independently
- `test_bloom_filter_rotation_under_load`: Verifies bloom filter rotation doesn't break deduplication
- `test_duplicate_submission_returns_cached_hash`: Verifies duplicate detection prevents RPC calls
- `test_multiple_identical_envelopes_within_window`: Verifies multiple simultaneous duplicates are handled
- `test_different_envelopes_submit_separately`: Verifies distinct messages are submitted separately

All 101 tests pass successfully.

## Performance Considerations

- **Bloom Filter**: Provides O(1) probabilistic duplicate detection with configurable false positive rate (default 1%)
- **LRU Cache**: Provides O(1) exact hash lookups with bounded memory usage
- **Memory Efficient**: Uses sliding window approach to handle high-volume scenarios without unbounded growth

## Future Improvements

- Consider adding metrics/logging for cache hit rates and bloom filter false positive rates
- Consider configurable cache capacity and bloom filter parameters
- Consider persistence for cache across restarts (though this may not be necessary for the use case)
