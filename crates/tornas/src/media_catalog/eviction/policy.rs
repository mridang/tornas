//! The eviction contract: what a policy is handed, what it returns, and the trait
//! itself. Strategies live in [`super::policies`].

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

/// How much must be freed and under what limit, bundled so the policy takes one
/// argument. `used` is the sum of sizes of all candidates plus anything else
/// charged to the budget. `incoming` is the size of the torrent about to be added
/// (0 for a periodic sweep). `limit` is the configured budget. `extra_needed` is a
/// second constraint on top of the budget rule (e.g. "keep min_free bytes free on
/// the filesystem"), expressed as bytes to free beyond it.
#[derive(Debug, Clone, Copy)]
pub struct Need {
    pub used: u64,
    pub incoming: u64,
    pub limit: u64,
    pub extra_needed: u64,
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

/// Decides which candidates to evict to satisfy [`Need`]. Implementations must
/// never select a `protected` candidate. Pure: no I/O, no engine.
pub trait EvictionPolicy: Send + Sync {
    fn select(&self, candidates: &[Candidate], need: Need) -> Result<Plan, PlanError>;
}
