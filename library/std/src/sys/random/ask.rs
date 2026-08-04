//! Kernel entropy source for ask (`GetEntropy`, hardware RDRAND/RDSEED with
//! a jitter-seeded SplitMix64 fallback — `recipes/core/kernel/source/src/`).

pub fn fill_bytes(bytes: &mut [u8]) {
    ask_abi::get_entropy(bytes).expect("failed to generate random data");
}
