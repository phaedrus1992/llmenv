//! The records of the JSON `icm_memory_recall` reply that prune reads (#3368).

/// One record of a recall reply, as prune needs it.
#[derive(Debug, Clone)]
pub(super) struct MemoryRecord {
    pub(super) id: String,
    pub(super) importance: Importance,
}

/// The importance levels of an icm record.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Importance {
    Critical,
    High,
    Medium,
    Low,
    /// A level that ICM may add later. Prune forgets for good, so a record it cannot place is
    /// kept.
    Unknown,
}

impl Importance {
    fn from_str(s: &str) -> Self {
        match s.trim() {
            "critical" => Self::Critical,
            "high" => Self::High,
            "medium" => Self::Medium,
            "low" => Self::Low,
            _ => Self::Unknown,
        }
    }

    pub(super) fn should_prune(&self) -> bool {
        matches!(self, Self::Medium | Self::Low)
    }
}

/// The records of a JSON recall reply, in icm's rank order.
///
/// # Errors
/// The reply is not a JSON list of icm records. Prune forgets for good, so it reads nothing from
/// a reply that it cannot read in full.
pub(super) fn read_records(
    body: &str,
) -> Result<Vec<MemoryRecord>, crate::memory::recall_json::RecallJsonError> {
    Ok(crate::memory::recall_json::parse(body)?
        .into_iter()
        .map(|r| MemoryRecord {
            importance: Importance::from_str(&r.importance),
            id: r.id,
        })
        .collect())
}
