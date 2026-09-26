//! Disk eviction for the media library: a pluggable policy that decides which
//! torrents to drop to stay within the disk budget. The policy is pure — it ranks
//! and selects candidates; executing the eviction (removing torrents, forgetting
//! rows) is the engine's job.

mod policies;
mod policy;

pub use policies::Lru;
pub use policy::{Candidate, EvictionPolicy, Need, Plan, PlanError};
