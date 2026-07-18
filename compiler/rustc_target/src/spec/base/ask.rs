use crate::spec::{Cc, LinkerFlavor, Lld, Os, PanicStrategy, RustcAbi, StackProbeType, TargetOptions};

pub(crate) fn opts() -> TargetOptions {
    TargetOptions {
        os: Os::Ask,
        executables: true,
        // Static, non-PIE binaries: the ask kernel's ELF loader maps PT_LOAD
        // segments verbatim and applies no relocations.
        dynamic_linking: false,
        position_independent_executables: false,
        static_position_independent_executables: false,
        linker_flavor: LinkerFlavor::Gnu(Cc::No, Lld::Yes),
        linker: Some("rust-lld".into()),
        // Soft-float, no SSE: the kernel does not save FPU/SSE state on
        // context switch, so userspace must not produce such instructions
        // (same feature set as every ask service's target specification).
        features: "-mmx,-sse,-sse2,-sse3,-ssse3,-sse4.1,-sse4.2,-avx,-avx2,+soft-float".into(),
        rustc_abi: Some(RustcAbi::Softfloat),
        // No ELF TLS: the kernel sets no thread pointer yet (docs/02's open
        // question); thread-locals go through std's OS-level fallback.
        has_thread_local: false,
        panic_strategy: PanicStrategy::Abort,
        stack_probes: StackProbeType::Inline,
        ..Default::default()
    }
}
