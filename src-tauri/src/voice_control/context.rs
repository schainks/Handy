//! What code knows about the desktop when a command is spoken: the frontmost
//! app (context for "command or dictation?"), the running and installed apps
//! (candidates for app arguments), and whether Hammerspoon's CLI is installed.

use super::executor::run_process;
use log::debug;
use serde::Deserialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

#[derive(Debug, Clone, Default)]
pub struct DesktopContext {
    pub frontmost_app: Option<String>,
    pub running_apps: Vec<String>,
    pub installed_apps: Vec<String>,
    pub hammerspoon_cli: Option<PathBuf>,
}

/// `hs.ipc.cliInstall()` puts the CLI in one of these (Apple Silicon Homebrew
/// prefix, or the Intel default). GUI apps don't get the shell's PATH, so the
/// locations are checked directly.
const HAMMERSPOON_CLI_PATHS: &[&str] = &["/opt/homebrew/bin/hs", "/usr/local/bin/hs"];

/// Folders scanned for `.app` bundles, one level of subfolders included
/// (Utilities, Setapp, vendor suites).
const APP_FOLDERS: &[&str] = &["/Applications", "/System/Applications"];

/// Frontmost and running regular (Dock) apps via NSWorkspace, which needs no
/// Automation permission.
const RUNNING_APPS_JXA: &str = r#"ObjC.import("AppKit");
const ws = $.NSWorkspace.sharedWorkspace;
const front = ws.frontmostApplication;
const apps = ws.runningApplications;
const running = [];
for (let i = 0; i < apps.count; i++) {
  const app = apps.objectAtIndex(i);
  const name = ObjC.unwrap(app.localizedName);
  if (app.activationPolicy === 0 && name) running.push(name);
}
JSON.stringify({ frontmost: front.isNil() ? null : ObjC.unwrap(front.localizedName), running: running });"#;

#[derive(Deserialize)]
struct RunningApps {
    frontmost: Option<String>,
    #[serde(default)]
    running: Vec<String>,
}

impl DesktopContext {
    /// Snapshot the desktop. Blocking (spawns `osascript`, reads folders), so
    /// callers run it off the async runtime, concurrently with transcription.
    pub fn capture() -> Self {
        if !cfg!(target_os = "macos") {
            return Self::default();
        }

        let mut ctx = Self {
            installed_apps: installed_apps(),
            hammerspoon_cli: hammerspoon_cli(),
            ..Default::default()
        };

        let mut jxa = Command::new("osascript");
        jxa.args(["-l", "JavaScript"]);
        match run_process(jxa, Some(RUNNING_APPS_JXA), Duration::from_secs(3))
            .and_then(|out| serde_json::from_str::<RunningApps>(&out).map_err(|e| e.to_string()))
        {
            Ok(apps) => {
                ctx.frontmost_app = apps.frontmost;
                ctx.running_apps = apps.running;
            }
            Err(e) => debug!("Could not list running apps: {e}"),
        }

        ctx
    }
}

pub fn hammerspoon_cli() -> Option<PathBuf> {
    HAMMERSPOON_CLI_PATHS
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
}

fn installed_apps() -> Vec<String> {
    let mut names = BTreeSet::new();
    let mut folders: Vec<PathBuf> = APP_FOLDERS.iter().map(PathBuf::from).collect();
    if let Some(home) = std::env::var_os("HOME") {
        folders.push(Path::new(&home).join("Applications"));
    }
    for folder in &folders {
        collect_apps(folder, 1, &mut names);
    }
    // Finder lives in CoreServices, next to dozens of agents nobody opens.
    names.insert("Finder".to_string());
    names.into_iter().collect()
}

fn collect_apps(folder: &Path, depth: usize, names: &mut BTreeSet<String>) {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match app_name(&path) {
            Some(name) => {
                names.insert(name);
            }
            None if depth > 0 && path.is_dir() => collect_apps(&path, depth - 1, names),
            None => {}
        }
    }
}

fn app_name(path: &Path) -> Option<String> {
    if path.extension()? != "app" {
        return None;
    }
    Some(path.file_stem()?.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_apps_one_folder_deep() {
        let root = tempfile::tempdir().unwrap();
        for dir in [
            "Safari.app/Contents",
            "Utilities/Terminal.app",
            "Setapp/CleanShot X.app",
            "Deep/Nested/TooDeep.app",
            "NotAnApp",
        ] {
            std::fs::create_dir_all(root.path().join(dir)).unwrap();
        }

        let mut names = BTreeSet::new();
        collect_apps(root.path(), 1, &mut names);

        assert_eq!(
            names.into_iter().collect::<Vec<_>>(),
            vec!["CleanShot X", "Safari", "Terminal"]
        );
    }
}
