#[test]
fn installer_only_installs_bender_and_prints_expected_next_steps() {
    let installer = include_str!("../install.sh");
    assert!(installer.contains(".local/bin"));
    assert!(installer.contains("Bender installed."));
    assert!(installer.contains("Run bender doctor."));
    assert!(installer.contains("Run bender."));
    for forbidden in [
        "npm install",
        "ollama pull",
        "rustup",
        "apt-get",
        "playwright install",
        "docker",
    ] {
        assert!(
            !installer.contains(forbidden),
            "installer must not run {forbidden}"
        );
    }
    let syntax = std::process::Command::new("sh")
        .args(["-n", "install.sh"])
        .status()
        .expect("sh must be available for installer syntax test");
    assert!(syntax.success());
}
