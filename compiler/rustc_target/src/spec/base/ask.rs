use crate::spec::{Cc, LinkerFlavor, Lld, Os, PanicStrategy, StackProbeType, TargetOptions};

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
        // Userspace uses the ordinary x86_64 hard-float/SSE2 ABI. The
        // kernel remains soft-float, but saves and restores each process's
        // FXSAVE area at every context switch.
        // No ELF TLS: the kernel sets no thread pointer yet (docs/02's open
        // question); thread-locals go through std's OS-level fallback.
        has_thread_local: false,
        panic_strategy: PanicStrategy::Abort,
        stack_probes: StackProbeType::Inline,
        ..Default::default()
    }
}
