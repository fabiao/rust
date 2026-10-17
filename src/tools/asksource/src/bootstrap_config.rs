pub mod environment {
    // Helper builds set each embedded Cargo library's own release configuration.
    pub const CARGO_RELEASES: &str = "ASK_SOURCE_CARGO_RELEASES";
}

pub mod build_mode {
    // Stable context names describe the prepared bootstrap Cargo command.
    pub const FRONTEND: &str = "frontend";
    pub const STD: &str = "std";
    pub const RUSTC: &str = "rustc";
    pub const CODEGEN: &str = "codegen";
    pub const TOOL_BOOTSTRAP: &str = "tool-bootstrap";
    pub const TOOL_TARGET: &str = "tool-target";
    pub const TOOL_STD: &str = "tool-std";
    pub const TOOL_RUSTC_PRIVATE: &str = "tool-rustc-private";

    const _: () = {
        let names = [FRONTEND, STD, RUSTC, CODEGEN, TOOL_BOOTSTRAP, TOOL_TARGET, TOOL_STD, TOOL_RUSTC_PRIVATE];
        let mut left = 0;
        while left < names.len() {
            let mut right = left + 1;
            while right < names.len() {
                let (a, b) = (names[left].as_bytes(), names[right].as_bytes());
                let mut byte = 0;
                while byte < a.len() && byte < b.len() && a[byte] == b[byte] {
                    byte += 1;
                }
                assert!(byte != a.len() || byte != b.len(), "duplicate bootstrap capture mode");
                right += 1;
            }
            left += 1;
        }
    };
}
