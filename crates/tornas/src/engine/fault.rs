//! Typed failures the HTTP layer turns into status codes. Anything else is a 500.

use serde::Serialize;

/// Why an operation failed, so the HTTP layer can pick a status without parsing messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FaultKind {
    NotFound,
    Conflict,
    Invalid,
    NoSpace,
    Upstream,
}

#[derive(Debug)]
pub struct Fault {
    pub kind: FaultKind,
    pub message: String,
}
impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Fault {}

pub fn fault(kind: FaultKind, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(Fault {
        kind,
        message: message.into(),
    })
}

pub fn fault_kind(e: &anyhow::Error) -> Option<FaultKind> {
    e.chain()
        .find_map(|c| c.downcast_ref::<Fault>().map(|f| f.kind))
}
