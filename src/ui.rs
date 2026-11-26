use crate::{audio, config::Config, setup};
use gtk::{gdk, gio, glib, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

pub const PAGES: [(&str, &str, &str); 5] = [
    ("general", "General", "preferences-system-symbolic"),
    ("model", "Speech model", "computer-symbolic"),
    (
        "vocabulary",
        "Vocabulary",
        "accessories-dictionary-symbolic",
    ),
    ("cleanup", "Text cleanup", "edit-clear-symbolic"),
    ("shortcuts", "Shortcuts", "input-keyboard-symbolic"),
];

pub struct ShortcutCapture {
    pub button: gtk::Button,
    title: String,
    value: RefCell<String>,
    recording: Cell<bool>,
    surface: RefCell<Option<gdk::Toplevel>>,
    changed: RefCell<Option<Rc<dyn Fn()>>>,
}

impl ShortcutCapture {
    fn new(title: &str, value: &str) -> Rc<Self> {
        let capture = Rc::new(Self {
            button: gtk::Button::builder().width_request(184).build(),
            title: title.into(),
            value: RefCell::new(value.into()),
            recording: Cell::new(false),
            surface: RefCell::new(None),
            changed: RefCell::new(None),
        });
        capture
            .button
            .set_tooltip_text(Some("Click, then press your shortcut. Escape cancels."));
        let weak = Rc::downgrade(&capture);
        capture.button.connect_clicked(move |_| {
            if let Some(capture) = weak.upgrade() {
                if capture.recording.get() {
                    capture.cancel();
                    return;
                }
                capture.button.grab_focus();
                capture.recording.set(true);
                capture.label("Press keys…");
                if let Some(surface) = capture
                    .button
                    .native()
                    .and_then(|native| native.surface())
                    .and_then(|surface| surface.downcast::<gdk::Toplevel>().ok())
                {
                    // Let an existing global shortcut reach the capture control instead of starting dictation.
                    surface.inhibit_system_shortcuts(None::<gdk::Event>);
                    *capture.surface.borrow_mut() = Some(surface);
                }
            }
        });
        let weak = Rc::downgrade(&capture);
        capture.button.connect_has_focus_notify(move |button| {
            if !button.has_focus()
                && let Some(capture) = weak.upgrade()
            {
                capture.cancel();
            }
        });
        capture
    }

    fn label(&self, label: &str) {
        self.button.set_label(label);
        self.button
            .update_property(&[gtk::accessible::Property::Label(&format!(
                "{} shortcut: {label}",
                self.title
            ))]);
    }

    fn cancel(&self) {
        self.recording.set(false);
        if let Some(surface) = self.surface.borrow_mut().take() {
            surface.restore_system_shortcuts();
        }
        self.label(&self.value.borrow().replace('+', " + "));
    }

    fn key_pressed(&self, key: gdk::Key, modifiers: gdk::ModifierType) -> bool {
        if !self.recording.get() {
            return false;
        }
        let mut parts = Vec::new();
        for (mask, name) in [
            (gdk::ModifierType::SUPER_MASK, "Super"),
            (gdk::ModifierType::CONTROL_MASK, "Ctrl"),
            (gdk::ModifierType::ALT_MASK, "Alt"),
            (gdk::ModifierType::SHIFT_MASK, "Shift"),
        ] {
            if modifiers.contains(mask) {
                parts.push(name.to_owned());
            }
        }
        if key == gdk::Key::Escape && parts.is_empty() {
            self.cancel();
            return true;
        }
        if matches!(
            key,
            gdk::Key::Shift_L
                | gdk::Key::Shift_R
                | gdk::Key::Control_L
                | gdk::Key::Control_R
                | gdk::Key::Alt_L
                | gdk::Key::Alt_R
                | gdk::Key::Super_L
                | gdk::Key::Super_R
                | gdk::Key::Meta_L
                | gdk::Key::Meta_R
                | gdk::Key::Hyper_L
                | gdk::Key::Hyper_R
                | gdk::Key::ISO_Level3_Shift
                | gdk::Key::ISO_Level5_Shift
        ) {
            return true;
        }
        if parts.is_empty() {
            self.label("Hold a modifier + key…");
            return true;
        }
        if let Some(key) = key.to_lower().name() {
            parts.push(key.to_string());
            if let Ok(value) = crate::shortcuts::normalize(&parts.join("+")) {
                let changed = *self.value.borrow() != value;
                *self.value.borrow_mut() = value;
                self.cancel();
                if changed && let Some(changed) = &*self.changed.borrow() {
                    changed();
                }
            }
        }
        true
    }
}

impl Drop for ShortcutCapture {
    fn drop(&mut self) {
        if let Some(surface) = self.surface.get_mut().take() {
            surface.restore_system_shortcuts();
        }
    }
}

#[derive(Clone)]
pub enum Control {
    Switch(gtk::Switch),
    Entry(gtk::Entry),
    Text(gtk::TextBuffer),
    Number(gtk::SpinButton),
    Choice(gtk::DropDown, RefCell<Vec<String>>),
    Shortcut(Rc<ShortcutCapture>),
}

impl Control {
    fn value(&self) -> serde_json::Value {
        use serde_json::json;
        match self {
            Self::Switch(widget) => json!(widget.is_active()),
            Self::Entry(widget) => json!(widget.text().as_str()),
            Self::Shortcut(capture) => json!(*capture.value.borrow()),
            Self::Text(buffer) => json!(
                buffer
                    .text(&buffer.start_iter(), &buffer.end_iter(), true)
                    .as_str()
            ),
            Self::Number(widget) => json!(widget.value()),
            Self::Choice(widget, values) => json!(
                values
                    .borrow()
                    .get(widget.selected() as usize)
                    .cloned()
                    .unwrap_or_default()
            ),
        }
    }

    fn connect_changed(&self, changed: Rc<dyn Fn()>) {
        match self {
            Self::Switch(widget) => {
                widget.connect_active_notify(move |_| changed());
            }
            Self::Entry(widget) => {
                widget.connect_changed(move |_| changed());
            }
            Self::Shortcut(capture) => {
                *capture.changed.borrow_mut() = Some(changed);
            }
            Self::Text(buffer) => {
                buffer.connect_changed(move |_| changed());
            }
            Self::Number(widget) => {
                widget.connect_value_changed(move |_| changed());
            }
            Self::Choice(widget, _) => {
                widget.connect_selected_notify(move |_| changed());
            }
        }
    }
}

pub struct Home {
    pub window: gtk::ApplicationWindow,
    pub stack: gtk::Stack,
    pub navigation: gtk::ListBox,
    pub controls: Vec<(String, Control)>,
    pub save_button: gtk::Button,
    pub status: gtk::Label,
    pub badge: gtk::Label,
    pub download_button: gtk::Button,
    values: RefCell<Config>,
    model_status: gtk::Label,
    model_ready: Cell<bool>,
    downloading: Cell<bool>,
    status_dot: gtk::Box,
    sidebar: gtk::Box,
    navigation_labels: Vec<gtk::Label>,
    rows: Vec<(gtk::Box, gtk::Widget)>,
    compact: Cell<bool>,
    title: gtk::Label,
}

fn label(text: &str, class: &str) -> gtk::Label {
    let widget = gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .max_width_chars(60)
        .build();
    if !class.is_empty() {
        widget.add_css_class(class);
    }
    widget
}

fn input_choices(sources: Vec<(String, String)>, keep: &[&str]) -> Vec<(String, String)> {
    let mut choices = vec![("auto".to_string(), "System default".to_string())];
    choices.extend(sources);
    for id in keep {
        if !choices.iter().any(|(key, _)| key == id) {
            choices.push((id.to_string(), format!("{id} (unavailable)")));
        }
    }
    choices
}

fn section(page: &gtk::Box, title: &str) -> gtk::Box {
    let heading = label(title, "section-title");
    page.append(&heading);
    let group = gtk::Box::new(gtk::Orientation::Vertical, 0);
    group.add_css_class("settings-panel");
    page.append(&group);
    group
}

fn row(group: &gtk::Box, title: &str, detail: &str, control: &impl IsA<gtk::Widget>) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 14);
    row.add_css_class("setting-row");
    let text = gtk::Box::new(gtk::Orientation::Vertical, 3);
    text.set_hexpand(true);
    text.append(&label(title, "row-title"));
    if !detail.is_empty() {
        text.append(&label(detail, "muted"));
    }
    row.append(&text);
    control.set_valign(gtk::Align::Center);
    control.set_halign(gtk::Align::End);
    control
        .as_ref()
        .update_property(&[gtk::accessible::Property::Label(title)]);
    row.append(control);
    group.append(&row);
    row
}

impl Home {
    pub fn new(
        app: &gtk::Application,
        values: &Config,
        action: Rc<dyn Fn(&str)>,
        save: Rc<dyn Fn(Config) -> bool>,
    ) -> Rc<Self> {
        let css = gtk::CssProvider::new();
        css.load_from_string(include_str!("../assets/settings.css"));
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &css,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("Hype Flow")
            .default_width(960)
            .default_height(680)
            .hide_on_close(true)
            .decorated(false)
            .build();
        window.set_size_request(620, 480);
        window.set_widget_name("hype-settings");
        let shell = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 14);
        sidebar.set_widget_name("settings-sidebar");
        sidebar.set_size_request(200, -1);
        shell.append(&sidebar);
        let lights = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        lights.add_css_class("traffic-lights");
        for (name, glyph, description) in [
            ("close", "×", "Close window (Ctrl+W)"),
            ("zoom", "+", "Zoom"),
        ] {
            let button = gtk::Button::with_label(glyph);
            button.add_css_class("traffic");
            button.add_css_class(name);
            button.set_tooltip_text(Some(description));
            button.set_focusable(false);
            button.update_property(&[gtk::accessible::Property::Label(description)]);
            let window = window.downgrade();
            button.connect_clicked(move |_| {
                if let Some(window) = window.upgrade() {
                    if name == "close" {
                        window.set_visible(false);
                    } else if window.is_maximized() {
                        window.unmaximize();
                    } else {
                        window.maximize();
                    }
                }
            });
            lights.append(&button);
        }
        let handle = gtk::WindowHandle::new();
        handle.set_child(Some(&lights));
        sidebar.append(&handle);
        let brand = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        brand.add_css_class("brand");
        let bytes = glib::Bytes::from_static(include_bytes!("../assets/logo.svg"));
        let icon = gdk::Texture::from_bytes(&bytes).ok();
        let image = gtk::Image::from_paintable(icon.as_ref());
        image.set_pixel_size(30);
        brand.append(&image);
        let brand_name = label("Hype Flow", "brand-name");
        brand.append(&brand_name);
        sidebar.append(&brand);
        let navigation = gtk::ListBox::new();
        navigation.set_widget_name("settings-navigation");
        navigation.set_selection_mode(gtk::SelectionMode::Single);
        sidebar.append(&navigation);
        let mut navigation_labels = vec![brand_name];
        for (index, (_, title, icon)) in PAGES.iter().enumerate() {
            let content = gtk::Box::new(gtk::Orientation::Horizontal, 9);
            let image = gtk::Image::from_icon_name(icon);
            image.add_css_class("tile");
            image.add_css_class(["gray", "blue", "orange", "green", "indigo"][index]);
            content.append(&image);
            let text = label(title, "");
            navigation_labels.push(text.clone());
            content.append(&text);
            let item = gtk::ListBoxRow::new();
            item.set_tooltip_text(Some(title));
            item.set_child(Some(&content));
            navigation.append(&item);
        }
        let content = gtk::Box::new(gtk::Orientation::Vertical, 14);
        content.set_widget_name("settings-content");
        content.set_hexpand(true);
        shell.append(&content);
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        header.add_css_class("page-header");
        let title = label("General", "page-title");
        title.set_hexpand(true);
        header.append(&title);
        let status_dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        status_dot.add_css_class("status-dot");
        status_dot.add_css_class("busy");
        status_dot.set_valign(gtk::Align::Center);
        header.append(&status_dot);
        let badge = label("On-device", "status-badge");
        header.append(&badge);
        content.append(&header);
        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .transition_duration(220)
            .vexpand(true)
            .build();
        content.append(&stack);
        let pages: Vec<gtk::Box> = PAGES
            .iter()
            .map(|(name, _, _)| {
                let page = gtk::Box::new(gtk::Orientation::Vertical, 10);
                page.add_css_class("settings-page");
                let scroll = gtk::ScrolledWindow::builder()
                    .hscrollbar_policy(gtk::PolicyType::Never)
                    .child(&page)
                    .build();
                stack.add_named(&scroll, Some(name));
                page
            })
            .collect();
        let mut controls = Vec::new();
        let mut rows = Vec::new();
        let dictation = section(&pages[0], "Dictation");
        let microphones = section(&pages[0], "Microphone");
        let feedback = section(&pages[0], "Feedback");
        let transcription = section(&pages[1], "Transcription");
        let model_summary = gtk::Box::new(gtk::Orientation::Vertical, 4);
        model_summary.add_css_class("panel-body");
        let model_status = label("Checking speech model…", "model-title");
        model_summary.append(&model_status);
        model_summary.append(&label(
            "English dictation runs entirely on this device.",
            "muted",
        ));
        transcription.append(&model_summary);
        let download_button = gtk::Button::with_label("Install selected model");
        download_button.set_halign(gtk::Align::End);
        {
            let action = action.clone();
            download_button.connect_clicked(move |_| action("download"));
        }
        let processing = section(&pages[1], "Audio processing");
        let memory = section(&pages[1], "Memory");
        let vocabulary = section(&pages[2], "Custom vocabulary");
        let near = section(&pages[2], "Near-miss correction");
        let corrections = section(&pages[3], "Auto-correction");
        let filters = section(&pages[3], "Word filters");
        let repetitions = section(&pages[3], "Repetitions");
        let json = serde_json::to_value(values).expect("valid settings serialize");
        for (key, title, detail, group) in [
            (
                "paste",
                "Paste after dictation",
                "Insert text into the app where you started speaking.",
                &dictation,
            ),
            (
                "modifier_release_guard",
                "Wait for shortcut keys to release",
                "Experimental. Checks accessible keyboards before pasting; falls back to the standard delay.",
                &dictation,
            ),
            (
                "trailing_space",
                "Add a space after pasted text",
                "Keeps consecutive dictations from running together.",
                &dictation,
            ),
            (
                "press_enter",
                "Press Enter after pasting",
                "Sends chat messages. Never sends Enter to terminals.",
                &dictation,
            ),
            (
                "keep_mic_ready",
                "Keep microphone ready",
                "Keeps the microphone open for 30 seconds after dictation, including half a second before your next shortcut.",
                &microphones,
            ),
            (
                "sounds",
                "Play start and stop sounds",
                "The start sound plays once the microphone is live.",
                &feedback,
            ),
            (
                "pause_media",
                "Pause music while dictating",
                "Pauses only players that are playing, then resumes them.",
                &feedback,
            ),
            (
                "cut_silence",
                "Cut silence before recognition",
                "Removes long pauses with on-device voice detection; keeps padding around speech.",
                &processing,
            ),
            (
                "noise_suppression",
                "Suppress background noise",
                "Cleans room noise before recognition. Adds processing time.",
                &processing,
            ),
            (
                "vocabulary_boosting",
                "Boost vocabulary during recognition",
                "Available with Unified. Experimental and can insert unwanted words.",
                &vocabulary,
            ),
            (
                "fuzzy_vocabulary",
                "Fix near-miss spellings",
                "Matches similar spellings and sounds, including split names such as Charge B.",
                &near,
            ),
            (
                "auto_correct",
                "Apply saved corrections",
                "Apply literal rules and preferred vocabulary capitalization.",
                &corrections,
            ),
            (
                "filter_words",
                "Remove listed words and phrases",
                "Preserve URLs, email addresses, quoted text and inline code.",
                &filters,
            ),
            (
                "recover_recordings",
                "Recover failed recordings",
                "Opt-in: stores failed and interrupted audio privately; keeps the latest 20. Retry copies text for review.",
                &memory,
            ),
            (
                "history_enabled",
                "Keep recent recording history",
                "Opt-in: stores the latest 20 successful recordings privately for re-transcription.",
                &memory,
            ),
            (
                "spoken_commands",
                "Spoken punctuation and commands",
                "Recognize punctuation names and whole-utterance New line, New paragraph and Scratch that.",
                &repetitions,
            ),
            (
                "remove_restarts",
                "Remove spoken restarts",
                "Experimental: removes bounded restarts and editing phrases such as I mean. Original remains recoverable.",
                &repetitions,
            ),
            (
                "format_numbers",
                "Format numbers, money and times",
                "Conservative English formatting; keeps ordinary phrases such as one of the things unchanged.",
                &repetitions,
            ),
            (
                "capitalize_after_filler",
                "Capitalize after removing a filler",
                "Capitalize sentence starts while preserving mixed-case identifiers.",
                &filters,
            ),
            (
                "confidence_filtering",
                "Protect confidently recognized vocabulary",
                "Only permit guessed vocabulary corrections on low-confidence words. Missing confidence prevents guessing.",
                &near,
            ),
            (
                "hallucination_filtering",
                "Filter anomalous recognition",
                "Experimental: reject improbable token timing and confidence patterns; needs calibration on your speech.",
                &processing,
            ),
            (
                "soft_speech_retry",
                "Retry audible empty recognition",
                "Experimental: retry once when VAD finds nothing but an energy gate detects speech-like input.",
                &processing,
            ),
            (
                "remove_repetitions",
                "Remove adjacent repeated words",
                "Optional: the the becomes the. Leave off to preserve emphasis.",
                &repetitions,
            ),
        ] {
            let switch = gtk::Switch::builder()
                .active(json[key].as_bool().unwrap_or(false))
                .build();
            row(group, title, detail, &switch);
            controls.push((key.to_string(), Control::Switch(switch)));
        }
        let sources = input_choices(Vec::new(), &[&values.input_target]);
        for (key, title, detail, choices, group) in [
            (
                "model_variant",
                "Model to install",
                "The active model stays in use until installation finishes.",
                vec![
                    ("unified".into(), "Unified English INT8 · 663 MB".into()),
                    (
                        "low-memory".into(),
                        "Parakeet 110M TDT INT8 · 136.5 MB".into(),
                    ),
                    (
                        "parakeet-ctc".into(),
                        "Parakeet 110M CTC INT8 · 131.7 MB".into(),
                    ),
                ],
                &transcription,
            ),
            (
                "dictation_mode",
                "Shortcut behavior",
                "Hold requires niri press/release bindings. Unsupported compositors keep the existing bindings.",
                vec![
                    ("toggle".into(), "Press to start and stop".into()),
                    ("hold".into(), "Hold to dictate".into()),
                ],
                &dictation,
            ),
            (
                "input_target",
                "Input device",
                "Used the next time you start dictating.",
                sources,
                &microphones,
            ),
            (
                "unload_minutes",
                "Unload model when idle",
                "Releases model memory and reloads while you speak.",
                crate::config::UNLOAD_CHOICES
                    .into_iter()
                    .map(|n| {
                        (
                            n.to_string(),
                            if n == crate::config::UNLOAD_AFTER_EACH {
                                "After each dictation".into()
                            } else if n == 0 {
                                "Never".into()
                            } else {
                                format!("After {n} minutes")
                            },
                        )
                    })
                    .collect(),
                &memory,
            ),
        ] {
            let labels: Vec<&str> = choices
                .iter()
                .map(|(_, description)| description.as_str())
                .collect();
            let dropdown = gtk::DropDown::from_strings(&labels);
            let current = match key {
                "unload_minutes" => values.unload_minutes.to_string(),
                "model_variant" => values.model_variant.clone(),
                "dictation_mode" => values.dictation_mode.clone(),
                _ => values.input_target.clone(),
            };
            dropdown.set_selected(
                choices
                    .iter()
                    .position(|(id, _)| id == &current)
                    .unwrap_or(0) as u32,
            );
            let item = row(group, title, detail, &dropdown);
            rows.push((item, dropdown.clone().upcast()));
            controls.push((
                key.to_string(),
                Control::Choice(
                    dropdown,
                    RefCell::new(choices.into_iter().map(|(id, _)| id).collect()),
                ),
            ));
        }
        let model_actions = gtk::Box::new(gtk::Orientation::Vertical, 0);
        model_actions.add_css_class("panel-body");
        model_actions.append(&download_button);
        transcription.append(&model_actions);
        let prompt = gtk::Entry::builder()
            .text(&values.prompt)
            .placeholder_text("niri, Hype Flow, project names…")
            .hexpand(true)
            .build();
        let vocabulary_body = gtk::Box::new(gtk::Orientation::Vertical, 6);
        vocabulary_body.add_css_class("panel-body");
        let prompt_title = "Names and specialist terms";
        vocabulary_body.append(&label(prompt_title, "row-title"));
        vocabulary_body.append(&label("Separate up to 100 entries with commas.", "muted"));
        prompt.update_property(&[gtk::accessible::Property::Label(prompt_title)]);
        vocabulary_body.append(&prompt);
        vocabulary.prepend(&vocabulary_body);
        controls.push(("prompt".into(), Control::Entry(prompt)));
        for (key, title, detail, minimum, maximum, step, group) in [
            (
                "boost_score",
                "Vocabulary strength",
                "Start low. Strong bias can insert words you did not say.",
                0.5,
                3.0,
                0.5,
                &vocabulary,
            ),
            (
                "fuzzy_tolerance",
                "Tolerance",
                "Higher catches more mishearings but risks replacing real words.",
                0.05,
                0.4,
                0.01,
                &near,
            ),
        ] {
            let spin = gtk::SpinButton::with_range(minimum, maximum, step);
            spin.set_digits(if key == "boost_score" { 1 } else { 2 });
            spin.set_value(json[key].as_f64().unwrap_or(minimum));
            row(group, title, detail, &spin);
            controls.push((key.to_string(), Control::Number(spin)));
        }
        for (key, title, detail, group) in [
            (
                "corrections",
                "Correction rules",
                "One rule per line: heard words => replacement. Example: open ai => OpenAI",
                &corrections,
            ),
            (
                "blocked_words",
                "Words and phrases to remove",
                "Separate entries with commas or new lines. Only list words you want removed.",
                &filters,
            ),
        ] {
            let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
            body.add_css_class("panel-body");
            body.append(&label(detail, "muted"));
            let editor = gtk::TextView::builder()
                .wrap_mode(gtk::WrapMode::WordChar)
                .top_margin(8)
                .bottom_margin(8)
                .left_margin(10)
                .right_margin(10)
                .build();
            editor.update_property(&[gtk::accessible::Property::Label(title)]);
            editor
                .buffer()
                .set_text(json[key].as_str().unwrap_or_default());
            let scroll = gtk::ScrolledWindow::builder()
                .min_content_height(100)
                .hscrollbar_policy(gtk::PolicyType::Never)
                .child(&editor)
                .build();
            scroll.add_css_class("text-editor");
            body.append(&scroll);
            group.append(&body);
            controls.push((key.to_string(), Control::Text(editor.buffer())));
        }
        let transcript = section(&pages[0], "Last transcript");
        for (title, detail, button_label, name) in [
            (
                "Cleaned transcript",
                "Recover the last dictation for this session.",
                "Copy transcript",
                "copy-last",
            ),
            (
                "Original transcript",
                "Recognizer output before corrections and filtering.",
                "Copy original",
                "copy-raw",
            ),
            (
                "Failed recording",
                "Re-transcribe the latest failed or interrupted recording; copied for review.",
                "Retry last failed",
                "retry-last",
            ),
            (
                "Recent recording",
                "Re-transcribe the latest retained successful recording; copied for review.",
                "Retry recent recording",
                "retry-history",
            ),
            (
                "Saved recordings",
                "Delete retained recovery and history audio. Disabling retention does not delete existing files.",
                "Delete saved recordings",
                "clear-recordings",
            ),
        ] {
            let button = gtk::Button::with_label(button_label);
            let action = action.clone();
            button.connect_clicked(move |_| action(name));
            row(&transcript, title, detail, &button);
            button.update_property(&[gtk::accessible::Property::Label(button_label)]);
        }
        pages[0].append(&label(
            "Use your Dictate shortcut to start. Press again to finish. Edit bindings in Shortcuts.",
            "muted",
        ));
        pages[3].append(&label("Copy original in General recovers text before cleanup. Your transcript hook receives cleaned text and has 30 seconds; on failure, cleaned text is kept.", "muted"));
        let shortcuts = section(&pages[4], "Keyboard shortcuts");
        for (key, title, detail, value) in [
            (
                "dictation_shortcut",
                "Dictate",
                "Start dictation; press again to finish.",
                &values.dictation_shortcut,
            ),
            (
                "cancel_shortcut",
                "Cancel",
                "Discard the current recording or transcription.",
                &values.cancel_shortcut,
            ),
        ] {
            let capture = ShortcutCapture::new(title, value);
            row(&shortcuts, title, detail, &capture.button);
            // Label after row(), which sets the accessible name to the bare title.
            capture.cancel();
            controls.push((key.into(), Control::Shortcut(capture)));
        }
        pages[4].append(&label("Click a shortcut, then press the keys you want. Escape cancels. Use Save changes to apply; existing bindings are checked first.", "muted"));
        let integration = section(&pages[4], "niri integration");
        let configure = gtk::Button::with_label("Configure niri");
        {
            let action = action.clone();
            configure.connect_clicked(move |_| action("shortcuts"));
        }
        row(
            &integration,
            "Desktop shortcuts and autostart",
            "Validates settings and saves a backup before applying.",
            &configure,
        );
        pages[4].append(&label("The dictation capsule shows microphone levels. Click it to finish, or right-click to cancel.", "muted"));
        let footer = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        footer.add_css_class("settings-footer");
        let status = label("Ready", "muted");
        status.set_hexpand(true);
        footer.append(&status);
        let save_button = gtk::Button::with_label("Save changes");
        save_button.add_css_class("primary");
        save_button.set_sensitive(false);
        footer.append(&save_button);
        content.append(&footer);
        window.set_child(Some(&shell));
        let home = Rc::new(Self {
            window,
            stack,
            navigation,
            controls,
            save_button,
            status,
            badge,
            download_button,
            values: RefCell::new(values.clone()),
            model_status,
            model_ready: Cell::new(false),
            downloading: Cell::new(false),
            status_dot,
            sidebar,
            navigation_labels,
            rows,
            compact: Cell::new(false),
            title,
        });
        let weak = Rc::downgrade(&home);
        home.navigation.connect_row_selected(move |_, row| {
            if let (Some(home), Some(row)) = (weak.upgrade(), row)
                && let Some((name, title, _)) = PAGES.get(row.index() as usize)
            {
                home.cancel_shortcut_capture();
                home.stack.set_visible_child_name(name);
                home.title.set_label(title);
            }
        });
        let weak = Rc::downgrade(&home);
        let dirty: Rc<dyn Fn()> = Rc::new(move || {
            if let Some(home) = weak.upgrade() {
                home.save_button.set_sensitive(true);
                home.save_button.set_label("Save changes");
            }
        });
        for (key, control) in &home.controls {
            let dirty = dirty.clone();
            if key == "model_variant" {
                let weak = Rc::downgrade(&home);
                control.connect_changed(Rc::new(move || {
                    dirty();
                    if let Some(home) = weak.upgrade() {
                        home.update_download_button();
                    }
                }));
            } else {
                control.connect_changed(dirty);
            }
        }
        let weak = Rc::downgrade(&home);
        home.save_button.connect_clicked(move |_| {
            if let Some(home) = weak.upgrade() {
                match home.changes() {
                    Ok(values) => {
                        if save(values.clone()) {
                            home.saved(values);
                        }
                    }
                    Err(error) => home.feedback(&format!("{error:#}")),
                }
            }
        });
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&home);
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            if let Some(home) = weak.upgrade() {
                for (_, control) in &home.controls {
                    if let Control::Shortcut(capture) = control
                        && capture.key_pressed(key, modifiers)
                    {
                        return glib::Propagation::Stop;
                    }
                }
            }
            if modifiers.intersection(gtk::accelerator_get_default_mod_mask())
                == gdk::ModifierType::CONTROL_MASK
            {
                if key == gdk::Key::w || key == gdk::Key::W {
                    if let Some(home) = weak.upgrade() {
                        home.window.set_visible(false);
                    }
                    return glib::Propagation::Stop;
                }
                if key == gdk::Key::q || key == gdk::Key::Q {
                    action("quit");
                    return glib::Propagation::Stop;
                }
            }
            glib::Propagation::Proceed
        });
        home.window.add_controller(keys);
        let weak = Rc::downgrade(&home);
        home.window.connect_is_active_notify(move |window| {
            if !window.is_active()
                && let Some(home) = weak.upgrade()
            {
                home.cancel_shortcut_capture();
            }
        });
        let weak = Rc::downgrade(&home);
        home.window.connect_hide(move |_| {
            if let Some(home) = weak.upgrade() {
                home.cancel_shortcut_capture();
            }
        });
        let weak = Rc::downgrade(&home);
        home.window.connect_show(move |_| {
            let weak = weak.clone();
            // pw-dump can take seconds, so list microphones off the GTK thread on every open.
            glib::spawn_future_local(async move {
                if let Ok(sources) = gio::spawn_blocking(audio::sources).await
                    && let Some(home) = weak.upgrade()
                {
                    home.set_sources(sources);
                }
            });
        });
        let weak = Rc::downgrade(&home);
        home.window.connect_realize(move |window| {
            let weak = weak.clone();
            // Fires on the first size and on interactive, tiled and maximized resizes.
            if let Some(surface) = window.surface() {
                surface.connect_width_notify(move |surface| {
                    if let Some(home) = weak.upgrade() {
                        home.resize_layout(surface.width());
                    }
                });
            }
        });
        home.navigation
            .select_row(home.navigation.row_at_index(0).as_ref());
        home.update_boosting_controls();
        home
    }

    fn cancel_shortcut_capture(&self) {
        for (_, control) in &self.controls {
            if let Control::Shortcut(capture) = control {
                capture.cancel();
            }
        }
    }

    fn set_sources(&self, sources: Vec<(String, String)>) {
        let Some((dropdown, ids)) = self
            .controls
            .iter()
            .find_map(|(key, control)| match control {
                Control::Choice(dropdown, ids) if key == "input_target" => Some((dropdown, ids)),
                _ => None,
            })
        else {
            return;
        };
        let selected = ids
            .borrow()
            .get(dropdown.selected() as usize)
            .cloned()
            .unwrap_or_else(|| "auto".into());
        let choices = input_choices(sources, &[&self.values.borrow().input_target, &selected]);
        let labels: Vec<&str> = choices.iter().map(|(_, label)| label.as_str()).collect();
        let position = choices.iter().position(|(id, _)| *id == selected);
        // A refreshed device list is not an edit, so keep the save button as it was.
        let (sensitive, label) = (self.save_button.is_sensitive(), self.save_button.label());
        *ids.borrow_mut() = choices.iter().map(|(id, _)| id.clone()).collect();
        dropdown.set_model(Some(&gtk::StringList::new(&labels)));
        dropdown.set_selected(position.unwrap_or(0) as u32);
        self.save_button.set_sensitive(sensitive);
        self.save_button
            .set_label(label.as_deref().unwrap_or_default());
    }

    pub fn changes(&self) -> anyhow::Result<Config> {
        let mut values = serde_json::to_value(&*self.values.borrow())?;
        for (key, control) in &self.controls {
            let mut value = control.value();
            if key == "unload_minutes" {
                value = serde_json::json!(value.as_str().unwrap_or("10").parse::<u32>()?);
            }
            values[key] = value;
        }
        let mut config: Config = serde_json::from_value(values)?;
        config.dictation_shortcut = crate::shortcuts::normalize(&config.dictation_shortcut)?;
        config.cancel_shortcut = crate::shortcuts::normalize(&config.cancel_shortcut)?;
        config.validate()?;
        Ok(config)
    }

    pub fn resize_layout(&self, width: i32) {
        let compact = width < 760;
        if self.compact.replace(compact) == compact {
            return;
        }
        self.sidebar
            .set_size_request(if compact { 64 } else { 200 }, -1);
        self.download_button.set_halign(if compact {
            gtk::Align::Start
        } else {
            gtk::Align::End
        });
        for label in &self.navigation_labels {
            label.set_visible(!compact);
        }
        for (row, control) in &self.rows {
            row.set_orientation(if compact {
                gtk::Orientation::Vertical
            } else {
                gtk::Orientation::Horizontal
            });
            control.set_halign(if compact {
                gtk::Align::Start
            } else {
                gtk::Align::End
            });
        }
    }

    pub fn saved(self: &Rc<Self>, values: Config) {
        *self.values.borrow_mut() = values.clone();
        self.update_boosting_controls();
        self.update_download_button();
        if !self.changes().is_ok_and(|draft| draft == values) {
            self.save_button.set_sensitive(true);
            self.save_button.set_label("Save changes");
            return;
        }
        self.save_button.set_sensitive(false);
        self.save_button.set_label("Saved");
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(std::time::Duration::from_millis(1600), move || {
            if let Some(home) = weak.upgrade()
                && !home.save_button.is_sensitive()
            {
                home.save_button.set_label("Save changes");
            }
        });
    }

    pub fn feedback(&self, message: &str) {
        self.status.set_label(message);
        self.status.set_tooltip_text(Some(message));
    }

    pub fn set_downloading(&self, downloading: bool) {
        self.downloading.set(downloading);
        self.update_download_button();
    }

    fn update_boosting_controls(&self) {
        let supported =
            setup::variant_for_path(std::path::Path::new(&self.values.borrow().model)) == "unified";
        for (key, control) in &self.controls {
            match (key.as_str(), control) {
                ("vocabulary_boosting", Control::Switch(switch)) => {
                    if !supported {
                        switch.set_active(false);
                    }
                    switch.set_sensitive(supported);
                }
                ("boost_score", Control::Number(spin)) => spin.set_sensitive(supported),
                _ => {}
            }
        }
    }

    fn update_download_button(&self) {
        let values = self.values.borrow();
        let saved_choice = self.controls.iter().any(|(key, control)| {
            key == "model_variant" && control.value().as_str() == Some(&values.model_variant)
        });
        let reinstall = self.model_ready.get()
            && setup::model_spec(&values.model_variant).is_ok_and(|spec| {
                crate::config::data_dir().join("models").join(spec.name)
                    == std::path::Path::new(&values.model)
            });
        let downloading = self.downloading.get();
        self.download_button
            .set_sensitive(saved_choice && !downloading);
        if saved_choice && !reinstall {
            self.download_button.add_css_class("primary");
        } else {
            self.download_button.remove_css_class("primary");
        }
        self.download_button.set_label(if downloading {
            "Installing…"
        } else if !saved_choice {
            "Save model choice to install"
        } else if reinstall {
            "Reinstall model"
        } else {
            "Install model"
        });
    }

    pub fn update(&self, state: &str, message: &str, ready: bool) {
        self.feedback(message);
        self.model_ready.set(ready);
        self.update_download_button();
        let model = std::path::Path::new(&self.values.borrow().model).to_path_buf();
        let installed = setup::model_files(&model)
            .iter()
            .all(|name| model.join(name).is_file());
        let model_status = if ready {
            let spec =
                setup::model_spec(setup::variant_for_path(&model)).expect("known model variant");
            format!("Active model · {}", spec.label)
        } else if installed {
            "Speech support needs an update".to_owned()
        } else {
            "No speech model installed".to_owned()
        };
        self.model_status.set_label(&model_status);
        self.badge.set_label(match state {
            "loading" => "Loading",
            "starting" => "Starting",
            "recording" => "Listening",
            "transcribing" => "Transcribing",
            "delivering" => "Copying",
            "error" => "Needs attention",
            _ if !ready => "Setup required",
            _ => "On-device",
        });
        for tone in ["ok", "busy", "error"] {
            self.status_dot.remove_css_class(tone);
        }
        self.status_dot.add_css_class(if state == "error" {
            "error"
        } else if ready && matches!(state, "idle" | "done") {
            "ok"
        } else {
            "busy"
        });
        if state == "error" {
            self.status.add_css_class("error");
        } else {
            self.status.remove_css_class("error");
        }
    }
}

struct PillState {
    phase: String,
    message: String,
    volume: f64,
    duration: f64,
    levels: [f64; 11],
    last_sample: i64,
    began: i64,
    width: f64,
    target: f64,
    input: [i32; 4],
}

impl PillState {
    fn animating(&self, animated: bool) -> bool {
        self.phase == "recording"
            || self.width != self.target
            || (animated && matches!(self.phase.as_str(), "transcribing" | "delivering"))
    }
}

fn animations() -> bool {
    gtk::Settings::default().is_none_or(|settings| settings.is_gtk_enable_animations())
}

pub struct Overlay {
    pub window: gtk::Window,
    area: gtk::DrawingArea,
    state: Rc<RefCell<PillState>>,
    ticking: Rc<Cell<bool>>,
}

impl Overlay {
    pub fn new(action: Rc<dyn Fn(&str)>) -> anyhow::Result<Self> {
        anyhow::ensure!(
            gtk4_layer_shell::is_supported(),
            "Wayland layer-shell is unavailable. Run inside niri."
        );
        let window = gtk::Window::builder()
            .title("Hype Flow")
            .decorated(false)
            .resizable(false)
            .build();
        window.set_widget_name("hype-flow");
        window.init_layer_shell();
        window.set_namespace(Some("hype-flow"));
        window.set_layer(Layer::Overlay);
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_exclusive_zone(0);
        window.set_anchor(Edge::Bottom, true);
        window.set_margin(Edge::Bottom, 20);
        let css = gtk::CssProvider::new();
        css.load_from_string("#hype-flow {background:transparent; box-shadow:none;}");
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &css,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
        let area = gtk::DrawingArea::builder()
            .content_width(340)
            .content_height(72)
            .build();
        area.update_property(&[gtk::accessible::Property::Label("Hype Flow dictation")]);
        window.set_child(Some(&area));
        let state = Rc::new(RefCell::new(PillState {
            phase: "idle".into(),
            message: "Ready".into(),
            volume: 0.0,
            duration: 0.0,
            levels: [0.0; 11],
            last_sample: 0,
            began: 0,
            width: 270.0,
            target: 270.0,
            input: [0; 4],
        }));
        let draw_state = state.clone();
        area.set_draw_func(move |area, cr, width, height| {
            let mut state = draw_state.borrow_mut();
            let now = glib::monotonic_time();
            if now - state.last_sample > 80_000 && state.phase == "recording" {
                state.levels.rotate_left(1);
                state.levels[10] = state.volume;
                state.last_sample = now;
            }
            let animated = animations();
            // Snap the last half pixel so the tick callback can stop.
            if animated && (state.target - state.width).abs() > 0.5 {
                state.width += (state.target - state.width) * 0.2;
            } else {
                state.width = state.target;
            }
            let w = state.width;
            let h = 48.0;
            let x = (width as f64 - w) / 2.0;
            let y = (height as f64 - h) / 2.0;
            let input = [x as i32, y as i32, w.ceil() as i32, h as i32];
            if state.input != input
                && let Some(surface) = area.native().and_then(|native| native.surface())
            {
                let region = gtk::cairo::Region::create_rectangle(&gtk::cairo::RectangleInt::new(
                    input[0], input[1], input[2], input[3],
                ));
                surface.set_input_region(Some(&region));
                state.input = input;
            }
            if matches!(state.phase.as_str(), "transcribing" | "delivering") {
                let angle = if animated {
                    (now as f64 / 1_600_000.0).fract() * std::f64::consts::TAU
                } else {
                    0.0
                };
                cr.set_source_rgba(0.22, 0.60, 0.97, 0.2);
                capsule(cr, x - 4.0, y - 4.0, w + 8.0, h + 8.0);
                let _ = cr.fill();
                let gradient = gtk::cairo::LinearGradient::new(
                    x + w / 2.0 + angle.cos() * w / 2.0,
                    y + angle.sin() * h,
                    x + w / 2.0 - angle.cos() * w / 2.0,
                    y - angle.sin() * h,
                );
                gradient.add_color_stop_rgb(0.0, 0.92, 0.97, 1.0);
                gradient.add_color_stop_rgb(0.3, 0.22, 0.74, 0.97);
                gradient.add_color_stop_rgb(1.0, 0.15, 0.38, 0.92);
                let _ = cr.set_source(&gradient);
                capsule(cr, x - 2.0, y - 2.0, w + 4.0, h + 4.0);
                let _ = cr.fill();
            }
            capsule(cr, x, y, w, h);
            cr.set_source_rgb(0.125, 0.133, 0.145);
            let _ = cr.fill_preserve();
            cr.set_source_rgb(if state.phase == "error" { 0.54 } else { 0.23 }, 0.23, 0.25);
            cr.set_line_width(1.0);
            let _ = cr.stroke();
            cr.set_source_rgb(0.91, 0.91, 0.91);
            cr.set_line_width(2.0);
            cr.set_line_cap(gtk::cairo::LineCap::Round);
            let cx = x + 24.0;
            let cy = height as f64 / 2.0;
            capsule(cr, cx - 3.0, cy - 9.0, 6.0, 13.0);
            let _ = cr.stroke();
            cr.arc(cx, cy - 1.0, 7.0, 0.0, std::f64::consts::PI);
            let _ = cr.stroke();
            cr.move_to(cx, cy + 6.0);
            cr.line_to(cx, cy + 10.0);
            cr.move_to(cx - 4.0, cy + 10.0);
            cr.line_to(cx + 4.0, cy + 10.0);
            let _ = cr.stroke();
            if state.phase == "recording" && now - state.began > 660_000 {
                for (index, level) in state.levels.iter().enumerate() {
                    let length = 4.0 + level.powf(0.75) * 18.0;
                    let x = cx + 20.0 + index as f64 * 5.0;
                    cr.move_to(x, cy - length / 2.0);
                    cr.line_to(x, cy + length / 2.0);
                }
                let _ = cr.stroke();
            } else if !matches!(
                state.phase.as_str(),
                "starting" | "recording" | "transcribing" | "delivering"
            ) {
                let title = if state.phase == "done" {
                    state.message.as_str()
                } else if state.phase == "error" {
                    "Hype Flow needs attention"
                } else if state.phase == "loading" {
                    "Loading speech model"
                } else {
                    "Hype Flow"
                };
                cr.select_font_face(
                    "Noto Sans",
                    gtk::cairo::FontSlant::Normal,
                    gtk::cairo::FontWeight::Normal,
                );
                cr.set_font_size(12.0);
                cr.move_to(x + 46.0, cy - 2.0);
                let clipped: String = title.chars().take(38).collect();
                let _ = cr.show_text(&clipped);
                cr.set_font_size(10.0);
                cr.set_source_rgb(0.60, 0.61, 0.63);
                cr.move_to(x + 46.0, cy + 13.0);
                let detail = if state.phase == "error" {
                    state.message.chars().take(42).collect::<String>()
                } else {
                    "Press your shortcut to speak".into()
                };
                let _ = cr.show_text(&detail);
            }
        });
        let gesture = gtk::GestureClick::new();
        gesture.set_button(0);
        let click_state = state.clone();
        gesture.connect_released(move |gesture, _, _, _| {
            if gesture.current_button() == 3 {
                action("cancel");
            } else if gesture.current_button() == 1 {
                action(
                    if matches!(
                        click_state.borrow().phase.as_str(),
                        "recording" | "starting"
                    ) {
                        "toggle"
                    } else {
                        "show"
                    },
                );
            }
        });
        area.add_controller(gesture);
        Ok(Self {
            window,
            area,
            state,
            ticking: Rc::new(Cell::new(false)),
        })
    }

    pub fn update(&self, phase: &str, message: &str, visible: bool, volume: f64, duration: f64) {
        let mut state = self.state.borrow_mut();
        state.volume = volume;
        state.duration = duration;
        // Audio frames repeat the phase about 20 times a second; skip the accessibility churn.
        if state.phase != phase || state.message != message || self.window.is_visible() != visible {
            if state.phase != phase {
                state.began = glib::monotonic_time();
                state.levels = [0.0; 11];
            }
            state.phase = phase.into();
            state.message = message.into();
            state.target = if phase == "recording" {
                114.0
            } else if matches!(phase, "starting" | "transcribing" | "delivering") {
                48.0
            } else {
                310.0
            };
            self.area
                .update_property(&[gtk::accessible::Property::Label(message)]);
            self.window.set_tooltip_text(Some(if phase == "recording" {
                "Click to finish dictation. Right-click to cancel."
            } else {
                message
            }));
            self.window.set_visible(visible);
        }
        let animating = visible && state.animating(animations());
        drop(state);
        self.area.queue_draw();
        if animating && !self.ticking.replace(true) {
            let state = self.state.clone();
            let ticking = self.ticking.clone();
            self.area.add_tick_callback(move |area, _| {
                area.queue_draw();
                if area.is_mapped() && state.borrow().animating(animations()) {
                    glib::ControlFlow::Continue
                } else {
                    ticking.set(false);
                    glib::ControlFlow::Break
                }
            });
        }
    }
}

fn capsule(cr: &gtk::cairo::Context, x: f64, y: f64, width: f64, height: f64) {
    let radius = height.min(width) / 2.0;
    cr.new_sub_path();
    cr.arc(
        x + width - radius,
        y + radius,
        radius,
        -std::f64::consts::FRAC_PI_2,
        std::f64::consts::FRAC_PI_2,
    );
    cr.arc(
        x + radius,
        y + radius,
        radius,
        std::f64::consts::FRAC_PI_2,
        3.0 * std::f64::consts::FRAC_PI_2,
    );
    cr.close_path();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_ticks_only_while_something_moves() {
        let mut pill = PillState {
            phase: "idle".into(),
            message: "Ready".into(),
            volume: 0.0,
            duration: 0.0,
            levels: [0.0; 11],
            last_sample: 0,
            began: 0,
            width: 310.0,
            target: 310.0,
            input: [0; 4],
        };
        assert!(!pill.animating(true));
        pill.target = 114.0;
        assert!(pill.animating(true));
        pill.phase = "recording".into();
        pill.width = 114.0;
        assert!(pill.animating(false));
        pill.phase = "transcribing".into();
        pill.width = 48.0;
        pill.target = 48.0;
        assert!(pill.animating(true));
        assert!(!pill.animating(false));
    }

    #[test]
    fn refreshed_input_choices_keep_saved_and_selected_devices() {
        let choices = input_choices(
            vec![("usb".into(), "USB microphone".into())],
            &["unplugged", "usb", "auto", "unplugged"],
        );
        assert_eq!(
            choices,
            [
                ("auto", "System default"),
                ("usb", "USB microphone"),
                ("unplugged", "unplugged (unavailable)"),
            ]
            .map(|(id, label)| (id.to_string(), label.to_string()))
        );
    }
}
