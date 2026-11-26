use crate::{
    process::{run_command, run_command_until, run_command_without_stderr},
    vocabulary::whitespace,
};
use anyhow::{Result, ensure};
use serde_json::Value;
use std::{
    fs::{self, File},
    os::fd::AsRawFd,
    process::Command,
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

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
const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(3);
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
static PRIMARY: Mutex<Selection> = Mutex::new(Selection {
    generation: 0,
    pending: None,
});
static RESTORERS: Mutex<Vec<thread::JoinHandle<()>>> = Mutex::new(Vec::new());

fn selection_state(primary: bool) -> &'static Mutex<Selection> {
    if primary { &PRIMARY } else { &SELECTION }
}

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

pub fn use_shift_insert(app_id: &str) -> bool {
    // OpenWhispr routes VS Code/Cursor away from Ctrl+V, including their embedded chat and terminal inputs.
    matches!(
        app_id.to_ascii_lowercase().as_str(),
        "code"
            | "code-insiders"
            | "code - insiders"
            | "code-oss"
            | "code - oss"
            | "com.microsoft.vscode"
            | "com.microsoft.vscodeinsiders"
            | "cursor"
            | "com.todesktop.230313mzl4w4u92"
            | "codium"
            | "vscodium"
            | "com.vscodium.codium"
    )
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
    let output = run_command_without_stderr(command.args(args), data, CLIPBOARD_TIMEOUT)?;
    ensure!(
        output.status.success(),
        "wl-copy failed with {}",
        output.status
    );
    Ok(())
}

fn copy_selection(text: &str, primary: bool) -> Result<()> {
    let args = ["--primary", "--type", TEXT_MIME];
    write_clipboard(&args[if primary { 0 } else { 1 }..], Some(text.as_bytes()))
}

pub fn copy_text(text: &str, primary: bool) -> Result<()> {
    let mut selection = selection_state(primary)
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    replace_selection(text, primary, false, &mut selection)?;
    Ok(())
}

enum Clipboard {
    Empty,
    Content(String, Vec<u8>),
}

fn read_selection(kind: &str, primary: bool) -> Option<Vec<u8>> {
    let mut command = Command::new("wl-paste");
    if primary {
        command.arg("--primary");
    }
    let output = run_command(
        command.args(["--no-newline", "--type", kind]),
        None,
        CLIPBOARD_TIMEOUT,
    )
    .ok()?;
    output.status.success().then_some(output.stdout)
}

fn previous_selection(primary: bool) -> Option<Clipboard> {
    let mut command = Command::new("wl-paste");
    if primary {
        command.arg("--primary");
    }
    let listing = run_command(
        command.args(["--list-types"]).env("LC_ALL", "C"),
        None,
        CLIPBOARD_TIMEOUT,
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
    let data = read_selection(kind, primary)?;
    Some(Clipboard::Content(kind.to_owned(), data))
}

fn still_selected(transcript: &str, primary: bool) -> bool {
    read_selection("text", primary).as_deref() == Some(transcript.as_bytes())
}

fn replace_selection(
    text: &str,
    primary: bool,
    preserve: bool,
    selection: &mut Selection,
) -> Result<Option<Clipboard>> {
    let inherited = preserve
        && selection
            .pending
            .as_ref()
            .is_some_and(|pending| still_selected(&pending.transcript, primary));
    let previous = if preserve && !inherited {
        previous_selection(primary)
    } else {
        None
    };
    copy_selection(text, primary)?;
    let previous = if inherited {
        selection.pending.take().map(|pending| pending.original)
    } else {
        previous
    };
    selection.pending = None;
    selection.generation = selection.generation.wrapping_add(1);
    Ok(previous)
}

fn restore_after_paste(
    previous: Option<Clipboard>,
    transcript: String,
    selection: &mut Selection,
    primary: bool,
) {
    let Some(previous) = previous else { return };
    let generation = selection.generation;
    selection.pending = Some(Pending {
        original: previous,
        transcript,
        generation,
    });
    let handle = thread::spawn(move || {
        thread::sleep(RESTORE_DELAY);
        let mut selection = selection_state(primary)
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if selection
            .pending
            .as_ref()
            .is_none_or(|pending| pending.generation != generation)
        {
            return;
        }
        let pending = selection.pending.take().expect("matching pending restore");
        if !still_selected(&pending.transcript, primary) {
            return;
        }
        let _ = match pending.original {
            Clipboard::Empty => {
                let args = ["--primary", "--clear"];
                write_clipboard(&args[if primary { 0 } else { 1 }..], None)
            }
            Clipboard::Content(kind, data) => {
                let args = ["--primary", "--type", &kind];
                write_clipboard(&args[if primary { 0 } else { 1 }..], Some(&data))
            }
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

const KEY_BITMAP_BYTES: usize = 96;
const MODIFIER_KEYS: &[usize] = &[29, 42, 54, 56, 97, 100, 125, 126];

fn key_pressed(bitmap: &[u8; KEY_BITMAP_BYTES], key: usize) -> bool {
    bitmap[key / 8] & (1 << (key % 8)) != 0
}

fn keyboard_devices() -> Option<Vec<File>> {
    let mut keyboards = Vec::new();
    for entry in fs::read_dir("/dev/input").ok()? {
        let entry = entry.ok()?;
        if !entry.file_name().to_str()?.starts_with("event") {
            continue;
        }
        // An unreadable device may be the active keyboard; retain the fixed wait.
        let device = File::open(entry.path()).ok()?;
        let mut supported = [0u8; KEY_BITMAP_BYTES];
        // Linux EVIOCGBIT(EV_KEY, 96), read capabilities without consuming events.
        if unsafe {
            libc::ioctl(
                device.as_raw_fd(),
                0x80604521 as libc::c_ulong,
                supported.as_mut_ptr(),
            )
        } < 0
        {
            return None;
        }
        if key_pressed(&supported, 30) && key_pressed(&supported, 44) && key_pressed(&supported, 28)
        {
            keyboards.push(device);
        }
    }
    (!keyboards.is_empty()).then_some(keyboards)
}

fn modifiers_down(keyboards: &[File]) -> Option<bool> {
    let mut held = false;
    for device in keyboards {
        let mut pressed = [0u8; KEY_BITMAP_BYTES];
        // Linux EVIOCGKEY(96); this does not require grabbing the keyboard.
        if unsafe {
            libc::ioctl(
                device.as_raw_fd(),
                0x80604518 as libc::c_ulong,
                pressed.as_mut_ptr(),
            )
        } < 0
        {
            return None;
        }
        held |= MODIFIER_KEYS.iter().any(|&key| key_pressed(&pressed, key));
    }
    Some(held)
}

fn wait_release(
    probe: impl Fn() -> Option<bool>,
    cancelled: &impl Fn() -> bool,
    minimum: Duration,
    maximum: Duration,
    fallback: Duration,
) -> Result<()> {
    let started = Instant::now();
    let mut readable = true;
    loop {
        ensure!(!cancelled(), "Clipboard delivery cancelled");
        let held = if readable { probe() } else { None };
        readable &= held.is_some();
        let elapsed = started.elapsed();
        if readable && elapsed >= minimum && held == Some(false) || !readable && elapsed >= fallback
        {
            return Ok(());
        }
        ensure!(
            elapsed < maximum,
            "Paste cancelled: shortcut modifiers are still pressed"
        );
        thread::sleep(Duration::from_millis(15));
    }
}

fn wait_for_modifiers(enabled: bool, cancelled: &impl Fn() -> bool) -> Result<()> {
    let keyboards = enabled.then(keyboard_devices).flatten();
    wait_release(
        || keyboards.as_deref().and_then(modifiers_down),
        cancelled,
        Duration::from_millis(50),
        Duration::from_millis(1500),
        Duration::from_millis(180),
    )
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
    deliver_guarded_until(
        text,
        target,
        paste,
        trailing_space,
        press_enter,
        false,
        cancelled,
    )
}

pub fn deliver_guarded_until(
    text: &str,
    target: Option<&Value>,
    paste: bool,
    trailing_space: bool,
    press_enter: bool,
    modifier_release_guard: bool,
    cancelled: impl Fn() -> bool,
) -> Result<String> {
    ensure!(!cancelled(), "Clipboard delivery cancelled");
    let mut transcript = text.to_owned();
    if trailing_space && !text.ends_with(whitespace) {
        transcript.push(' ');
    }
    let mut selection = SELECTION.lock().unwrap_or_else(|error| error.into_inner());
    ensure!(!cancelled(), "Clipboard delivery cancelled");
    let previous = replace_selection(&transcript, false, paste, &mut selection)?;
    if !paste {
        copy_text(&transcript, true)?;
        return Ok("Copied".to_owned());
    }
    // The compositor must release the shortcut modifiers before synthesized keys.
    wait_for_modifiers(modifier_release_guard, &cancelled)?;
    ensure!(!cancelled(), "Clipboard delivery cancelled");
    let id = target.and_then(window_id);
    if !id.is_some_and(|id| same_focus(id) || refocus(id)) {
        copy_text(&transcript, true)?;
        return Ok("Copied · focus changed".to_owned());
    }
    if !still_selected(&transcript, false) {
        copy_text(&transcript, true)?;
        return Ok("Copied · clipboard changed".to_owned());
    }
    if !id.is_some_and(same_focus) {
        copy_text(&transcript, true)?;
        return Ok("Copied · focus changed".to_owned());
    }
    ensure!(!cancelled(), "Clipboard delivery cancelled");
    let app_id = target
        .and_then(|window| window.get("app_id"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let terminal = is_terminal(app_id);
    if use_shift_insert(app_id) {
        let mut primary = PRIMARY.lock().unwrap_or_else(|error| error.into_inner());
        let previous_primary = replace_selection(&transcript, true, true, &mut primary)?;
        let pasted: Result<bool> = (|| {
            ensure!(!cancelled(), "Clipboard delivery cancelled");
            if !still_selected(&transcript, true)
                || !still_selected(&transcript, false)
                || !id.is_some_and(same_focus)
            {
                return Ok(false);
            }
            type_keys(&["-M", "shift", "-k", "Insert", "-m", "shift"], &cancelled)?;
            Ok(true)
        })();
        restore_after_paste(previous_primary, transcript.clone(), &mut primary, true);
        if !pasted? {
            return Ok("Copied · paste target changed".to_owned());
        }
    } else if terminal {
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
    restore_after_paste(previous, transcript, &mut selection, false);
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

#[cfg(test)]
mod modifier_tests {
    use super::*;

    #[test]
    fn modifier_bitmap_covers_both_sides_without_ordinary_keys() {
        let mut bitmap = [0u8; KEY_BITMAP_BYTES];
        bitmap[30 / 8] |= 1 << (30 % 8);
        assert!(!MODIFIER_KEYS.iter().any(|&key| key_pressed(&bitmap, key)));
        for &key in MODIFIER_KEYS {
            bitmap[key / 8] |= 1 << (key % 8);
            assert!(key_pressed(&bitmap, key));
            bitmap[key / 8] &= !(1 << (key % 8));
        }
    }

    #[test]
    fn modifier_wait_requires_release_and_honors_cancellation() {
        let timeout = Duration::from_millis(25);
        assert!(wait_release(|| Some(true), &|| false, Duration::ZERO, timeout, timeout).is_err());
        assert!(wait_release(|| Some(false), &|| true, Duration::ZERO, timeout, timeout).is_err());
        assert!(wait_release(|| Some(false), &|| false, Duration::ZERO, timeout, timeout).is_ok());
    }

    #[test]
    fn unreadable_keyboard_uses_fallback_wait() {
        let started = Instant::now();
        let fallback = Duration::from_millis(15);
        wait_release(
            || None,
            &|| false,
            Duration::ZERO,
            Duration::from_secs(1),
            fallback,
        )
        .unwrap();
        assert!(started.elapsed() >= fallback);
    }
}
