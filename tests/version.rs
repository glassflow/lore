use std::process::Command;

#[test]
fn version_flag_prints_the_build_version() {
    let out = Command::new(env!("CARGO_BIN_EXE_lore"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8(out.stdout).unwrap().trim_end(),
        format!("lore {}", lore::VERSION)
    );
}
