use bloomfilter::Bloom;

pub struct MessageFilter {
    filter: Bloom<[u8; 32]>,
}

impl MessageFilter {
    /// Create a new filter optimized for `capacity` items with `false_positive_rate`
    pub fn new(capacity: usize, false_positive_rate: f64) -> Self {
        Self {
            filter: Bloom::new_for_fp_rate(capacity, false_positive_rate),
        }
    }

    /// Returns true if the message is PROBABLY already seen.
    /// Returns false if the message is DEFINITELY new.
    pub fn check_and_add(&mut self, message_id: &[u8; 32]) -> bool {
        if self.filter.check(message_id) {
            true
        } else {
            self.filter.set(message_id);
            false
        }
    }
}

pub struct SlidingBloomFilter {
    current: Bloom<[u8; 32]>,
    previous: Bloom<[u8; 32]>,
    capacity: usize,
    fp_rate: f64,
    insert_count: usize,
}

impl SlidingBloomFilter {
    pub fn new(capacity_per_window: usize, fp_rate: f64) -> Self {
        Self {
            current: Bloom::new_for_fp_rate(capacity_per_window, fp_rate),
            previous: Bloom::new_for_fp_rate(capacity_per_window, fp_rate),
            capacity: capacity_per_window,
            fp_rate,
            insert_count: 0,
        }
    }

    pub fn check_and_add(&mut self, message_id: &[u8; 32]) -> bool {
        if self.current.check(message_id) || self.previous.check(message_id) {
            true
        } else {
            self.rotate_if_full();
            self.current.set(message_id);
            self.insert_count += 1;
            false
        }
    }

    fn rotate_if_full(&mut self) {
        if self.insert_count >= self.capacity {
            let new_filter = Bloom::new_for_fp_rate(self.capacity, self.fp_rate);
            self.previous = std::mem::replace(&mut self.current, new_filter);
            self.insert_count = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_filter() {
        let mut filter = MessageFilter::new(100, 0.01);
        let msg1 = [1u8; 32];
        let msg2 = [2u8; 32];

        assert_eq!(filter.check_and_add(&msg1), false);
        assert_eq!(filter.check_and_add(&msg1), true);
        assert_eq!(filter.check_and_add(&msg2), false);
        assert_eq!(filter.check_and_add(&msg2), true);
    }

    #[test]
    fn test_sliding_bloom_filter_rotation() {
        let mut filter = SlidingBloomFilter::new(10, 0.01);

        // Add 10 items to fill current filter
        for i in 0..10 {
            let mut msg = [0u8; 32];
            msg[0] = i as u8;
            assert_eq!(filter.check_and_add(&msg), false);
        }

        // Next item should cause rotation
        let mut msg11 = [0u8; 32];
        msg11[0] = 11;
        assert_eq!(filter.check_and_add(&msg11), false);

        // Old items should still be recognized (they are now in previous)
        let mut msg0 = [0u8; 32];
        msg0[0] = 0;
        assert_eq!(filter.check_and_add(&msg0), true);

        // Add 10 more items to cause another rotation
        for i in 12..22 {
            let mut msg = [0u8; 32];
            msg[0] = i as u8;
            assert_eq!(filter.check_and_add(&msg), false);
        }

        // Now msg0 should be forgotten since it was in 'previous' which just got overwritten
        // Note: Bloom filter is probabilistic, but msg0 is DEFINITELY not in current,
        // We'll just verify the filter rotated properly by checking insert_count.
        // It rotated after 11th item (insert_count became 1), then we added 10 more items.
        // wait... 11th item triggered rotation: previous=current(10 items), current=new_filter, insert_count=0 -> 1.
        // then we added 10 more items (12..22 = 10 items). Each adds to current.
        // so insert_count should be 1 + 10 = 11. Wait, let's trace:
        // items 0..10 (10 items) -> insert_count = 10
        // item 11 -> rotation triggers because 10 >= 10. insert_count=0. Then item 11 added, insert_count=1.
        // items 12..21 (10 items) -> item 12 adds, insert_count=2 ... item 20 adds, insert_count=10.
        // item 21 -> rotation triggers because 10 >= 10. insert_count=0. Then item 21 added, insert_count=1.
        // So total items: 0..10 (10 items), 11 (1 item), 12..22 is actually 10 items.
        // Wait, 12..22 is 10 items (12, 13, 14, 15, 16, 17, 18, 19, 20, 21).
        // Let's just remove the explicit insert_count check and verify false positive behavior, or accept the current insert_count.
        // At the end, insert_count is 1.
        assert_eq!(filter.insert_count, 1);
    }

    #[test]
    fn test_false_positive_rate() {
        let mut filter = SlidingBloomFilter::new(1000, 0.05);
        let mut false_positives = 0;

        for i in 0..1000u32 {
            let mut msg = [0u8; 32];
            let bytes = i.to_le_bytes();
            msg[0..4].copy_from_slice(&bytes);
            filter.check_and_add(&msg);
        }

        for i in 1000..2000u32 {
            let mut msg = [0u8; 32];
            let bytes = i.to_le_bytes();
            msg[0..4].copy_from_slice(&bytes);
            if filter.check_and_add(&msg) {
                // Was incorrectly marked as seen (false positive) since it's the first check for this ID
                false_positives += 1;
            }
        }

        let fp_rate = false_positives as f64 / 1000.0;
        assert!(fp_rate <= 0.10, "False positive rate too high: {}", fp_rate);
    }
}
