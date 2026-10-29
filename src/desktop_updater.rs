use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use reqwest::blocking::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MB_YESNO, MessageBoxW,
};

const MANIFEST_URL: &str =
    "https://raw.githubusercontent.com/Cypherusuh/CypherSR/main/latest.json";

const DESKTOP_EXE_NAME: &str = "CypherSR-Desktop.exe";

const DESKTOP_UPDATE_DIR: &str = ".tmp\\desktop-update";
const STAGED_EXE_NAME: &str = "CypherSR-Desktop.exe";
const PARTIAL_EXE_NAME: &str = "CypherSR-Desktop.exe.part";
const UPDATE_SCRIPT_NAME: &str = "replace-desktop.ps1";

#[derive(Debug, Deserialize)]
struct LatestManifest {
    desktop: DesktopUpdateManifest,
}

#[derive(Debug, Deserialize)]
struct DesktopUpdateManifest {
    version: String,
    url: String,
    sha256: String,
}

/// Returns true when an update has been staged and the app should exit
/// so the external updater can replace the running executable.
pub fn check_and_apply_update() -> bool {
    let current_version = env!("CARGO_PKG_VERSION");

    let client = match Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(format!("CypherSR-Desktop/{current_version}"))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            eprintln!("[DesktopUpdate] Could not create HTTP client: {error}");
            return false;
        }
    };

    let manifest = match client.get(MANIFEST_URL).send() {
        Ok(response) => match response.error_for_status() {
            Ok(response) => match response.json::<LatestManifest>() {
                Ok(manifest) => manifest.desktop,
                Err(error) => {
                    eprintln!("[DesktopUpdate] Invalid latest.json: {error}");
                    return false;
                }
            },
            Err(error) => {
                eprintln!("[DesktopUpdate] Manifest request failed: {error}");
                return false;
            }
        },
        Err(error) => {
            eprintln!("[DesktopUpdate] Could not check for updates: {error}");
            return false;
        }
    };

    if !is_newer_version(&manifest.version, current_version) {
        println!(
            "[DesktopUpdate] Current version {current_version}; latest version {}.",
            manifest.version
        );
        return false;
    }

    println!(
        "[DesktopUpdate] Update available: {current_version} -> {}",
        manifest.version
    );

    let prompt = format!(
        "A new CypherSR Desktop version is available.\n\n\
         Installed version: {current_version}\n\
         Available version: {}\n\n\
         Download and install it now?",
        manifest.version
    );

    if !ask_yes_no(&prompt) {
        return false;
    }

    match download_and_stage_update(&client, &manifest) {
        Ok(()) => true,
        Err(error) => {
            show_error(&format!(
                "Could not install the desktop update.\n\n{error}\n\n\
                 The current version will continue running."
            ));
            false
        }
    }
}

fn download_and_stage_update(
    client: &Client,
    manifest: &DesktopUpdateManifest,
) -> Result<(), Box<dyn std::error::Error>> {
    let executable = std::env::current_exe()?;

    let install_dir = executable.parent().ok_or_else(|| {
        io::Error::other(
            "Could not determine the desktop installation directory",
        )
    })?;

    if executable
        .file_name()
        .and_then(|name| name.to_str())
        != Some(DESKTOP_EXE_NAME)
    {
        return Err(io::Error::other(format!(
            "The running executable is not named {DESKTOP_EXE_NAME}. \
             Current path: {}",
            executable.display()
        ))
        .into());
    }

    let update_dir = install_dir.join(DESKTOP_UPDATE_DIR);

    fs::create_dir_all(&update_dir)?;

    let staged_executable = update_dir.join(STAGED_EXE_NAME);
    let partial_executable = update_dir.join(PARTIAL_EXE_NAME);
    let script_path = update_dir.join(UPDATE_SCRIPT_NAME);

    // Remove stale files from a previous successful/failed update attempt.
    let _ = fs::remove_file(&staged_executable);
    let _ = fs::remove_file(&partial_executable);
    let _ = fs::remove_file(&script_path);

    println!(
        "[DesktopUpdate] Downloading version {} from {}",
        manifest.version, manifest.url
    );

    let mut response = client.get(&manifest.url).send()?.error_for_status()?;

    let mut output = fs::File::create(&partial_executable)?;
    let mut hasher = Sha256::new();

    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0_u64;

    loop {
        let count = response.read(&mut buffer)?;

        if count == 0 {
            break;
        }

        total = total
            .checked_add(count as u64)
            .ok_or_else(|| io::Error::other("Desktop update size overflow"))?;

        output.write_all(&buffer[..count])?;
        hasher.update(&buffer[..count]);
    }

    output.flush()?;
    output.sync_all()?;
    drop(output);

    if total == 0 {
        let _ = fs::remove_file(&partial_executable);

        return Err(
            io::Error::other("The downloaded executable is empty").into(),
        );
    }

    let actual_sha256 = format!("{:x}", hasher.finalize());

    if !actual_sha256.eq_ignore_ascii_case(manifest.sha256.trim()) {
        let _ = fs::remove_file(&partial_executable);

        return Err(io::Error::other(format!(
            "SHA-256 verification failed.\nExpected: {}\nReceived: {}",
            manifest.sha256, actual_sha256
        ))
        .into());
    }

    // The download is now verified. Promote it from .part to the
    // final staged executable.
    fs::rename(&partial_executable, &staged_executable)?;

    println!(
        "[DesktopUpdate] Verified update and staged {}",
        staged_executable.display()
    );

    if let Err(error) = start_replacement_script(
        &executable,
        &staged_executable,
        std::process::id(),
        &script_path,
    ) {
        let _ = fs::remove_file(&staged_executable);
        let _ = fs::remove_file(&script_path);

        return Err(error.into());
    }

    println!("[DesktopUpdate] Handing off replacement to PowerShell.");

    Ok(())
}

fn start_replacement_script(
    target_executable: &Path,
    new_executable: &Path,
    process_id: u32,
    script_path: &Path,
) -> io::Result<()> {
    let script = r#"
param(
    [Parameter(Mandatory = $true)][int]$ProcessId,
    [Parameter(Mandatory = $true)][string]$NewExe,
    [Parameter(Mandatory = $true)][string]$TargetExe,
    [Parameter(Mandatory = $true)][string]$UpdateDir
)

$ErrorActionPreference = 'Stop'

try {
    # Wait until the current Desktop process exits.
    Wait-Process -Id $ProcessId -ErrorAction SilentlyContinue

    Start-Sleep -Milliseconds 500

    if (-not (Test-Path -LiteralPath $NewExe -PathType Leaf)) {
        throw "Staged executable not found: $NewExe"
    }

    # Replace the old Desktop executable.
    Move-Item `
        -LiteralPath $NewExe `
        -Destination $TargetExe `
        -Force

    # Start the updated Desktop.
    Start-Process `
        -FilePath $TargetExe `
        -WorkingDirectory (Split-Path -Parent $TargetExe)

    # Cleanup the temporary update directory after the new
    # process has successfully been started.
    Start-Sleep -Milliseconds 500

    Remove-Item `
        -LiteralPath $UpdateDir `
        -Recurse `
        -Force `
        -ErrorAction SilentlyContinue
}
catch {
    $logPath = Join-Path $UpdateDir 'update-error.log'

    try {
        $_ | Out-String | Set-Content -LiteralPath $logPath
    }
    catch {
        # Do not hide the original update failure.
    }

    Add-Type -AssemblyName PresentationFramework

    [System.Windows.MessageBox]::Show(
        "The desktop update could not be completed.`n`n$($_.Exception.Message)`n`nTemporary files were retained in:`n$UpdateDir",
        'CypherSR Desktop Updater',
        'OK',
        'Error'
    ) | Out-Null
}
"#;

    fs::write(script_path, script)?;

    let update_dir = script_path.parent().ok_or_else(|| {
        io::Error::other("Could not determine Desktop update directory")
    })?;

    Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(script_path)
        .arg("-ProcessId")
        .arg(process_id.to_string())
        .arg("-NewExe")
        .arg(new_executable)
        .arg("-TargetExe")
        .arg(target_executable)
        .arg("-UpdateDir")
        .arg(update_dir)
        .spawn()?;

    Ok(())
}

fn is_newer_version(candidate: &str, current: &str) -> bool {
    fn parse_version(version: &str) -> Option<(u64, u64, u64)> {
        let mut parts = version.trim().split('.');

        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;

        if parts.next().is_some() {
            return None;
        }

        Some((major, minor, patch))
    }

    match (parse_version(candidate), parse_version(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => {
            eprintln!(
                "[DesktopUpdate] Invalid version format: candidate={candidate:?}, \
                 current={current:?}"
            );
            false
        }
    }
}

fn ask_yes_no(message: &str) -> bool {
    let title = to_wide("CypherSR Desktop");
    let message = to_wide(message);

    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            message.as_ptr(),
            title.as_ptr(),
            MB_YESNO | MB_ICONINFORMATION,
        ) == 6 // IDYES
    }
}

fn show_error(message: &str) {
    let title = to_wide("CypherSR Desktop Updater");
    let message = to_wide(message);

    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            message.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn to_wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}