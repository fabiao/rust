pub fn fill_bytes(bytes: &mut [u8]) {
    askrust::random::fill(bytes).expect("failed to generate random data");
}
