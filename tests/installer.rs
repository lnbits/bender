#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;

struct Fixture {
    root: TempDir,
    bin: PathBuf,
    releases: PathBuf,
    prefix: PathBuf,
    log: PathBuf,
}

impl Fixture {
    fn new(asset: &str, version: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("mock-bin");
        let releases = root.path().join("releases");
        let prefix = root.path().join("prefix");
        let log = root.path().join("downloads.log");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&releases).unwrap();
        write_executable(
            &releases.join(asset),
            &format!("#!/bin/sh\necho 'bender {version}'\n"),
        );
        write_sums(&releases, asset);
        write_executable(
            &bin.join("curl"),
            r#"#!/bin/sh
set -eu
url=
out=
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o) out=$2; shift 2 ;;
    -*) shift ;;
    *) url=$1; shift ;;
  esac
done
printf '%s\n' "$url" >> "$BENDER_TEST_DOWNLOAD_LOG"
name=${url##*/}
[ "${BENDER_TEST_CURL_MODE:-}" != interrupt ] || {
  printf partial > "$out"
  exit 18
}
[ -f "$BENDER_TEST_FIXTURES/$name" ] || exit 22
cp "$BENDER_TEST_FIXTURES/$name" "$out"
"#,
        );
        Self {
            root,
            bin,
            releases,
            prefix,
            log,
        }
    }

    fn run(&self, os: &str, arch: &str, args: &[&str]) -> Output {
        self.run_with_base(os, arch, args, Some("https://fixtures.invalid/download"))
    }

    fn run_with_base(
        &self,
        os: &str,
        arch: &str,
        args: &[&str],
        download_base: Option<&str>,
    ) -> Output {
        let system_path = std::env::var("PATH").unwrap_or_default();
        let mut command = Command::new("sh");
        command
            .arg("install.sh")
            .args(args)
            .env("HOME", self.root.path().join("home"))
            .env("PATH", format!("{}:{system_path}", self.bin.display()))
            .env("BENDER_TEST_UNAME_S", os)
            .env("BENDER_TEST_UNAME_M", arch)
            .env("BENDER_TEST_FIXTURES", &self.releases)
            .env("BENDER_TEST_DOWNLOAD_LOG", &self.log)
            .env_remove("BENDER_DOWNLOAD_BASE_URL");
        if let Some(download_base) = download_base {
            command.env("BENDER_DOWNLOAD_BASE_URL", download_base);
        }
        command.output().unwrap()
    }

    fn run_interrupted(&self, os: &str, arch: &str, args: &[&str]) -> Output {
        let system_path = std::env::var("PATH").unwrap_or_default();
        Command::new("sh")
            .arg("install.sh")
            .args(args)
            .env("HOME", self.root.path().join("home"))
            .env("PATH", format!("{}:{system_path}", self.bin.display()))
            .env("BENDER_TEST_UNAME_S", os)
            .env("BENDER_TEST_UNAME_M", arch)
            .env("BENDER_TEST_FIXTURES", &self.releases)
            .env("BENDER_TEST_DOWNLOAD_LOG", &self.log)
            .env(
                "BENDER_DOWNLOAD_BASE_URL",
                "https://fixtures.invalid/download",
            )
            .env("BENDER_TEST_CURL_MODE", "interrupt")
            .output()
            .unwrap()
    }
}

fn write_executable(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn write_sums(releases: &Path, asset: &str) {
    let output = Command::new("sha256sum")
        .arg(releases.join(asset))
        .output()
        .unwrap();
    let hash = String::from_utf8(output.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();
    fs::write(releases.join("SHA256SUMS"), format!("{hash}  {asset}\n")).unwrap();
}

#[test]
fn supported_platform_mapping_and_latest_url_construction() {
    for (os, arch, asset) in [
        ("Linux", "x86_64", "bender-linux-x86_64"),
        ("Linux", "aarch64", "bender-linux-aarch64"),
        ("Darwin", "x86_64", "bender-macos-x86_64"),
        ("Darwin", "arm64", "bender-macos-aarch64"),
        ("MINGW64_NT", "amd64", "bender-windows-x86_64.exe"),
    ] {
        let fixture = Fixture::new(asset, "0.2.0");
        let prefix = fixture.prefix.to_str().unwrap();
        let output = fixture.run(os, arch, &["--prefix", prefix, "--non-interactive"]);
        assert!(
            output.status.success(),
            "{os}/{arch}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let log = fs::read_to_string(&fixture.log).unwrap();
        assert!(log.contains(&format!("https://fixtures.invalid/download/{asset}")));
        assert!(log.contains("https://fixtures.invalid/download/SHA256SUMS"));
    }
}

#[test]
fn successful_explicit_version_custom_prefix_is_atomic_and_idempotent() {
    let fixture = Fixture::new("bender-linux-x86_64", "0.2.0");
    let prefix = fixture.prefix.to_str().unwrap();
    let args = [
        "--version",
        "v0.2.0",
        "--prefix",
        prefix,
        "--non-interactive",
    ];
    for _ in 0..2 {
        let output = fixture.run_with_base("Linux", "x86_64", &args, None);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("Bender installed successfully."));
        assert!(stdout.contains("bender 0.2.0"));
        assert!(stdout.contains("  codex login"));
        assert!(stdout.contains("  bender doctor"));
    }
    let installed = fixture.prefix.join("bin/bender");
    assert!(installed.exists());
    assert_ne!(
        fs::metadata(&installed).unwrap().permissions().mode() & 0o111,
        0
    );
    let log = fs::read_to_string(&fixture.log).unwrap();
    assert!(log
        .contains("https://github.com/lnbits/bender/releases/download/v0.2.0/bender-linux-x86_64"));
}

#[test]
fn checksum_mismatch_and_missing_entry_preserve_existing_binary() {
    for sums in [
        "deadbeef  bender-linux-x86_64\n",
        "deadbeef  another-asset\n",
    ] {
        let fixture = Fixture::new("bender-linux-x86_64", "0.2.0");
        fs::create_dir_all(fixture.prefix.join("bin")).unwrap();
        let installed = fixture.prefix.join("bin/bender");
        fs::write(&installed, "existing").unwrap();
        fs::write(fixture.releases.join("SHA256SUMS"), sums).unwrap();
        let output = fixture.run(
            "Linux",
            "x86_64",
            &["--prefix", fixture.prefix.to_str().unwrap()],
        );
        assert!(!output.status.success());
        assert_eq!(fs::read_to_string(&installed).unwrap(), "existing");
    }
}

#[test]
fn missing_asset_and_interrupted_download_preserve_existing_binary() {
    let fixture = Fixture::new("bender-linux-x86_64", "0.2.0");
    fs::remove_file(fixture.releases.join("bender-linux-x86_64")).unwrap();
    fs::create_dir_all(fixture.prefix.join("bin")).unwrap();
    let installed = fixture.prefix.join("bin/bender");
    fs::write(&installed, "existing").unwrap();
    let output = fixture.run(
        "Linux",
        "x86_64",
        &["--prefix", fixture.prefix.to_str().unwrap()],
    );
    assert!(!output.status.success());
    assert_eq!(fs::read_to_string(&installed).unwrap(), "existing");

    write_executable(
        &fixture.releases.join("bender-linux-x86_64"),
        "#!/bin/sh\necho bender\n",
    );
    write_sums(&fixture.releases, "bender-linux-x86_64");
    let output = fixture.run_interrupted(
        "Linux",
        "x86_64",
        &["--prefix", fixture.prefix.to_str().unwrap()],
    );
    assert!(!output.status.success());
    assert_eq!(fs::read_to_string(installed).unwrap(), "existing");
}

#[test]
fn unsupported_platform_fails_before_creating_target() {
    let fixture = Fixture::new("bender-linux-x86_64", "0.2.0");
    for (os, arch) in [
        ("Plan9", "x86_64"),
        ("Linux", "mips64"),
        ("MINGW64_NT", "arm64"),
    ] {
        let output = fixture.run(os, arch, &["--prefix", fixture.prefix.to_str().unwrap()]);
        assert!(!output.status.success());
    }
    assert!(!fixture.prefix.exists());
}

#[test]
fn installer_syntax_and_dependency_scope() {
    let installer = include_str!("../install.sh");
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
    assert!(Command::new("sh")
        .args(["-n", "install.sh"])
        .status()
        .unwrap()
        .success());
}
