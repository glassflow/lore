//! Runs the unit tests of the retrieval eval and the agent comparison (bench/eval/test_eval.py)
//! under python3. Skipped without python3. They need no server, no model and no network.

#[test]
fn bench_eval_scripts_pass_their_tests() {
    let python = std::process::Command::new("python3")
        .arg("--version")
        .output();
    if !python.map(|o| o.status.success()).unwrap_or(false) {
        eprintln!("python3 is not installed: skipping the bench/eval checks");
        return;
    }
    let root = env!("CARGO_MANIFEST_DIR");
    let out = std::process::Command::new("python3")
        .args(["-m", "unittest", "discover", "-s"])
        .arg(format!("{root}/bench/eval"))
        .args(["-p", "test_*.py"])
        .output()
        .expect("run python3");
    assert!(
        out.status.success(),
        "bench/eval tests failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
