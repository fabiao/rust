# `allocator_reclaim`

Fork-local experiment in ASK; not upstreamed and not accepted by Rust upstream.
Submission and reviewer selection are deferred until ASK PAL stabilization.

`core::alloc::AllocatorReclaim` optionally extends `GlobalAlloc` with
`reclaim(retain_bytes)`. It operates on the selected allocator instance,
preserves live allocations and synchronizes with its other operations.
Implementations must not unwind or recursively allocate. The requested bytes
are free tail storage retained in addition to allocator bookkeeping.

`ReclaimResult` distinguishes `Unsupported`, `Unchanged` and `Released`.
The default is unsupported. Unchanged includes fragmentation and backing
release refusal; released means some unused backing was returned, without
promising a total heap-size limit or complete reclamation.

The ASK `System` backend invokes its locked dlmalloc owner. Other system
backends retain unsupported behavior. Custom global allocators are not
implicitly discovered; they may implement the extension themselves.
Existing allocation methods and compiler allocation shims are unchanged.

ASK applications use `askme::heap::reclaim` with explicit SDK heap selection.
Host API/backing checks and cumulative ASK verification own efficacy claims;
no efficacy is claimed for unsupported targets. The ASK toolchain contract,
Book and provider-account evidence record the current proof boundaries.
