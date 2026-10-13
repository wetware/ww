use cid::Cid;
use linked_hash_map::LinkedHashMap;
use std::cmp;

/// Counters for cache hits, misses, and evictions.
#[derive(Debug, Clone, Default)]
pub struct ArcStats {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
}

/// Weight-aware Adaptive Replacement Cache (Megiddo & Modha).
///
/// Not thread-safe — wrap in a `Mutex` for concurrent access.
/// Uses byte-weight budgeting instead of entry-count capacity.
///
/// Four lists:
/// - T1: recent entries (seen once)
/// - T2: frequent entries (seen twice or more)
/// - B1: ghost entries evicted from T1 (keys only, no values)
/// - B2: ghost entries evicted from T2 (keys only, no values)
///
/// The parameter `p` (in weight units) controls the split between
/// T1 and T2 during eviction. It adapts based on ghost list hits:
/// - B1 hit → increase p (favor recency, grow T1's share)
/// - B2 hit → decrease p (favor frequency, grow T2's share)
pub struct ArcInner<V> {
    t1: LinkedHashMap<Cid, V>,
    t2: LinkedHashMap<Cid, V>,
    b1: LinkedHashMap<Cid, ()>,
    b2: LinkedHashMap<Cid, ()>,
    /// Adaptive split parameter (weight units). Determines how much
    /// of the budget is "allocated" to T1 vs T2 during replace().
    p: usize,
    /// Current total weight of T1 + T2 entries.
    weight: usize,
    /// Current weight of T1 entries only (tracked to avoid O(n) recomputation).
    t1_weight: usize,
    /// Maximum weight allowed for T1 + T2 combined.
    budget: usize,
    /// Maximum total ghost entries (B1 + B2). Ghost list caps are derived
    /// from this and `p`: B1 ≤ ghost_capacity - p_frac, B2 ≤ p_frac,
    /// where p_frac = p * ghost_capacity / budget (normalizes weight-space
    /// p to count-space for ghost caps).
    ghost_capacity: usize,
    /// Computes the weight of a (key, value) pair in bytes.
    weighter: fn(&Cid, &V) -> usize,
    /// Hit/miss/eviction counters for observability.
    stats: ArcStats,
}

impl<V> ArcInner<V> {
    /// Create a new ARC with the given byte budget, ghost capacity, and weighter function.
    ///
    /// `ghost_capacity` controls the maximum number of ghost entries (B1 + B2).
    /// Ghost entries are CID keys only (~40 bytes each), so even large values
    /// use little memory. The paper caps ghosts at the entry-count capacity;
    /// since our budget is in bytes, this must be set explicitly.
    ///
    /// # Panics
    /// Panics if `budget` is 0 or `ghost_capacity` is 0.
    pub fn new(budget: usize, ghost_capacity: usize, weighter: fn(&Cid, &V) -> usize) -> Self {
        assert!(budget > 0, "ARC budget must be > 0");
        assert!(ghost_capacity > 0, "ARC ghost_capacity must be > 0");
        Self {
            t1: LinkedHashMap::new(),
            t2: LinkedHashMap::new(),
            b1: LinkedHashMap::new(),
            b2: LinkedHashMap::new(),
            p: 0,
            weight: 0,
            t1_weight: 0,
            budget,
            ghost_capacity,
            weighter,
            stats: ArcStats::default(),
        }
    }

    /// Look up a key, promoting it from T1→T2 if found in T1.
    /// Returns `None` on miss.
    pub fn get(&mut self, k: &Cid) -> Option<&V> {
        // Case 1: hit in T1 → promote to T2 MRU
        if self.t1.contains_key(k) {
            let v = self.t1.remove(k).unwrap();
            let w = (self.weighter)(k, &v);
            self.t1_weight -= w;
            self.t2.insert(*k, v);
            self.stats.hits += 1;
            return self.t2.get(k);
        }

        // Case 2: hit in T2 → refresh at MRU (remove + re-insert)
        if self.t2.contains_key(k) {
            let v = self.t2.remove(k).unwrap();
            self.t2.insert(*k, v);
            self.stats.hits += 1;
            return self.t2.get(k);
        }

        // Miss
        self.stats.misses += 1;
        None
    }

    /// Insert or update a key-value pair. Returns evicted (key, value) pairs
    /// that the caller should unpin.
    ///
    /// If the entry's weight exceeds the total budget, it is rejected
    /// and returned in the evicted vec (caller can still fetch without caching).
    pub fn put(&mut self, k: Cid, v: V) -> Vec<(Cid, V)> {
        let entry_weight = (self.weighter)(&k, &v);
        let mut evicted = Vec::new();

        // Reject entries that exceed the entire budget.
        if entry_weight > self.budget {
            evicted.push((k, v));
            return evicted;
        }

        // Case 1: key already in T1 or T2 → update in place, move to T2 MRU.
        if let Some(old) = self.t1.remove(&k) {
            let old_weight = (self.weighter)(&k, &old);
            self.weight -= old_weight;
            self.t1_weight -= old_weight;
            self.evict_to_fit(entry_weight, &mut evicted);
            self.weight += entry_weight;
            self.t2.insert(k, v);
        } else if let Some(old) = self.t2.remove(&k) {
            let old_weight = (self.weighter)(&k, &old);
            self.weight -= old_weight;
            self.evict_to_fit(entry_weight, &mut evicted);
            self.weight += entry_weight;
            self.t2.insert(k, v);
        }
        // Case 2: key in ghost B1 → adapt p upward (favor recency)
        else if self.b1.remove(&k).is_some() {
            let delta = cmp::max(1, self.b2_weight() / cmp::max(1, self.b1_weight()));
            self.p = cmp::min(self.budget, self.p.saturating_add(delta));
            self.replace(false, &mut evicted);
            self.evict_to_fit(entry_weight, &mut evicted);
            self.weight += entry_weight;
            self.t2.insert(k, v);
        }
        // Case 3: key in ghost B2 → adapt p downward (favor frequency)
        else if self.b2.remove(&k).is_some() {
            let delta = cmp::max(1, self.b1_weight() / cmp::max(1, self.b2_weight()));
            self.p = self.p.saturating_sub(delta);
            self.replace(true, &mut evicted);
            self.evict_to_fit(entry_weight, &mut evicted);
            self.weight += entry_weight;
            self.t2.insert(k, v);
        }
        // Case 4: complete miss → evict to budget, insert into T1.
        else {
            self.evict_to_fit(entry_weight, &mut evicted);
            self.weight += entry_weight;
            self.t1_weight += entry_weight;
            self.t1.insert(k, v);
        }

        // Prune ghost lists after every insert. replace() and evict_to_fit()
        // can push entries into B1/B2, so we always need to enforce caps.
        self.prune_ghosts();

        evicted
    }

    /// Check if a key exists in T1 or T2 without promoting.
    pub fn contains(&self, k: &Cid) -> bool {
        self.t1.contains_key(k) || self.t2.contains_key(k)
    }

    /// Borrow a resident value mutably without changing ARC recency.
    pub(crate) fn value_mut(&mut self, k: &Cid) -> Option<&mut V> {
        self.t1.get_mut(k).or_else(|| self.t2.get_mut(k))
    }

    /// Number of entries in T1 + T2.
    pub fn len(&self) -> usize {
        self.t1.len() + self.t2.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Current total weight of cached entries.
    pub fn weight(&self) -> usize {
        self.weight
    }

    /// Hit/miss/eviction counters.
    pub fn stats(&self) -> &ArcStats {
        &self.stats
    }

    /// Iterator over all keys in T1 and T2.
    pub fn keys(&self) -> impl Iterator<Item = &Cid> {
        self.t1.keys().chain(self.t2.keys())
    }

    // --- Private helpers ---

    /// ARC replace: evict one entry from T1 or T2 based on p.
    /// `b2_hit` is true when the caller was triggered by a B2 ghost hit,
    /// which biases toward evicting from T1 (per the paper).
    fn replace(&mut self, b2_hit: bool, evicted: &mut Vec<(Cid, V)>) {
        if self.t1.is_empty() && self.t2.is_empty() {
            return;
        }

        let should_evict_t1 = if self.t1.is_empty() {
            false
        } else if self.t2.is_empty() || self.t1_weight > self.p {
            true
        } else if self.t1_weight == self.p && b2_hit {
            // Tie-break: if triggered by B2 hit, prefer evicting T1
            true
        } else {
            false
        };

        if should_evict_t1 {
            if let Some((k, v)) = self.t1.pop_front() {
                let w = (self.weighter)(&k, &v);
                self.weight -= w;
                self.t1_weight -= w;
                self.b1.insert(k, ());
                self.stats.evictions += 1;
                evicted.push((k, v));
            }
        } else if let Some((k, v)) = self.t2.pop_front() {
            let w = (self.weighter)(&k, &v);
            self.weight -= w;
            self.b2.insert(k, ());
            self.stats.evictions += 1;
            evicted.push((k, v));
        }
    }

    /// Evict entries from T1/T2 until there's room for `needed` weight.
    fn evict_to_fit(&mut self, needed: usize, evicted: &mut Vec<(Cid, V)>) {
        while self.weight + needed > self.budget {
            if self.t1.is_empty() && self.t2.is_empty() {
                break;
            }
            self.replace(false, evicted);
        }
    }

    /// Prune ghost lists per the ARC paper's caps.
    ///
    /// The paper caps B1 at `c - p` and B2 at `p` (both in entry-count units).
    /// Since our `p` is in weight units, we normalize:
    ///   `p_frac = p * ghost_capacity / budget`
    /// Then: B1 ≤ ghost_capacity - p_frac, B2 ≤ p_frac.
    ///
    /// This keeps ghost list sizes proportional to the adaptive split,
    /// ensuring that the list the algorithm is currently favoring retains
    /// more history for future adaptation decisions.
    fn prune_ghosts(&mut self) {
        let p_frac = (self.p * self.ghost_capacity)
            .checked_div(self.budget)
            .unwrap_or(0);

        let b1_cap = self.ghost_capacity.saturating_sub(p_frac).max(1);
        let b2_cap = p_frac.max(1);

        while self.b1.len() > b1_cap {
            self.b1.pop_front();
        }
        while self.b2.len() > b2_cap {
            self.b2.pop_front();
        }
    }

    /// "Weight" of B1 ghost list. Since ghosts have no values, we use
    /// entry count as the weight proxy for p adaptation ratios.
    fn b1_weight(&self) -> usize {
        self.b1.len()
    }

    /// "Weight" of B2 ghost list. Same count-based proxy.
    fn b2_weight(&self) -> usize {
        self.b2.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cid(n: u8) -> Cid {
        // Create a deterministic CID from a byte. Uses identity multihash.
        let mh = cid::multihash::Multihash::wrap(0x00, &[n]).unwrap();
        Cid::new_v1(0x55, mh) // 0x55 = raw codec
    }

    fn unit_weighter(_k: &Cid, _v: &u64) -> usize {
        1
    }

    fn value_weighter(_k: &Cid, v: &u64) -> usize {
        *v as usize
    }

    /// Verify all ARC structural invariants. Call after every operation
    /// in stress tests to catch corruption early.
    fn assert_invariants(cache: &ArcInner<u64>, weighter: fn(&Cid, &u64) -> usize) {
        // 1. weight == actual sum of T1 + T2 weights
        let actual_weight: usize = cache
            .t1
            .iter()
            .chain(cache.t2.iter())
            .map(|(k, v)| weighter(k, v))
            .sum();
        assert_eq!(
            cache.weight, actual_weight,
            "weight field ({}) != actual T1+T2 weight ({})",
            cache.weight, actual_weight
        );

        // 2. t1_weight == actual sum of T1 weights
        let actual_t1_weight: usize = cache.t1.iter().map(|(k, v)| weighter(k, v)).sum();
        assert_eq!(
            cache.t1_weight, actual_t1_weight,
            "t1_weight field ({}) != actual T1 weight ({})",
            cache.t1_weight, actual_t1_weight
        );

        // 3. weight <= budget
        assert!(
            cache.weight <= cache.budget,
            "weight ({}) exceeds budget ({})",
            cache.weight,
            cache.budget
        );

        // 4. 0 <= p <= budget
        assert!(
            cache.p <= cache.budget,
            "p ({}) exceeds budget ({})",
            cache.p,
            cache.budget
        );

        // 5. Disjointness: no key in more than one list
        for k in cache.t1.keys() {
            assert!(!cache.t2.contains_key(k), "key in both T1 and T2");
            assert!(!cache.b1.contains_key(k), "key in both T1 and B1");
            assert!(!cache.b2.contains_key(k), "key in both T1 and B2");
        }
        for k in cache.t2.keys() {
            assert!(!cache.b1.contains_key(k), "key in both T2 and B1");
            assert!(!cache.b2.contains_key(k), "key in both T2 and B2");
        }
        for k in cache.b1.keys() {
            assert!(!cache.b2.contains_key(k), "key in both B1 and B2");
        }

        // 6. Ghost list caps
        let p_frac = (cache.p * cache.ghost_capacity)
            .checked_div(cache.budget)
            .unwrap_or(0);
        let b1_cap = cache.ghost_capacity.saturating_sub(p_frac).max(1);
        let b2_cap = p_frac.max(1);
        assert!(
            cache.b1.len() <= b1_cap,
            "B1 len ({}) exceeds cap ({})",
            cache.b1.len(),
            b1_cap
        );
        assert!(
            cache.b2.len() <= b2_cap,
            "B2 len ({}) exceeds cap ({})",
            cache.b2.len(),
            b2_cap
        );
    }

    // 1. test_insert_to_t1 — new entry lands in T1
    #[test]
    fn test_insert_to_t1() {
        let mut cache = ArcInner::new(10, 128, unit_weighter);
        let k = test_cid(1);
        let evicted = cache.put(k, 100);
        assert!(evicted.is_empty());
        assert!(cache.contains(&k));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.weight(), 1);

        // Should be findable via get (which promotes to T2)
        assert!(cache.get(&k).is_some());
    }

    // 2. test_hit_promotes_t1_to_t2 — second access moves to T2
    #[test]
    fn test_hit_promotes_t1_to_t2() {
        let mut cache = ArcInner::new(10, 128, unit_weighter);
        let k1 = test_cid(1);
        let k2 = test_cid(2);

        cache.put(k1, 10);
        cache.put(k2, 20);

        // get(k1) promotes from T1 to T2
        assert_eq!(*cache.get(&k1).unwrap(), 10);

        // k1 is now in T2, k2 still in T1
        // Fill cache to force eviction — T1 entries evict before T2
        // (since p starts at 0, T1 weight > p = 0 always)
        for i in 3..=10 {
            cache.put(test_cid(i), i as u64);
        }

        // k1 was promoted to T2, should survive longer
        assert!(cache.contains(&k1));
    }

    // 3. test_eviction_by_weight — entries evicted when weight exceeds budget
    #[test]
    fn test_eviction_by_weight() {
        let mut cache = ArcInner::new(5, 128, value_weighter);
        cache.put(test_cid(1), 3); // weight 3
        cache.put(test_cid(2), 2); // weight 2, total 5

        // This should trigger eviction
        let evicted = cache.put(test_cid(3), 2);
        assert!(!evicted.is_empty());
        assert!(cache.weight() <= 5);
    }

    // 4. test_ghost_b1_adapts_p_up — B1 hit increases p
    #[test]
    fn test_ghost_b1_adapts_p_up() {
        let mut cache = ArcInner::new(3, 128, unit_weighter);
        let k1 = test_cid(1);
        let k2 = test_cid(2);
        let k3 = test_cid(3);
        let k4 = test_cid(4);

        // Fill T1: k1, k2, k3
        cache.put(k1, 1);
        cache.put(k2, 2);
        cache.put(k3, 3);

        // This evicts k1 to B1 (LRU of T1, since p=0 means T1 weight > p)
        let evicted = cache.put(k4, 4);
        assert!(!evicted.is_empty());

        let p_before = cache.p;

        // Re-insert k1 — it's in B1, so p should increase
        cache.put(k1, 1);
        assert!(cache.p > p_before, "p should increase on B1 hit");
    }

    // 5. test_ghost_b2_adapts_p_down — B2 hit decreases p
    #[test]
    fn test_ghost_b2_adapts_p_down() {
        let mut cache = ArcInner::new(3, 128, unit_weighter);

        let k1 = test_cid(1);
        let k2 = test_cid(2);
        let k3 = test_cid(3);
        let k4 = test_cid(4);

        // Put k1 and promote to T2
        cache.put(k1, 1);
        cache.get(&k1); // T2=[k1]

        // Put k2 and promote to T2
        cache.put(k2, 2);
        cache.get(&k2); // T2=[k1, k2]

        // Put k3 into T1
        cache.put(k3, 3); // T1=[k3], T2=[k1, k2], weight=3

        // Set p high so replace() evicts from T2 (not T1)
        cache.p = cache.budget; // p=3, T1 weight (1) < p (3) → evict T2

        // Insert k4 — forces eviction. Since p is high, evicts LRU of T2 (k1) → B2
        cache.put(k4, 4);

        // k1 should now be in B2
        assert!(cache.b2.contains_key(&k1), "k1 should be in B2");

        let p_before = cache.p;
        // Re-insert k1 — B2 hit, p should decrease
        cache.put(k1, 1);
        assert!(cache.p < p_before, "p should decrease on B2 hit");
    }

    // 6. test_put_returns_evicted — evicted entries returned for unpin
    #[test]
    fn test_put_returns_evicted() {
        let mut cache = ArcInner::new(2, 128, unit_weighter);
        cache.put(test_cid(1), 10);
        cache.put(test_cid(2), 20);

        let evicted = cache.put(test_cid(3), 30);
        assert!(!evicted.is_empty());
        assert_eq!(evicted.len(), 1);
    }

    // 7. test_weighted_entries — heavier entries evict more items
    #[test]
    fn test_weighted_entries() {
        let mut cache = ArcInner::new(10, 128, value_weighter);
        cache.put(test_cid(1), 2);
        cache.put(test_cid(2), 2);
        cache.put(test_cid(3), 2);
        cache.put(test_cid(4), 2);
        assert_eq!(cache.weight(), 8);

        // Insert a heavy entry (weight 6) — needs to evict multiple small ones
        let evicted = cache.put(test_cid(5), 6);
        assert!(evicted.len() >= 2, "should evict multiple entries");
        assert!(cache.weight() <= 10);
    }

    // 8. test_contains_no_promotion — contains() is peek-only
    #[test]
    fn test_contains_no_promotion() {
        let mut cache = ArcInner::new(10, 128, unit_weighter);
        let k = test_cid(1);
        cache.put(k, 1);

        // contains() should not promote from T1 to T2
        assert!(cache.contains(&k));

        // Verify it's still in T1 by checking that get() promotes it
        // (if it were already in T2, get wouldn't need to promote)
        assert!(cache.t1.contains_key(&k));
        assert!(!cache.t2.contains_key(&k));
    }

    // 9. test_budget_zero — budget 0 panics
    #[test]
    #[should_panic(expected = "ARC budget must be > 0")]
    fn test_budget_zero() {
        ArcInner::<u64>::new(0, 128, unit_weighter);
    }

    // 10. test_replace_chooses_t1_vs_t2 — replace() respects p threshold
    #[test]
    fn test_replace_chooses_t1_vs_t2() {
        let mut cache = ArcInner::new(4, 128, unit_weighter);

        // Put k1, k2 in T1
        cache.put(test_cid(1), 1);
        cache.put(test_cid(2), 2);

        // Promote both to T2
        cache.get(&test_cid(1));
        cache.get(&test_cid(2));

        // Put k3, k4 in T1
        cache.put(test_cid(3), 3);
        cache.put(test_cid(4), 4);

        // Now: T1 = [k3, k4] (weight 2), T2 = [k1, k2] (weight 2), total = 4
        // Set p = 3 (high) — T1 weight (2) < p (3), so replace should evict from T2
        cache.p = 3;

        let mut evicted = Vec::new();
        cache.replace(false, &mut evicted);

        // Evicted entry should be from T2 (k1, the LRU of T2)
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].0, test_cid(1));
    }

    // 11. test_single_entry_exceeds_budget — entry larger than budget is rejected
    #[test]
    fn test_single_entry_exceeds_budget() {
        let mut cache = ArcInner::new(5, 128, value_weighter);

        let evicted = cache.put(test_cid(1), 10); // weight 10 > budget 5
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].0, test_cid(1));
        assert!(!cache.contains(&test_cid(1)));
        assert_eq!(cache.weight(), 0);
    }

    // 12. test_ghost_cap_zero — ghost_capacity 0 panics
    #[test]
    #[should_panic(expected = "ARC ghost_capacity must be > 0")]
    fn test_ghost_cap_zero() {
        ArcInner::<u64>::new(10, 0, unit_weighter);
    }

    // 13. test_ghost_b1_cap_respects_p — B1 capped at ghost_capacity - p_frac
    #[test]
    fn test_ghost_b1_cap_respects_p() {
        // Small ghost capacity to make caps easy to reason about.
        // budget=10, ghost_capacity=4, unit weighter.
        let mut cache = ArcInner::new(10, 4, unit_weighter);

        // p=0 → p_frac=0 → B1 cap = 4-0 = 4, B2 cap = max(0,1) = 1.
        // Fill T1 to capacity, then keep inserting to evict into B1.
        for i in 0..20 {
            cache.put(test_cid(i), i as u64);
        }

        // B1 should be capped at ghost_capacity (4) since p=0.
        assert!(
            cache.b1.len() <= 4,
            "B1 should be capped at ghost_capacity when p=0, got {}",
            cache.b1.len()
        );
    }

    // 14. test_ghost_b2_cap_respects_p — B2 capped at p_frac
    #[test]
    fn test_ghost_b2_cap_respects_p() {
        // budget=4, ghost_capacity=4, unit weighter.
        let mut cache = ArcInner::new(4, 4, unit_weighter);

        // Promote entries to T2 so evictions go to B2.
        for i in 0..4u8 {
            cache.put(test_cid(i), i as u64);
            cache.get(&test_cid(i)); // promote T1 → T2
        }

        // Set p to budget (max) → p_frac = 4*4/4 = 4 → B2 cap = 4.
        cache.p = cache.budget;

        // Now insert more entries. Since all existing are in T2 and p is high
        // (T1 weight < p), replace() evicts from T2 → B2.
        for i in 10..20 {
            cache.put(test_cid(i), i as u64);
        }

        assert!(
            cache.b2.len() <= 4,
            "B2 should be capped at p_frac (=ghost_capacity when p=budget), got {}",
            cache.b2.len()
        );
    }

    // 15. test_ghost_caps_adapt_with_p — caps shift as p adapts
    #[test]
    fn test_ghost_caps_adapt_with_p() {
        // budget=6, ghost_capacity=6, unit weighter.
        let mut cache = ArcInner::new(6, 6, unit_weighter);

        // Fill cache, force evictions to B1 (p starts at 0).
        for i in 0..12u8 {
            cache.put(test_cid(i), i as u64);
        }

        // Now set p = budget/2 = 3 → p_frac = 3*6/6 = 3 → B1 cap = 3, B2 cap = 3.
        cache.p = 3;

        // Trigger prune by inserting more.
        for i in 20..30u8 {
            cache.put(test_cid(i), i as u64);
        }

        assert!(
            cache.b1.len() <= 3,
            "B1 should be capped at ghost_capacity - p_frac = 3, got {}",
            cache.b1.len()
        );
    }

    // 16. test_ghost_caps_tight — ghost lists never exceed ghost_capacity total
    #[test]
    fn test_ghost_caps_tight() {
        let mut cache = ArcInner::new(3, 4, unit_weighter);

        // Churn many entries through to fill ghost lists.
        for i in 0..50u8 {
            cache.put(test_cid(i), i as u64);
        }

        let total_ghosts = cache.b1.len() + cache.b2.len();
        assert!(
            total_ghosts <= 4 + 1, // +1 for the max(1) floor on each side
            "total ghosts should be bounded by ghost_capacity, got {}",
            total_ghosts
        );
    }

    // --- Invariant stress tests ---

    // 17. Sequential insert churn — invariants hold after every put
    #[test]
    fn test_invariants_sequential_churn() {
        let mut cache = ArcInner::new(10, 16, unit_weighter);

        for i in 0..=255u8 {
            cache.put(test_cid(i), i as u64);
            assert_invariants(&cache, unit_weighter);
        }
    }

    // 18. Mixed get/put — invariants hold through promotion + eviction
    #[test]
    fn test_invariants_mixed_get_put() {
        let mut cache = ArcInner::new(5, 8, unit_weighter);

        for round in 0..3u8 {
            // Insert a batch
            for i in 0..10u8 {
                let id = round * 50 + i;
                cache.put(test_cid(id), id as u64);
                assert_invariants(&cache, unit_weighter);
            }

            // Read some back (promotes T1→T2)
            for i in 0..5u8 {
                let id = round * 50 + i;
                cache.get(&test_cid(id));
                assert_invariants(&cache, unit_weighter);
            }

            // Insert more (triggers ghost hits if keys cycle)
            for i in 0..10u8 {
                let id = round * 50 + i;
                cache.put(test_cid(id), id as u64);
                assert_invariants(&cache, unit_weighter);
            }
        }
    }

    // 19. Weighted entries — invariants hold with variable-weight entries
    #[test]
    fn test_invariants_weighted_churn() {
        let mut cache = ArcInner::new(20, 16, value_weighter);

        // Insert entries with weights 1..=5, cycling
        for i in 0..100u8 {
            let weight = (i % 5) as u64 + 1;
            cache.put(test_cid(i), weight);
            assert_invariants(&cache, value_weighter);
        }
    }

    // 20. Ghost hit cycles — B1/B2 hits adapt p, invariants hold
    #[test]
    fn test_invariants_ghost_hit_cycles() {
        let mut cache = ArcInner::new(3, 8, unit_weighter);

        // Phase 1: fill and evict to create ghost entries
        for i in 0..10u8 {
            cache.put(test_cid(i), i as u64);
            assert_invariants(&cache, unit_weighter);
        }

        // Phase 2: re-insert evicted keys to trigger ghost hits
        for i in 0..10u8 {
            cache.put(test_cid(i), i as u64);
            assert_invariants(&cache, unit_weighter);
        }

        // Phase 3: promote some, then cycle again
        for i in 0..5u8 {
            cache.get(&test_cid(i));
            assert_invariants(&cache, unit_weighter);
        }
        for i in 0..10u8 {
            cache.put(test_cid(i), i as u64);
            assert_invariants(&cache, unit_weighter);
        }
    }

    // 21. Oversize entries interleaved — budget never violated
    #[test]
    fn test_invariants_oversize_interleaved() {
        let mut cache = ArcInner::new(10, 8, value_weighter);

        for i in 0..50u8 {
            // Alternate between normal (weight 2) and oversize (weight 20)
            let weight = if i % 7 == 0 { 20 } else { 2 };
            cache.put(test_cid(i), weight);
            assert_invariants(&cache, value_weighter);
        }
    }

    // 22. Rapid p swings — force alternating B1/B2 hits
    #[test]
    fn test_invariants_rapid_p_swings() {
        let mut cache = ArcInner::new(4, 8, unit_weighter);

        // Build up ghost lists on both sides by alternating access patterns.
        // Even keys: insert, evict, re-insert (B1 hits → p up)
        // Odd keys: insert, promote to T2, evict, re-insert (B2 hits → p down)
        for round in 0..5u8 {
            let base = round * 20;

            // Insert even keys (will land in T1, evict to B1)
            for i in (0..8).step_by(2) {
                cache.put(test_cid(base + i), (base + i) as u64);
                assert_invariants(&cache, unit_weighter);
            }

            // Insert odd keys with promotion (will evict to B2)
            for i in (1..8).step_by(2) {
                cache.put(test_cid(base + i), (base + i) as u64);
                cache.get(&test_cid(base + i)); // promote to T2
                assert_invariants(&cache, unit_weighter);
            }

            // Re-insert earlier keys to trigger ghost hits
            for i in 0..8u8 {
                let prev_base = if round > 0 { (round - 1) * 20 } else { 0 };
                cache.put(test_cid(prev_base + i), (prev_base + i) as u64);
                assert_invariants(&cache, unit_weighter);
            }
        }
    }

    // 23. Tight ghost capacity — ghost_capacity = 2, minimal room
    #[test]
    fn test_invariants_tight_ghost_capacity() {
        let mut cache = ArcInner::new(3, 2, unit_weighter);

        for i in 0..100u8 {
            cache.put(test_cid(i), i as u64);
            assert_invariants(&cache, unit_weighter);

            if i % 3 == 0 {
                cache.get(&test_cid(i));
                assert_invariants(&cache, unit_weighter);
            }
        }
    }
}
