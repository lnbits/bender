use std::{fs, process::Command};

use tempfile::tempdir;

#[test]
fn globally_located_binary_initializes_the_launch_directory() {
    let parent = tempdir().unwrap();
    let project = parent.path().join("selected-project");
    fs::create_dir(&project).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_bender"))
        .arg("init")
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(project.join(".bender/config.toml").exists());
    assert!(project.join(".bender/project.toml").exists());
    assert!(project.join(".bender/jobs").is_dir());
    assert!(String::from_utf8_lossy(&output.stdout).contains(project.to_str().unwrap()));
}
