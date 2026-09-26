//! Concrete eviction strategies. Only [`Lru`] ships today; the trait is here so
//! adding another (largest-first, oldest-by-added, …) is a new struct, not a
//! rewrite.

use super::policy::{Candidate, EvictionPolicy, Need, Plan, PlanError};

/// Least-recently-used: evict the coldest torrents first (by `last_used_at`, ties
/// broken by key for determinism), skipping protected ones, until enough is freed.
pub struct Lru;

impl EvictionPolicy for Lru {
    fn select(&self, candidates: &[Candidate], need: Need) -> Result<Plan, PlanError> {
        let Need {
            used,
            incoming,
            limit,
            extra_needed,
        } = need;
        if incoming > limit {
            return Err(PlanError::LargerThanBudget { incoming, limit });
        }
        let over_budget = (used + incoming).saturating_sub(limit);
        let mut needed = over_budget.max(extra_needed);
        let mut evict = Vec::new();
        let mut freed = 0u64;
        if needed == 0 {
            return Ok(Plan {
                evict,
                used_before: used,
                used_after: used,
            });
        }
        let mut sorted: Vec<&Candidate> = candidates.iter().filter(|c| !c.protected).collect();
        sorted.sort_by_key(|c| (c.last_used_at, c.key.clone()));
        for c in sorted {
            if freed >= needed {
                break;
            }
            freed += c.size;
            evict.push(c.clone());
        }
        if freed < needed {
            needed -= freed;
            return Err(PlanError::NotEnoughSpace {
                needed,
                freeable: freed,
            });
        }
        Ok(Plan {
            evict,
            used_before: used,
            used_after: used.saturating_sub(freed),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(key: &str, size: u64, last: i64, protected: bool) -> Candidate {
        Candidate {
            key: key.into(),
            title: key.into(),
            size,
            last_used_at: last,
            protected,
        }
    }

    fn need(used: u64, incoming: u64, limit: u64, extra_needed: u64) -> Need {
        Need {
            used,
            incoming,
            limit,
            extra_needed,
        }
    }

    #[test]
    fn nothing_to_do_when_under_budget() {
        let cs = vec![c("a", 50, 1, false)];
        let p = Lru.select(&cs, need(50, 40, 120, 0)).unwrap();
        assert!(p.evict.is_empty());
    }

    #[test]
    fn evicts_oldest_first() {
        let cs = vec![
            c("new", 50, 30, false),
            c("old", 50, 10, false),
            c("mid", 50, 20, false),
        ];
        let p = Lru.select(&cs, need(150, 50, 160, 0)).unwrap();
        let keys: Vec<_> = p.evict.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, vec!["old"]);
        assert_eq!(p.used_after, 100);
    }

    #[test]
    fn evicts_several_if_needed() {
        let cs = vec![
            c("a", 50, 1, false),
            c("b", 50, 2, false),
            c("c", 50, 3, false),
        ];
        let p = Lru.select(&cs, need(150, 120, 160, 0)).unwrap();
        let keys: Vec<_> = p.evict.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, vec!["a", "b", "c"]);
    }

    #[test]
    fn skips_protected() {
        let cs = vec![c("a", 50, 1, true), c("b", 50, 2, false)];
        let p = Lru.select(&cs, need(100, 50, 120, 0)).unwrap();
        let keys: Vec<_> = p.evict.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, vec!["b"]);
    }

    #[test]
    fn errors_when_cannot_free_enough() {
        let cs = vec![c("a", 50, 1, true), c("b", 10, 2, false)];
        let e = Lru.select(&cs, need(60, 100, 120, 0)).unwrap_err();
        assert!(matches!(e, PlanError::NotEnoughSpace { .. }));
    }

    #[test]
    fn errors_when_bigger_than_budget() {
        let e = Lru.select(&[], need(0, 200, 120, 0)).unwrap_err();
        assert!(matches!(e, PlanError::LargerThanBudget { .. }));
    }

    #[test]
    fn honours_extra_needed_for_min_free() {
        let cs = vec![c("a", 50, 1, false), c("b", 50, 2, false)];
        // Under budget, but the filesystem needs 30 more bytes free.
        let p = Lru.select(&cs, need(100, 0, 1000, 30)).unwrap();
        assert_eq!(p.evict.len(), 1);
        assert_eq!(p.evict[0].key, "a");
    }
}
