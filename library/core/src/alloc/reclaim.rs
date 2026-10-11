use super::GlobalAlloc;

/// Outcome of a best-effort unused-backing reclamation request.
#[unstable(feature = "allocator_reclaim", issue = "none")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReclaimResult {
    /// This allocator does not support reclamation requests.
    Unsupported,
    /// The request returned no backing, including refused releases.
    Unchanged,
    /// The allocator returned some unused backing.
    Released,
}

/// Optional backing reclamation for an explicitly selected allocator.
/// Implementations preserve live allocations, synchronize with other heap
/// operations, and must not unwind or recursively allocate.
#[unstable(feature = "allocator_reclaim", issue = "none")]
pub trait AllocatorReclaim: GlobalAlloc {
    /// Retain at least `retain_bytes` of free tail plus bookkeeping when trimming.
    /// Fragmentation and backing refusal may prevent release; this does not
    /// select or discover the program's global allocator.
    fn reclaim(&self, _retain_bytes: usize) -> ReclaimResult {
        ReclaimResult::Unsupported
    }
}
