//! Runs a chosen action on macOS: AppleScript through `osascript`, Lua through
//! Hammerspoon's `hs` CLI, shell commands, URLs and apps through `open`, and
//! menu commands through the Accessibility API.
//!
//! Arguments come from speech, so they never become code: AppleScript gets
//! them as an escaped string literal, Lua as a long-bracket string, the shell
//! as `$1`, and URLs percent-encoded. A menu command is pressed only if its
//! label matches one that code read from the app.

use super::context::DesktopContext;
use super::elements;
use super::menus;
use super::registry::{Action, Runner};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Long enough for a first-run Automation permission prompt to be answered.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(15);
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);

/// What running an action asks of the caller.
#[derive(Debug, PartialEq)]
pub enum Effect {
    Done,
    /// Paste this text into the frontmost app (the caller owns pasting).
    TypeText(String),
}

pub fn run(action: &Action, arg: Option<&str>, ctx: &DesktopContext) -> Result<Effect, String> {
    if let Runner::TypeText = action.runner {
        return match arg {
            Some(text) if !text.trim().is_empty() => Ok(Effect::TypeText(text.to_string())),
            _ => Err("nothing to type".to_string()),
        };
    }
    if !cfg!(target_os = "macos") {
        return Err("voice commands can only run actions on macOS".to_string());
    }

    let arg = arg.unwrap_or("");
    match &action.runner {
        Runner::AppleScript(source) => {
            run_process(
                Command::new("osascript"),
                Some(&applescript_with_arg(source, arg)),
                SCRIPT_TIMEOUT,
            )?;
        }
        Runner::Hammerspoon(lua) => {
            let cli = ctx
                .hammerspoon_cli
                .clone()
                .ok_or("Hammerspoon's hs CLI is not installed")?;
            let mut hs = Command::new(cli);
            hs.arg("-c").arg(lua_with_arg(lua, arg));
            run_process(hs, None, SCRIPT_TIMEOUT)?;
        }
        Runner::Shell(script) => {
            let mut sh = Command::new("/bin/sh");
            sh.arg("-c").arg(script).arg("handy").arg(arg);
            run_process(sh, None, SCRIPT_TIMEOUT)?;
        }
        Runner::OpenUrl(template) => open(&template.replace("{text}", &percent_encode(arg)))?,
        Runner::OpenWebsite => open(&website_url(arg))?,
        Runner::OpenApp => {
            let mut open_app = Command::new("open");
            open_app.arg("-a").arg(arg);
            run_process(open_app, None, OPEN_TIMEOUT)?;
        }
        Runner::MenuItem => {
            let item = ctx
                .menu_items
                .iter()
                .find(|item| item.label() == arg)
                .ok_or("that menu command is no longer available")?;
            let pid = ctx.frontmost_pid.ok_or("no frontmost app")?;
            menus::press(pid, &item.path)?;
        }
        Runner::ClickPointer => {
            elements::click_pointer()?;
        }
        Runner::ClickElement => {
            return Err("clicking by name needs its target picked first".to_string());
        }
        Runner::TypeText => unreachable!("handled above"),
    }
    Ok(Effect::Done)
}

fn open(target: &str) -> Result<(), String> {
    let mut open = Command::new("open");
    open.arg(target);
    run_process(open, None, OPEN_TIMEOUT).map(|_| ())
}

/// Run a process to completion (or kill it at `timeout`), feeding `stdin` if
/// given. Returns trimmed stdout, or stderr as the error.
pub fn run_process(
    mut command: Command,
    stdin: Option<&str>,
    timeout: Duration,
) -> Result<String, String> {
    let program = command.get_program().to_string_lossy().into_owned();
    command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| format!("could not start {program}: {e}"))?;

    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        // A process that exits without reading its input reports its own error.
        let _ = pipe.write_all(input.as_bytes());
    }

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{program} timed out after {}s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(format!("{program} failed: {e}")),
        }
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("{program} failed: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        Err(if detail.is_empty() {
            format!("{program} exited with {}", output.status)
        } else {
            format!("{program}: {detail}")
        })
    }
}

/// AppleScript string literal: only `\` and `"` can end or escape it.
fn applescript_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Make the argument available to the script as the property `arg`, which
/// top-level statements and handlers can both read.
fn applescript_with_arg(source: &str, arg: &str) -> String {
    format!("property arg : {}\n{source}", applescript_string(arg))
}

/// Lua long-bracket string, with enough `=` that the text can't close it.
fn lua_string(text: &str) -> String {
    let mut level = 0;
    while text.contains(&format!("]{}]", "=".repeat(level))) {
        level += 1;
    }
    let eq = "=".repeat(level);
    // A newline right after the opening bracket is dropped by Lua; add one so a
    // leading newline in the text survives.
    format!("[{eq}[\n{text}]{eq}]")
}

fn lua_with_arg(lua: &str, arg: &str) -> String {
    format!("local arg = {}\n{lua}", lua_string(arg))
}

/// RFC 3986 unreserved characters pass through; everything else is %-encoded.
pub fn percent_encode(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// "github.com" and "github dot com" open directly; a name like "the verge"
/// goes to DuckDuckGo's "I'm Feeling Ducky" (a leading `\`), which redirects
/// to the top result.
pub fn website_url(spoken: &str) -> String {
    let site = spoken.trim().trim_end_matches('/');
    if site.starts_with("http://") || site.starts_with("https://") {
        return site.to_string();
    }
    let dotted = site
        .to_lowercase()
        .replace(" dot ", ".")
        .replace(". ", ".")
        .replace(" .", ".");
    if !dotted.is_empty() && !dotted.contains(' ') && dotted.contains('.') {
        return format!("https://{dotted}");
    }
    format!(
        "https://duckduckgo.com/?q={}",
        percent_encode(&format!("\\{site}"))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice_control::registry::ArgKind;

    fn action(runner: Runner) -> Action {
        Action {
            id: "test".into(),
            title: "Test".into(),
            what: "test".into(),
            not_for: None,
            arg: ArgKind::Text,
            runner,
        }
    }

    #[test]
    fn applescript_arguments_cannot_break_out_of_the_literal() {
        let script =
            applescript_with_arg("return arg", r#"a" & (do shell script "rm -rf ~") & "\"#);
        assert_eq!(
            script.lines().next().unwrap(),
            r#"property arg : "a\" & (do shell script \"rm -rf ~\") & \"\\""#
        );
    }

    #[test]
    fn lua_arguments_cannot_close_their_long_bracket() {
        assert_eq!(lua_string("plain"), "[[\nplain]]");
        assert_eq!(lua_string("a]]b"), "[=[\na]]b]=]");
        assert_eq!(lua_string("a]]b]=]c"), "[==[\na]]b]=]c]==]");
        assert!(lua_with_arg("print(arg)", "x").starts_with("local arg = [[\nx]]\n"));
    }

    #[test]
    fn percent_encodes_queries() {
        assert_eq!(percent_encode("flights to Denver"), "flights%20to%20Denver");
        assert_eq!(percent_encode("a&b=c/d?"), "a%26b%3Dc%2Fd%3F");
        assert_eq!(percent_encode("café"), "caf%C3%A9");
    }

    #[test]
    fn websites_open_directly_or_through_the_top_result() {
        assert_eq!(website_url("github.com"), "https://github.com");
        assert_eq!(website_url("GitHub dot com"), "https://github.com");
        assert_eq!(website_url("https://example.org/"), "https://example.org");
        assert_eq!(
            website_url("the verge"),
            "https://duckduckgo.com/?q=%5Cthe%20verge"
        );
    }

    #[test]
    fn type_text_hands_the_text_back_on_any_platform() {
        let ctx = DesktopContext::default();
        assert_eq!(
            run(&action(Runner::TypeText), Some("hello world"), &ctx),
            Ok(Effect::TypeText("hello world".into()))
        );
        assert!(run(&action(Runner::TypeText), Some("  "), &ctx).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn run_process_returns_output_and_errors() {
        let mut echo = Command::new("/bin/sh");
        echo.args(["-c", "cat; echo \" $1\"", "sh", "arg"]);
        assert_eq!(
            run_process(echo, Some("from stdin"), Duration::from_secs(5)),
            Ok("from stdin arg".to_string())
        );

        let mut fail = Command::new("/bin/sh");
        fail.args(["-c", "echo nope >&2; exit 3"]);
        assert_eq!(
            run_process(fail, None, Duration::from_secs(5)),
            Err("/bin/sh: nope".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_process_kills_hung_processes() {
        let mut sleep = Command::new("/bin/sh");
        sleep.args(["-c", "sleep 5"]);
        let started = Instant::now();
        let result = run_process(sleep, None, Duration::from_millis(200));
        assert!(result.unwrap_err().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
