use crate::{
    process::{run_command, run_command_until, run_command_without_stderr},
    vocabulary::whitespace,
};
use anyhow::{Result, ensure};
use serde_json::Value;
use std::{process::Command, sync::Mutex, thread, time::Duration};

// OpenWhispr's Linux terminal classes use Ctrl+Shift+V instead of Ctrl+V.
const TERMINALS: &[&str] = &[
    "konsole",
    "gnome-terminal",
    "terminal",
    "kitty",
    "alacritty",
    "terminator",
    "xterm",
    "urxvt",
    "rxvt",
    "tilix",
    "terminology",
    "wezterm",
    "foot",
    "st",
    "yakuake",
    "ghostty",
    "guake",
    "tilda",
    "hyper",
    "tabby",
    "sakura",
    "warp",
    "termius",
    "waveterm",
    "ptyxis",
    "kgx",
    "org.gnome.console",
    "cosmicterm",
    "xst",
    "stterm",
];
const RESTORE_DELAY: Duration = Duration::from_millis(800);
const CLIPBOARD_LIMIT: usize = 32 * 1024 * 1024;
const TEXT_MIME: &str = "text/plain;charset=utf-8";
struct Selection {
    generation: u64,
    pending: Option<Pending>,
}

struct Pending {
    original: Clipboard,
    transcript: String,
    generation: u64,
}

static SELECTION: Mutex<Selection> = Mutex::new(Selection {
    generation: 0,
    pending: None,
});
static RESTORERS: Mutex<Vec<thread::JoinHandle<()>>> = Mutex::new(Vec::new());

pub fn focused_window() -> Option<Value> {
    let output = run_command(
        Command::new("niri").args(["msg", "--json", "focused-window"]),
        None,
        Duration::from_secs(2),
    )
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let window: Value = serde_json::from_slice(&output.stdout).ok()?;
    window_id(&window)?;
    Some(window)
}

pub fn is_terminal(app_id: &str) -> bool {
    let app_id = app_id.to_lowercase();
    TERMINALS.iter().any(|name| {
        if name.len() > 2 {
            app_id.contains(name)
        } else {
            app_id
                .split(|character: char| !character.is_ascii_alphanumeric())
                .any(|word| word == *name)
        }
    })
}

fn window_id(window: &Value) -> Option<u64> {
    window.get("id")?.as_u64()
}

fn same_focus(id: u64) -> bool {
    focused_window().as_ref().and_then(window_id) == Some(id)
}

fn refocus(id: u64) -> bool {
    let result = run_command(
        Command::new("niri").args(["msg", "action", "focus-window", "--id", &id.to_string()]),
        None,
        Duration::from_secs(2),
    );
    if result.is_ok_and(|output| output.status.success()) {
        thread::sleep(Duration::from_millis(100));
        return same_focus(id);
    }
    false
}

fn write_clipboard(args: &[&str], data: Option<&[u8]>) -> Result<()> {
    let mut command = Command::new("wl-copy");
    command.env("CLIPBOARD_STATE", "sensitive");
    if data.is_some() {
        command.arg("--sensitive");
    }
    // wl-copy keeps stderr open in the daemon that owns the clipboard.
    let output = run_command_without_stderr(command.args(args), data, Duration::from_secs(3))?;
    ensure!(
        output.status.success(),
        "wl-copy failed with {}",
        output.status
    );
    Ok(())
}

fn copy_selection(text: &str, primary: bool) -> Result<()> {
    let args = ["--type", TEXT_MIME, "--primary"];
    write_clipboard(&args[..if primary { 3 } else { 2 }], Some(text.as_bytes()))
}

pub fn copy_text(text: &str, primary: bool) -> Result<()> {
    let mut selection = SELECTION.lock().unwrap_or_else(|error| error.into_inner());
    copy_selection(text, primary)?;
    if !primary {
        selection.generation = selection.generation.wrapping_add(1);
        selection.pending = None;
    }
    Ok(())
}

enum Clipboard {
    Empty,
    Content(String, Vec<u8>),
}

fn read_selection(kind: &str) -> Option<Vec<u8>> {
    let output = run_command(
        Command::new("wl-paste").args(["--no-newline", "--type", kind]),
        None,
        Duration::from_secs(1),
    )
    .ok()?;
    (output.status.success() && output.stdout.len() <= CLIPBOARD_LIMIT).then_some(output.stdout)
}

fn previous_selection() -> Option<Clipboard> {
    let listing = run_command(
        Command::new("wl-paste")
            .args(["--list-types"])
            .env("LC_ALL", "C"),
        None,
        Duration::from_secs(1),
    )
    .ok()?;
    if !listing.status.success() {
        return (String::from_utf8_lossy(&listing.stderr).trim() == "Nothing is copied")
            .then_some(Clipboard::Empty);
    }
    let names = std::str::from_utf8(&listing.stdout).ok()?;
    let Some(kind) = names
        .split_whitespace()
        .find(|name| name.starts_with("text/plain"))
        .or_else(|| names.split_whitespace().next())
    else {
        return Some(Clipboard::Empty);
    };
    let data = read_selection(kind)?;
    Some(Clipboard::Content(kind.to_owned(), data))
}

fn still_selected(transcript: &str) -> bool {
    read_selection("text").as_deref() == Some(transcript.as_bytes())
}

fn restore_after_paste(previous: Option<Clipboard>, transcript: String, selection: &mut Selection) {
    let Some(previous) = previous else { return };
    let generation = selection.generation;
    selection.pending = Some(Pending {
        original: previous,
        transcript,
        generation,
    });
    let handle = thread::spawn(move || {
        thread::sleep(RESTORE_DELAY);
        let mut selection = SELECTION.lock().unwrap_or_else(|error| error.into_inner());
        if selection
            .pending
            .as_ref()
            .is_none_or(|pending| pending.generation != generation)
        {
            return;
        }
        let pending = selection.pending.take().expect("matching pending restore");
        if !still_selected(&pending.transcript) {
            return;
        }
        let _ = match pending.original {
            Clipboard::Empty => write_clipboard(&["--clear"], None),
            Clipboard::Content(kind, data) => write_clipboard(&["--type", &kind], Some(&data)),
        };
    });
    let mut restorers = RESTORERS.lock().unwrap_or_else(|error| error.into_inner());
    restorers.retain(|restorer| !restorer.is_finished());
    restorers.push(handle);
}

pub fn finish_pending() {
    let handles = std::mem::take(&mut *RESTORERS.lock().unwrap_or_else(|error| error.into_inner()));
    for handle in handles {
        let _ = handle.join();
    }
}

fn type_keys(args: &[&str], cancelled: &impl Fn() -> bool) -> Result<()> {
    let output = run_command_until(
        Command::new("wtype").args(args),
        None,
        Duration::from_secs(3),
        true,
        cancelled,
    )?;
    ensure!(
        output.status.success(),
        "wtype failed with {}",
        output.status
    );
    Ok(())
}

pub fn deliver(
    text: &str,
    target: Option<&Value>,
    paste: bool,
    trailing_space: bool,
    press_enter: bool,
) -> Result<String> {
    deliver_until(text, target, paste, trailing_space, press_enter, || false)
}

pub fn deliver_until(
    text: &str,
    target: Option<&Value>,
    paste: bool,
    trailing_space: bool,
    press_enter: bool,
    cancelled: impl Fn() -> bool,
) -> Result<String> {
    ensure!(!cancelled(), "Clipboard delivery cancelled");
    let mut transcript = text.to_owned();
    if trailing_space && !text.ends_with(whitespace) {
        transcript.push(' ');
    }
    let mut selection = SELECTION.lock().unwrap_or_else(|error| error.into_inner());
    ensure!(!cancelled(), "Clipboard delivery cancelled");
    let inherited = paste
        && selection
            .pending
            .as_ref()
            .is_some_and(|pending| still_selected(&pending.transcript));
    let previous = if paste && !inherited {
        previous_selection()
    } else {
        None
    };
    copy_selection(&transcript, false)?;
    let previous = if inherited {
        selection.pending.take().map(|pending| pending.original)
    } else {
        previous
    };
    selection.pending = None;
    selection.generation = selection.generation.wrapping_add(1);
    if !paste {
        copy_selection(&transcript, true)?;
        return Ok("Copied".to_owned());
    }
    // The compositor must release the shortcut modifiers before synthesized keys.
    thread::sleep(Duration::from_millis(180));
    ensure!(!cancelled(), "Clipboard delivery cancelled");
    let id = target.and_then(window_id);
    if !id.is_some_and(|id| same_focus(id) || refocus(id)) {
        copy_selection(&transcript, true)?;
        return Ok("Copied · focus changed".to_owned());
    }
    if !still_selected(&transcript) {
        copy_selection(&transcript, true)?;
        return Ok("Copied · clipboard changed".to_owned());
    }
    if !id.is_some_and(same_focus) {
        copy_selection(&transcript, true)?;
        return Ok("Copied · focus changed".to_owned());
    }
    ensure!(!cancelled(), "Clipboard delivery cancelled");
    let terminal = target
        .and_then(|window| window.get("app_id"))
        .and_then(Value::as_str)
        .is_some_and(is_terminal);
    if terminal {
        type_keys(
            &[
                "-M", "ctrl", "-M", "shift", "-P", "v", "-p", "v", "-m", "shift", "-m", "ctrl",
            ],
            &cancelled,
        )?;
    } else {
        type_keys(
            &["-M", "ctrl", "-P", "v", "-p", "v", "-m", "ctrl"],
            &cancelled,
        )?;
    }
    restore_after_paste(previous, transcript, &mut selection);
    let submitted = if press_enter && !terminal {
        thread::sleep(Duration::from_millis(50));
        if !cancelled() && id.is_some_and(same_focus) && !cancelled() {
            type_keys(&["-k", "Return"], &cancelled)?;
            true
        } else {
            false
        }
    } else {
        false
    };
    Ok(if submitted {
        "Pasted and sent"
    } else {
        "Pasted"
    }
    .to_owned())
}
