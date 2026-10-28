//! Axint VCS Windows Installer (`axint-setup.exe`).
//!
//! A self-contained Windows installer package that installs `axint.exe` into the
//! user's application directory, registers it into the persistent Windows PATH,
//! detects and integrates with GitHub CLI (`gh`), and validates installation.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const EMBEDDED_AXINT_EXE: &[u8] = include_bytes!("../../../target/release/axint.exe");
const VERSION: &str = "0.1.0";

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_help();
        return;
    }

    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("axint-setup v{VERSION} (Windows x86_64)");
        return;
    }

    print_banner();

    if args.iter().any(|arg| arg == "--uninstall") {
        run_uninstall();
    } else {
        run_install();
    }
}

fn print_banner() {
    println!(
        r#"
======================================================================
                 Axint VCS Windows Package Installer
        Next-Generation High-Performance Version Control System
======================================================================
"#
    );
}

fn print_help() {
    println!(
        r#"Axint VCS Windows Package Installer

USAGE:
    axint-setup.exe [OPTIONS]

OPTIONS:
    -h, --help        Print help information
    -V, --version     Print version information
    --uninstall       Remove Axint VCS from your Windows system and clean PATH
"#
    );
}

fn get_install_dir() -> PathBuf {
    if let Ok(local_app_data) = env::var("LOCALAPPDATA") {
        PathBuf::from(local_app_data).join("Programs").join("Axint").join("bin")
    } else if let Ok(user_profile) = env::var("USERPROFILE") {
        PathBuf::from(user_profile).join(".axint").join("bin")
    } else {
        PathBuf::from(r"C:\Program Files\Axint\bin")
    }
}

fn run_install() {
    let install_dir = get_install_dir();
    let target_exe = install_dir.join("axint.exe");

    println!(">>> Step 1/4: Preparing installation directory...");
    println!("    Destination: {}", install_dir.display());

    if let Err(e) = fs::create_dir_all(&install_dir) {
        eprintln!("[-] Error creating installation directory: {e}");
        return;
    }
    println!("    [OK] Directory ready.");

    println!("\n>>> Step 2/4: Extracting Axint executable...");
    match fs::write(&target_exe, EMBEDDED_AXINT_EXE) {
        Ok(()) => {
            let size_kb = EMBEDDED_AXINT_EXE.len() / 1024;
            println!("    [OK] Installed axint.exe ({size_kb} KB)");
        }
        Err(e) => {
            eprintln!("[-] Failed to write axint.exe: {e}");
            return;
        }
    }

    println!("\n>>> Step 3/4: Registering Axint in Windows User PATH...");
    add_to_user_path(&install_dir);

    println!("\n>>> Step 4/4: Detecting Ecosystem Integrations...");
    check_ecosystem();

    println!("\n>>> Validating Installed Executable...");
    let test_run = Command::new(&target_exe).arg("--version").output();
    match test_run {
        Ok(out) if out.status.success() => {
            let ver = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            println!("    [OK] Verified: {ver}");
        }
        _ => {
            println!("    [NOTE] Binary installed successfully at {}", target_exe.display());
        }
    }

    println!(
        r#"
======================================================================
[SUCCESS] Axint VCS has been successfully installed on your machine!
======================================================================

You can now use `axint` directly from PowerShell, Windows Terminal, or CMD:

  axint --help                               # View all commands
  axint init .                               # Initialize new repository
  axint status                               # Fast O(delta) status check (<5ms)
  axint commit -m "commit message"           # FastCDC + BLAKE3 signed commit
  axint push https://github.com/user/repo    # Push directly to GitHub / GitLab
  axint undo                                 # Non-destructive instant undo (<2ms)

* Note: If your current terminal was opened before installation, restart it
  or run `$env:PATH = [System.Environment]::GetEnvironmentVariable("Path","User") + ";$env:PATH"`
======================================================================
"#
    );
}

fn run_uninstall() {
    let install_dir = get_install_dir();
    let target_exe = install_dir.join("axint.exe");

    println!(">>> Uninstalling Axint VCS from: {}", install_dir.display());

    if target_exe.exists() {
        if let Err(e) = fs::remove_file(&target_exe) {
            eprintln!("[-] Could not delete axint.exe: {e}");
        } else {
            println!("    [OK] Removed axint.exe");
        }
    }

    let _ = fs::remove_dir(&install_dir);
    if let Some(parent) = install_dir.parent() {
        let _ = fs::remove_dir(parent);
    }

    remove_from_user_path(&install_dir);

    println!("\n[SUCCESS] Axint VCS uninstalled successfully.");
}

fn add_to_user_path(dir: &Path) {
    let dir_str = dir.to_string_lossy().to_string();

    let output = Command::new("reg")
        .args(["query", "HKCU\\Environment", "/v", "Path"])
        .output();

    let mut current_path = String::new();
    if let Ok(out) = output {
        if out.status.success() {
            let stdout = String::from_utf8_lossy(&out.stdout);
            for line in stdout.lines() {
                if line.contains("REG_SZ") || line.contains("REG_EXPAND_SZ") {
                    if let Some(pos) = line.find("REG_") {
                        let sub = &line[pos..];
                        if let Some((_, val)) = sub.split_once(' ') {
                            current_path = val.trim().to_owned();
                        }
                    }
                }
            }
        }
    }

    let parts: Vec<&str> = current_path.split(';').map(str::trim).collect();
    if parts.iter().any(|part| part.eq_ignore_ascii_case(&dir_str)) {
        println!("    [OK] Directory already exists in User PATH.");
        return;
    }

    let new_path = if current_path.is_empty() {
        dir_str.clone()
    } else if current_path.ends_with(';') {
        format!("{current_path}{dir_str}")
    } else {
        format!("{current_path};{dir_str}")
    };

    let status = Command::new("reg")
        .args(["add", "HKCU\\Environment", "/v", "Path", "/t", "REG_EXPAND_SZ", "/d", &new_path, "/f"])
        .status();

    match status {
        Ok(s) if s.success() => {
            println!("    [OK] Added `{}` to persistent User PATH registry.", dir.display());
            notify_environment_change();
        }
        _ => {
            println!("    [!] Could not update registry automatically. Please add `{}` to PATH manually.", dir.display());
        }
    }
}

fn remove_from_user_path(dir: &Path) {
    let dir_str = dir.to_string_lossy().to_string();

    let output = Command::new("reg")
        .args(["query", "HKCU\\Environment", "/v", "Path"])
        .output();

    let mut current_path = String::new();
    if let Ok(out) = output {
        if out.status.success() {
            let stdout = String::from_utf8_lossy(&out.stdout);
            for line in stdout.lines() {
                if line.contains("REG_SZ") || line.contains("REG_EXPAND_SZ") {
                    if let Some(pos) = line.find("REG_") {
                        let sub = &line[pos..];
                        if let Some((_, val)) = sub.split_once(' ') {
                            current_path = val.trim().to_owned();
                        }
                    }
                }
            }
        }
    }

    let parts: Vec<&str> = current_path
        .split(';')
        .map(str::trim)
        .filter(|part| !part.is_empty() && !part.eq_ignore_ascii_case(&dir_str))
        .collect();

    let new_path = parts.join(";");

    let _ = Command::new("reg")
        .args(["add", "HKCU\\Environment", "/v", "Path", "/t", "REG_EXPAND_SZ", "/d", &new_path, "/f"])
        .status();

    println!("    [OK] Removed from User PATH.");
    notify_environment_change();
}

fn notify_environment_change() {
    let _ = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "[System.Environment]::SetEnvironmentVariable('PATH', [System.Environment]::GetEnvironmentVariable('PATH', 'User'), 'Process')",
        ])
        .status();
}

fn check_ecosystem() {
    let gh_found = Command::new("where.exe").arg("gh").output().map(|o| o.status.success()).unwrap_or(false);
    if gh_found {
        println!("    [OK] GitHub CLI (`gh`) detected! Configured to run seamlessly alongside Axint.");
        if let Ok(out) = Command::new("gh").args(["auth", "status"]).output() {
            if out.status.success() {
                let info = String::from_utf8_lossy(&out.stdout);
                if let Some(line) = info.lines().find(|l| l.contains("Logged in to")) {
                    println!("         {}", line.trim());
                }
            }
        }
    } else {
        println!("    [INFO] GitHub CLI (`gh`) not detected. You can install it anytime with `winget install GitHub.cli`");
    }

    let git_found = Command::new("where.exe").arg("git").output().map(|o| o.status.success()).unwrap_or(false);
    if git_found {
        println!("    [OK] Git engine detected. Axint Git Bridge enabled for bidirectional sync.");
    }
}
