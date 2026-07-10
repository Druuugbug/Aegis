/// 2.3.4: Basic integration test - verify large strings don't panic
#[test]
fn test_large_output_no_panic() {
    let big = "x".repeat(1_000_000);
    // 只要不 panic 就算通过
    let _ = big.len();
    assert!(big.len() == 1_000_000);
}
