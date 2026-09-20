//! Pure disk-budget planner. Given what is on disk and what is about to arrive,
//! decide which torrents to evict, least recently used first.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    /// Stable key, the torrent info hash.
    pub key: String,
    pub title: String,
    pub size: u64,
    pub last_used_at: i64,
    /// Protected items (streamed within the grace window) are never evicted.
    pub protected: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub evict: Vec<Candidate>,
    pub used_before: u64,
    pub used_after: u64,
}

#[derive(Debug)]
pub enum PlanError {
    NotEnoughSpace { needed: u64, freeable: u64 },
    LargerThanBudget { incoming: u64, limit: u64 },
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::NotEnoughSpace { needed, freeable } => write!(
                f,
                "not enough evictable space: need {needed} bytes, could free only {freeable}"
            ),
            PlanError::LargerThanBudget { incoming, limit } => write!(
                f,
                "incoming torrent ({incoming} bytes) is larger than the whole budget ({limit} bytes)"
            ),
        }
    }
}
impl std::error::Error for PlanError {}

/// `used` is the sum of sizes of all candidates plus anything else charged to the budget.
/// `incoming` is the size of the torrent about to be added (0 for a periodic sweep).
/// `limit` is the configured budget. `extra_needed` lets the caller add a second
/// constraint, e.g. "filesystem must keep min_free bytes free", expressed as bytes that
/// must be freed on top of the budget rule.
pub fn plan(
    candidates: &[Candidate],
    used: u64,
    incoming: u64,
    limit: u64,
    extra_needed: u64,
) -> Result<Plan, PlanError> {
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

    #[test]
    fn nothing_to_do_when_under_budget() {
        let cs = vec![c("a", 50, 1, false)];
        let p = plan(&cs, 50, 40, 120, 0).unwrap();
        assert!(p.evict.is_empty());
    }

    #[test]
    fn evicts_oldest_first() {
        let cs = vec![
            c("new", 50, 30, false),
            c("old", 50, 10, false),
            c("mid", 50, 20, false),
        ];
        let p = plan(&cs, 150, 50, 160, 0).unwrap();
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
        let p = plan(&cs, 150, 120, 160, 0).unwrap();
        let keys: Vec<_> = p.evict.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, vec!["a", "b", "c"]);
    }

    #[test]
    fn skips_protected() {
        let cs = vec![c("a", 50, 1, true), c("b", 50, 2, false)];
        let p = plan(&cs, 100, 50, 120, 0).unwrap();
        let keys: Vec<_> = p.evict.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, vec!["b"]);
    }

    #[test]
    fn errors_when_cannot_free_enough() {
        let cs = vec![c("a", 50, 1, true), c("b", 10, 2, false)];
        let e = plan(&cs, 60, 100, 120, 0).unwrap_err();
        assert!(matches!(e, PlanError::NotEnoughSpace { .. }));
    }

    #[test]
    fn errors_when_bigger_than_budget() {
        let e = plan(&[], 0, 200, 120, 0).unwrap_err();
        assert!(matches!(e, PlanError::LargerThanBudget { .. }));
    }

    #[test]
    fn honours_extra_needed_for_min_free() {
        let cs = vec![c("a", 50, 1, false), c("b", 50, 2, false)];
        // Under budget, but the filesystem needs 30 more bytes free.
        let p = plan(&cs, 100, 0, 1000, 30).unwrap();
        assert_eq!(p.evict.len(), 1);
        assert_eq!(p.evict[0].key, "a");
    }
}
