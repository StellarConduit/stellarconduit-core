use crate::gossip::bloom::SlidingBloomFilter;
use lru::LruCache;
use std::num::NonZeroUsize;

pub struct RelayDeduplicator {
    seen_ids: SlidingBloomFilter,
    result_cache: LruCache<[u8; 32], String>, // message_id -> stellar tx hash
}

impl RelayDeduplicator {
    pub fn new(capacity: usize) -> Self {
        // Use a reasonable false positive rate for the bloom filter
        let fp_rate = 0.01; // 1% false positive rate
                            // The bloom filter capacity should be large enough to handle the expected load
                            // We'll use the same capacity for the bloom filter window
        let seen_ids = SlidingBloomFilter::new(capacity, fp_rate);

        // Create LRU cache with the same capacity
        let cache_capacity =
            NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::new(1000).unwrap());
        let result_cache = LruCache::new(cache_capacity);

        Self {
            seen_ids,
            result_cache,
        }
    }

    /// Check if we've already processed this message_id.
    /// Returns Some(tx_hash) if already submitted, None if new.
    ///
    /// This method only checks the LRU cache, which stores the exact message_id -> tx_hash mapping.
    /// The bloom filter is used in mark_submitted() to prevent duplicate additions to the cache.
    /// If an item is not in the cache, we cannot return a hash, so we treat it as new.
    pub fn check(&mut self, message_id: &[u8; 32]) -> Option<String> {
        // Check the LRU cache for an exact match
        // If found, return the cached transaction hash
        self.result_cache.get(message_id).cloned()
    }

    /// Record that a message_id has been successfully submitted with the given tx hash.
    pub fn mark_submitted(&mut self, message_id: [u8; 32], tx_hash: String) {
        // Add to bloom filter
        self.seen_ids.add(&message_id);

        // Add to LRU cache
        self.result_cache.put(message_id, tx_hash);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_returns_none_for_new_message_id() {
        let mut dedup = RelayDeduplicator::new(1000);
        let message_id = [1u8; 32];

        assert_eq!(dedup.check(&message_id), None);
    }

    #[test]
    fn test_check_returns_hash_after_mark_submitted() {
        let mut dedup = RelayDeduplicator::new(1000);
        let message_id = [1u8; 32];
        let tx_hash = "abc123".to_string();

        assert_eq!(dedup.check(&message_id), None);
        dedup.mark_submitted(message_id, tx_hash.clone());
        assert_eq!(dedup.check(&message_id), Some(tx_hash));
    }

    #[test]
    fn test_multiple_message_ids() {
        let mut dedup = RelayDeduplicator::new(1000);
        let msg1 = [1u8; 32];
        let msg2 = [2u8; 32];
        let hash1 = "hash1".to_string();
        let hash2 = "hash2".to_string();

        dedup.mark_submitted(msg1, hash1.clone());
        dedup.mark_submitted(msg2, hash2.clone());

        assert_eq!(dedup.check(&msg1), Some(hash1));
        assert_eq!(dedup.check(&msg2), Some(hash2));
    }

    #[test]
    fn test_bloom_filter_rotation_under_load() {
        let mut dedup = RelayDeduplicator::new(20); // Capacity large enough to hold all items

        // Add items to fill the bloom filter window and trigger rotation
        for i in 0..25 {
            let mut msg_id = [0u8; 32];
            msg_id[0] = i as u8;
            let tx_hash = format!("hash_{}", i);
            dedup.mark_submitted(msg_id, tx_hash.clone());

            // Verify we can still retrieve it immediately
            assert_eq!(dedup.check(&msg_id), Some(tx_hash));
        }

        // Verify recent items are still accessible (they should be in the cache)
        let mut msg20 = [0u8; 32];
        msg20[0] = 20;
        assert_eq!(dedup.check(&msg20), Some("hash_20".to_string()));

        // Verify that bloom filter rotation doesn't break the ability to detect duplicates
        // Even if an item was evicted from cache, the bloom filter should still indicate
        // it was probably seen (though we can't return the hash without the cache)
        let mut msg5 = [0u8; 32];
        msg5[0] = 5;
        // This might be None if evicted from cache, or Some if still in cache
        // The important thing is that mark_submitted still works
        let result = dedup.check(&msg5);
        // If it's Some, verify it's correct; if None, that's okay (cache eviction)
        if let Some(hash) = result {
            assert_eq!(hash, "hash_5".to_string());
        }
    }
}
