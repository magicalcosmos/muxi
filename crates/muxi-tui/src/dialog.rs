//! Native OS directory picker used when adding a workspace.
//!
//! The picker runs as an external process while the TUI is suspended. Each
//! platform provides one or more candidate pickers; when none is available
//! (headless session, missing tool) or the user cancels, this returns `None`
//! instead of failing the add flow.

use std::path::PathBuf;
use std::process::Command;

/// Opens the platform directory picker. Returns the selected directory, or
/// `None` when the user cancelled or no picker could be launched.
pub fn pick_directory() -> Option<PathBuf> {
    let raw = run_picker()?;
    parse_pick_output(&raw)
}

/// Runs candidate pickers in order: a picker that exits non-zero means the
/// user cancelled (stop), while a launch failure means it is not installed
/// (try the next).
fn run_picker() -> Option<Vec<u8>> {
    for mut command in picker_commands() {
        match command.output() {
            Ok(output) if output.status.success() => {
                if output.stdout.iter().any(|byte| !byte.is_ascii_whitespace()) {
                    return Some(output.stdout);
                }
                return None;
            }
            Ok(_) => return None,
            Err(_) => {}
        }
    }
    None
}

#[cfg(windows)]
fn picker_commands() -> Vec<Command> {
    let mut command = Command::new("powershell");
    command
        .args(["-NoProfile", "-NonInteractive", "-STA", "-Command"])
        .arg(
            // UTF-8 output keeps non-ASCII paths intact regardless of the
            // system code page.
            r"[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
Add-Type -AssemblyName System.Windows.Forms | Out-Null
$dialog = New-Object System.Windows.Forms.FolderBrowserDialog
$dialog.Description = 'Select workspace folder'
$dialog.ShowNewFolderButton = $false
if ($dialog.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {
    [Console]::Out.Write($dialog.SelectedPath)
}",
        );
    vec![command]
}

#[cfg(target_os = "macos")]
fn picker_commands() -> Vec<Command> {
    let mut command = Command::new("osascript");
    command
        .arg("-e")
        .arg(r#"POSIX path of (choose folder with prompt "Select workspace folder")"#);
    vec![command]
}

#[cfg(all(unix, not(target_os = "macos")))]
fn picker_commands() -> Vec<Command> {
    let mut zenity = Command::new("zenity");
    zenity
        .args(["--file-selection", "--directory", "--title"])
        .arg("Select workspace folder");
    let mut kdialog = Command::new("kdialog");
    kdialog
        .arg("--getexistingdirectory")
        .arg(".")
        .arg("--title")
        .arg("Select workspace folder");
    vec![zenity, kdialog]
}

#[cfg(not(any(windows, target_os = "macos", all(unix, not(target_os = "macos")))))]
fn picker_commands() -> Vec<Command> {
    Vec::new()
}

/// Extracts a single non-empty line as the picked path.
fn parse_pick_output(raw: &[u8]) -> Option<PathBuf> {
    let text = String::from_utf8_lossy(raw).trim().to_owned();
    if text.is_empty() || text.contains('\n') {
        return None;
    }
    Some(PathBuf::from(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_line_output() {
        assert_eq!(
            parse_pick_output(b"E:\\lab\\agent\r\n"),
            Some(PathBuf::from("E:\\lab\\agent"))
        );
        assert_eq!(
            parse_pick_output("/tmp/demo\n".as_bytes()),
            Some(PathBuf::from("/tmp/demo"))
        );
    }

    #[test]
    fn empty_and_multiline_output_are_cancellations() {
        assert_eq!(parse_pick_output(b""), None);
        assert_eq!(parse_pick_output(b"\n"), None);
        assert_eq!(parse_pick_output(b"a\nb\n"), None);
    }
}
