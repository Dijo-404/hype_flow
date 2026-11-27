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
        let brand = gtk::Box::new(gtk::Orientation::Horizontal, 6);
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
            // Labels say the same in the wide layout; resize_layout turns tooltips on when compact.
            item.set_has_tooltip(false);
            item.set_child(Some(&content));
            navigation.append(&item);
        }
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
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
            .transition_duration(120)
            .vexpand(true)
            .build();
        content.append(&stack);
        let pages: Vec<gtk::Box> = PAGES
            .iter()
            .map(|(name, _, _)| {
                let page = gtk::Box::new(gtk::Orientation::Vertical, 8);
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
        download_button.set_valign(gtk::Align::Center);
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
        let mut sources = input_choices(Vec::new(), &[]);
        if values.input_target != "auto" {
            // Bare id until the refresh on show knows whether the device is plugged in.
            sources.push((values.input_target.clone(), values.input_target.clone()));
        }
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
            if key == "model_variant" {
                item.append(&download_button);
            }
            rows.push((item, dropdown.clone().upcast()));
            controls.push((
                key.to_string(),
                Control::Choice(
                    dropdown,
                    RefCell::new(choices.into_iter().map(|(id, _)| id).collect()),
                ),
            ));
        }
        let prompt = gtk::Entry::builder()
            .text(&values.prompt)
            .placeholder_text("niri, Hype Flow, project names…")
            .hexpand(true)
            .build();
        let vocabulary_body = gtk::Box::new(gtk::Orientation::Vertical, 3);
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
            group.insert_child_after(&body, group.first_child().as_ref());
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
            let button = gtk::Button::builder()
                .label(button_label)
                .width_request(184)
                .build();
            if name == "clear-recordings" {
                button.add_css_class("destructive-action");
            }
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
        let configure = gtk::Button::builder()
            .label("Configure niri")
            .width_request(184)
            .build();
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
                home.save_button.remove_css_class("saved");
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
            if let Some(parent) = label.parent() {
                parent.set_halign(if compact {
                    gtk::Align::Center
                } else {
                    gtk::Align::Fill
                });
            }
            if let Some(row) = label.ancestor(gtk::ListBoxRow::static_type()) {
                row.set_has_tooltip(compact);
            }
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
            self.save_button.remove_css_class("saved");
            return;
        }
        self.save_button.set_sensitive(false);
        self.save_button.set_label("Saved");
        self.save_button.add_css_class("saved");
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(std::time::Duration::from_millis(1600), move || {
            if let Some(home) = weak.upgrade()
                && !home.save_button.is_sensitive()
            {
                home.save_button.set_label("Save changes");
                home.save_button.remove_css_class("saved");
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

const MORPH_US: i64 = 180_000;
const FADE_IN_US: i64 = 140_000;
const FADE_OUT_US: i64 = 110_000;
// 8px would push the 6px shadow past the 72px surface at the smallest scale.
const RISE: f64 = 5.0;

fn ease_out_cubic(t: f64) -> f64 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

struct PillState {
    phase: String,
    message: String,
    levels: [f64; 11],
    width: f64,
    from: f64,
    target: f64,
    moved: i64,
    shown: f64,
    show_from: f64,
    faded: i64,
    wanted: bool,
    input: [i32; 4],
}

impl PillState {
    fn animating(&self, animated: bool) -> bool {
        if !self.wanted {
            return self.shown != 0.0;
        }
        self.shown != 1.0
            || self.width != self.target
            || (animated && matches!(self.phase.as_str(), "transcribing" | "delivering"))
    }

    /// Returns whether the phase or message changed.
    fn set_phase(&mut self, phase: &str, message: &str, now: i64) -> bool {
        let target = if phase == "recording" {
            112.0
        } else if matches!(phase, "starting" | "transcribing" | "delivering") {
            48.0
        } else {
            310.0
        };
        if target != self.target {
            (self.from, self.target, self.moved) = (self.width, target, now);
        }
        if self.phase == phase && self.message == message {
            return false;
        }
        if self.phase != phase {
            self.levels = [0.0; 11];
        }
        self.phase = phase.into();
        self.message = message.into();
        true
    }

    fn set_wanted(&mut self, wanted: bool, now: i64) {
        if wanted != self.wanted {
            (self.wanted, self.show_from, self.faded) = (wanted, self.shown, now);
        }
    }

    /// Width and presence are pure functions of time, so every frame rate lands exactly.
    fn step(&mut self, now: i64, animated: bool) {
        let t = (now - self.moved) as f64 / MORPH_US as f64;
        self.width = if animated && t < 1.0 {
            self.from + (self.target - self.from) * ease_out_cubic(t)
        } else {
            self.target
        };
        let (goal, length) = if self.wanted {
            (1.0, FADE_IN_US)
        } else {
            (0.0, FADE_OUT_US)
        };
        let t = (now - self.faded) as f64 / length as f64;
        self.shown = if animated && t < 1.0 {
            self.show_from + (goal - self.show_from) * ease_out_cubic(t)
        } else {
            goal
        };
    }
}

fn animations() -> bool {
    gtk::Settings::default().is_none_or(|settings| {
        settings.is_gtk_enable_animations()
            // GTK 4.22+: 1 = GTK_REDUCED_MOTION_REDUCE; read by name to keep the v4_12 floor.
            && !(settings.find_property("gtk-interface-reduced-motion").is_some()
                && glib::EnumValue::from_value(
                    &settings.property_value("gtk-interface-reduced-motion"),
                )
                .is_some_and(|(_, value)| value.value() == 1))
    })
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
            levels: [0.0; 11],
            width: 310.0,
            from: 310.0,
            target: 310.0,
            moved: 0,
            shown: 0.0,
            show_from: 0.0,
            faded: 0,
            wanted: false,
            input: [0; 4],
        }));
        let draw_state = state.clone();
        area.set_draw_func(move |area, cr, width, height| {
            let mut state = draw_state.borrow_mut();
            let now = area
                .frame_clock()
                .map_or_else(glib::monotonic_time, |clock| clock.frame_time());
            let (width, height) = (f64::from(width), f64::from(height));
            draw_pill(cr, &mut state, width, height, now, animations());
            // A fading capsule must not take clicks meant for the window under it.
            let input = if state.wanted {
                let x = (width - state.width) / 2.0;
                let y = (height - 48.0) / 2.0;
                [x as i32, y as i32, state.width.ceil() as i32, 48]
            } else {
                [0; 4]
            };
            if state.input != input
                && let Some(surface) = area.native().and_then(|native| native.surface())
            {
                let region = gtk::cairo::Region::create_rectangle(&gtk::cairo::RectangleInt::new(
                    input[0], input[1], input[2], input[3],
                ));
                surface.set_input_region(Some(&region));
                state.input = input;
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

    pub fn update(&self, phase: &str, message: &str, visible: bool, volume: f64, _duration: f64) {
        let animated = animations();
        let now = glib::monotonic_time();
        let mut state = self.state.borrow_mut();
        state.step(now, animated);
        // A fading capsule keeps its last look; the next visible update carries the current phase.
        let fading = !visible && self.window.is_visible();
        let changed = !fading && state.set_phase(phase, message, now);
        if !fading && phase == "recording" {
            // One bar per audio frame (~20 Hz) so the meter scrolls in step with the mic.
            state.levels.rotate_left(1);
            state.levels[10] = volume;
        }
        // Audio frames repeat the phase about 20 times a second; skip the accessibility churn.
        if changed || state.wanted != visible || self.window.is_visible() != visible {
            self.area
                .update_property(&[gtk::accessible::Property::Label(message)]);
            self.window.set_tooltip_text(Some(if phase == "recording" {
                "Click to finish dictation. Right-click to cancel."
            } else {
                message
            }));
            state.set_wanted(visible, now);
            if visible && !self.window.is_visible() {
                // Appear at the final size; only animate between visible phases.
                (state.width, state.from, state.show_from) = (state.target, state.target, 0.0);
                self.window.set_visible(true);
            } else if !visible && !(animated && self.area.is_mapped()) {
                (state.shown, state.show_from) = (0.0, 0.0);
                self.window.set_visible(false);
            }
        }
        let animating = self.window.is_visible() && state.animating(animated);
        drop(state);
        self.area.queue_draw();
        if animating && !self.ticking.replace(true) {
            let state = self.state.clone();
            let ticking = self.ticking.clone();
            let window = self.window.downgrade();
            self.area.add_tick_callback(move |area, _| {
                area.queue_draw();
                if area.is_mapped() && state.borrow().animating(animations()) {
                    return glib::ControlFlow::Continue;
                }
                // The fade-out ended (or cannot run unmapped): finish the deferred hide.
                if !state.borrow().wanted
                    && let Some(window) = window.upgrade()
                {
                    window.set_visible(false);
                }
                ticking.set(false);
                glib::ControlFlow::Break
            });
        }
    }
}

/// Paints the capsule at `now`; widget-free so tests can render it offscreen.
fn draw_pill(
    cr: &gtk::cairo::Context,
    state: &mut PillState,
    width: f64,
    height: f64,
    now: i64,
    animated: bool,
) {
    state.step(now, animated);
    let presence = state.shown;
    if presence <= 0.0 {
        return;
    }
    if presence < 1.0 {
        let (mx, my) = (width / 2.0, height / 2.0);
        let scale = 0.94 + 0.06 * presence;
        cr.translate(mx, my + (1.0 - presence) * RISE);
        cr.scale(scale, scale);
        cr.translate(-mx, -my);
        cr.push_group();
    }
    let (w, h) = (state.width, 48.0);
    let x = (width - w) / 2.0;
    let y = (height - h) / 2.0;
    let error = state.phase == "error";
    // Stacked fills fake a blurred shadow; cairo has no blur.
    for (spread, alpha) in [
        (6.0, 0.03),
        (5.0, 0.03),
        (4.0, 0.035),
        (3.0, 0.04),
        (2.0, 0.045),
        (1.0, 0.05),
    ] {
        cr.set_source_rgba(0.0, 0.0, 0.0, alpha);
        capsule(
            cr,
            x - spread,
            y - spread + 2.0,
            w + 2.0 * spread,
            h + 2.0 * spread,
        );
        let _ = cr.fill();
    }
    if matches!(state.phase.as_str(), "transcribing" | "delivering") {
        let angle = if animated {
            (now as f64 / 1_600_000.0).fract() * std::f64::consts::TAU
        } else {
            0.0
        };
        cr.set_source_rgba(0.04, 0.52, 1.0, 0.18);
        capsule(cr, x - 4.0, y - 4.0, w + 8.0, h + 8.0);
        let _ = cr.fill();
        // Endpoints mirror through the centre so the highlight circles the ring.
        let (mx, my) = (x + w / 2.0, y + h / 2.0);
        let (dx, dy) = (angle.cos() * w / 2.0, angle.sin() * h / 2.0);
        let gradient = gtk::cairo::LinearGradient::new(mx + dx, my + dy, mx - dx, my - dy);
        gradient.add_color_stop_rgb(0.0, 0.85, 0.93, 1.0);
        gradient.add_color_stop_rgb(0.35, 0.04, 0.52, 1.0);
        gradient.add_color_stop_rgb(1.0, 0.0, 0.25, 0.75);
        let _ = cr.set_source(&gradient);
        capsule(cr, x - 2.0, y - 2.0, w + 4.0, h + 4.0);
        let _ = cr.fill();
    }
    capsule(cr, x, y, w, h);
    cr.set_source_rgb(0.149, 0.149, 0.157);
    let _ = cr.fill_preserve();
    let _ = cr.save();
    cr.clip(); // nothing below may leave the pill while it morphs
    let reveal = (1.0 - (state.target - w).abs() / 40.0).clamp(0.0, 1.0);
    let cx = (x + 24.0).round();
    let cy = height / 2.0;
    if error {
        cr.set_source_rgb(1.0, 0.41, 0.38);
    } else {
        cr.set_source_rgb(0.92, 0.92, 0.92);
    }
    cr.set_line_width(2.0);
    cr.set_line_cap(gtk::cairo::LineCap::Round);
    capsule(cr, cx - 3.0, cy - 10.0, 6.0, 12.0);
    let _ = cr.stroke();
    cr.arc(cx, cy - 1.0, 7.0, 0.0, std::f64::consts::PI);
    let _ = cr.stroke();
    cr.move_to(cx, cy + 6.0);
    cr.line_to(cx, cy + 10.0);
    cr.move_to(cx - 4.0, cy + 10.0);
    cr.line_to(cx + 4.0, cy + 10.0);
    let _ = cr.stroke();
    cr.set_source_rgba(0.92, 0.92, 0.92, reveal);
    if state.phase == "recording" {
        for (index, level) in state.levels.iter().enumerate() {
            let length = 4.0 + level.powf(0.75) * 18.0;
            let bar = cx + 20.0 + index as f64 * 5.0;
            cr.move_to(bar, cy - length / 2.0);
            cr.line_to(bar, cy + length / 2.0);
        }
        let _ = cr.stroke();
    } else if !matches!(
        state.phase.as_str(),
        "starting" | "transcribing" | "delivering"
    ) {
        let hint = "Press your shortcut to speak";
        let (title, detail) = match state.phase.as_str() {
            "error" => (
                state.message.as_str(),
                "Click for details · right-click to dismiss",
            ),
            "done" => state
                .message
                .split_once(" · ")
                .unwrap_or((state.message.as_str(), hint)),
            "loading" => ("Loading speech model", hint),
            _ => ("Hype Flow", hint),
        };
        // Measured against the target so the ellipsis holds still while the pill grows.
        let max = state.target - 66.0;
        cr.select_font_face(
            "Noto Sans",
            gtk::cairo::FontSlant::Normal,
            gtk::cairo::FontWeight::Normal,
        );
        cr.set_font_size(13.0);
        cr.move_to(cx + 22.0, cy - 3.0);
        let _ = cr.show_text(&fit(cr, title, max));
        cr.set_font_size(11.0);
        cr.set_source_rgba(0.62, 0.62, 0.64, reveal);
        cr.move_to(cx + 22.0, cy + 13.0);
        let mut chars = detail.chars();
        let detail: String = chars
            .next()
            .into_iter()
            .flat_map(char::to_uppercase)
            .chain(chars)
            .collect();
        let _ = cr.show_text(&fit(cr, &detail, max));
    }
    let _ = cr.restore();
    // Half-pixel inset puts the 1px hairline on one pixel row instead of two half-lit ones.
    capsule(cr, x + 0.5, y + 0.5, w - 1.0, h - 1.0);
    if error {
        cr.set_source_rgba(1.0, 0.41, 0.38, 0.55);
    } else {
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.10);
    }
    cr.set_line_width(1.0);
    let _ = cr.stroke();
    if presence < 1.0 {
        let _ = cr.pop_group_to_source();
        let _ = cr.paint_with_alpha(presence);
    }
}

/// Cuts `text` to `max` pixels with an ellipsis; the toy text API has no layout.
fn fit(cr: &gtk::cairo::Context, text: &str, max: f64) -> String {
    let width = |text: &str| {
        cr.text_extents(text)
            .map_or(0.0, |extents| extents.x_advance())
    };
    if width(text) <= max {
        return text.into();
    }
    let mut cut = text.to_string();
    while cut.pop().is_some() {
        let candidate = format!("{}…", cut.trim_end());
        if width(&candidate) <= max {
            return candidate;
        }
    }
    "…".into()
}

/// Full-radius rounded rectangle: a horizontal or vertical pill.
fn capsule(cr: &gtk::cairo::Context, x: f64, y: f64, width: f64, height: f64) {
    use std::f64::consts::{FRAC_PI_2, PI};
    let radius = height.min(width) / 2.0;
    cr.new_sub_path();
    cr.arc(x + width - radius, y + radius, radius, -FRAC_PI_2, 0.0);
    cr.arc(
        x + width - radius,
        y + height - radius,
        radius,
        0.0,
        FRAC_PI_2,
    );
    cr.arc(x + radius, y + height - radius, radius, FRAC_PI_2, PI);
    cr.arc(x + radius, y + radius, radius, PI, 3.0 * FRAC_PI_2);
    cr.close_path();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idle_pill() -> PillState {
        PillState {
            phase: "idle".into(),
            message: "Ready".into(),
            levels: [0.0; 11],
            width: 310.0,
            from: 310.0,
            target: 310.0,
            moved: 0,
            shown: 1.0,
            show_from: 1.0,
            faded: 0,
            wanted: true,
            input: [0; 4],
        }
    }

    /// A shown capsule that has finished morphing into `phase`.
    fn settled_pill(phase: &str, message: &str) -> PillState {
        let mut pill = idle_pill();
        pill.set_phase(phase, message, 0);
        pill.step(MORPH_US, true);
        pill
    }

    fn render(pill: &mut PillState, now: i64, background: Option<f64>) -> gtk::cairo::ImageSurface {
        let surface =
            gtk::cairo::ImageSurface::create(gtk::cairo::Format::ARgb32, 340, 72).unwrap();
        let cr = gtk::cairo::Context::new(&surface).unwrap();
        if let Some(shade) = background {
            cr.set_source_rgb(shade, shade, shade);
            cr.paint().unwrap();
        }
        draw_pill(&cr, pill, 340.0, 72.0, now, true);
        drop(cr);
        surface.flush();
        surface
    }

    /// Premultiplied BGRA rows, 340 pixels each.
    fn pixels(mut surface: gtk::cairo::ImageSurface) -> Vec<Vec<[u8; 4]>> {
        let stride = surface.stride() as usize;
        let data = surface.data().unwrap();
        data.chunks(stride)
            .map(|row| {
                row[..340 * 4]
                    .chunks(4)
                    .map(|pixel| pixel.try_into().unwrap())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn overlay_ticks_only_while_something_moves() {
        let mut pill = idle_pill();
        assert!(!pill.animating(true));
        pill.target = 112.0;
        assert!(pill.animating(true));
        pill.phase = "recording".into();
        pill.width = 112.0;
        // Each audio frame queues the redraw of a settled recording capsule.
        assert!(!pill.animating(true));
        pill.shown = 0.5;
        assert!(pill.animating(false));
        pill.shown = 1.0;
        pill.phase = "transcribing".into();
        pill.width = 48.0;
        pill.target = 48.0;
        assert!(pill.animating(true));
        assert!(!pill.animating(false));
    }

    #[test]
    fn overlay_width_settles_exactly_so_the_tick_stops() {
        let mut pill = idle_pill();
        pill.set_phase("transcribing", "Transcribing", 0);
        pill.step(MORPH_US / 2, true);
        assert!(pill.width > 48.0 && pill.width < 310.0 && pill.animating(false));
        pill.step(MORPH_US, true);
        assert_eq!(pill.width, 48.0);
        assert!(!pill.animating(false));
    }

    #[test]
    fn overlay_motion_ignores_the_frame_rate() {
        let (mut slow, mut fast) = (idle_pill(), idle_pill());
        for pill in [&mut slow, &mut fast] {
            pill.set_phase("starting", "Opening microphone", 0);
            (pill.wanted, pill.shown) = (false, 0.0);
            pill.set_wanted(true, 0);
        }
        for frame in 0..6 {
            slow.step(frame * 16_667, true);
        }
        for frame in 0..15 {
            fast.step(frame * 6_944, true);
        }
        slow.step(100_000, true);
        fast.step(100_000, true);
        assert!(slow.width > 48.0 && slow.width < 310.0 && slow.shown < 1.0);
        assert_eq!((slow.width, slow.shown), (fast.width, fast.shown));
    }

    #[test]
    fn overlay_fade_out_ends_hidden_and_reverses_smoothly() {
        let mut pill = idle_pill();
        pill.set_wanted(false, 0);
        pill.step(FADE_OUT_US / 2, true);
        let midway = pill.shown;
        assert!(midway > 0.0 && midway < 1.0 && pill.animating(true));
        pill.set_wanted(true, FADE_OUT_US / 2);
        pill.step(FADE_OUT_US / 2, true);
        assert_eq!(pill.shown, midway);
        pill.step(FADE_OUT_US / 2 + FADE_IN_US, true);
        assert_eq!(pill.shown, 1.0);
        pill.set_wanted(false, 1_000_000);
        pill.step(1_000_000 + FADE_OUT_US, true);
        assert_eq!(pill.shown, 0.0);
        assert!(!pill.animating(true));
        pill.set_wanted(true, 2_000_000);
        pill.step(2_000_000, false);
        assert_eq!(pill.shown, 1.0);
    }

    #[test]
    fn repeated_audio_frames_keep_the_overlay_phase() {
        let mut pill = idle_pill();
        assert!(pill.set_phase("recording", "Listening", 0));
        pill.levels[10] = 0.6;
        assert!(!pill.set_phase("recording", "Listening", 50_000));
        assert_eq!((pill.moved, pill.levels[10], pill.target), (0, 0.6, 112.0));
        assert!(pill.set_phase("recording", "Microphone busy", 100_000));
        assert_eq!(pill.levels[10], 0.6);
    }

    #[test]
    fn overlay_content_never_leaves_the_capsule_while_it_grows() {
        let error = "Paste failed. Use Copy transcript in Hype Flow. wtype: compositor does not support virtual keyboard";
        for (from, phase, message) in [
            (
                "delivering",
                "done",
                "Microphone is silent · check it is not muted",
            ),
            ("recording", "error", error),
        ] {
            let mut pill = settled_pill(from, "");
            pill.set_phase(phase, message, 0);
            let mut text = 0;
            for now in (0..=MORPH_US + 20_000).step_by(2_000) {
                let rows = pixels(render(&mut pill, now, None));
                let (left, right) = (
                    (340.0 - pill.width) / 2.0 + 24.0,
                    (340.0 + pill.width) / 2.0 - 24.0,
                );
                text = 0;
                for (row, pixels) in rows.iter().enumerate() {
                    for (column, pixel) in pixels.iter().enumerate() {
                        let (px, py) = (column as f64 + 0.5, row as f64 + 0.5);
                        // One pixel of slack covers the edge antialiasing.
                        let inside = (px - px.clamp(left, right)).hypot(py - 36.0) <= 25.0;
                        // Only the black shadow may paint outside the capsule.
                        assert!(
                            inside || pixel[..3] == [0, 0, 0],
                            "{phase} at {now}us: ink at {column},{row}"
                        );
                        text += usize::from(column > 61 && pixel[1] > 128);
                    }
                }
            }
            assert_eq!(pill.width, 310.0);
            assert!(text > 100, "{phase} shows its message once settled");
        }
    }

    #[test]
    fn overlay_frames_fit_the_surface() {
        let error = "Paste failed. Use Copy transcript in Hype Flow. wtype: compositor does not support virtual keyboard";
        let phases = [
            ("idle", "Ready"),
            ("loading", "Loading speech model"),
            ("starting", "Opening microphone"),
            ("recording", "Listening"),
            ("transcribing", "Transcribing"),
            ("delivering", "Copying transcript"),
            ("done", "Microphone is silent · check it is not muted"),
            ("error", error),
        ];
        for (phase, message) in phases {
            for presence in [0.01, 0.1, 0.3, 0.5, 0.8, 1.0] {
                // Ring angles 0, 90, 180 and 270 degrees.
                for now in [1_600_000, 2_000_000, 2_400_000, 2_800_000] {
                    let mut pill = settled_pill(phase, message);
                    pill.levels = [1.0; 11];
                    (pill.show_from, pill.faded) = (presence, now);
                    let rows = pixels(render(&mut pill, now, None));
                    let mut edges = rows[0]
                        .iter()
                        .chain(&rows[71])
                        .chain(rows.iter().flat_map(|row| [&row[0], &row[339]]));
                    assert!(
                        edges.all(|pixel| pixel[3] == 0),
                        "{phase} at presence {presence} touches the surface edge"
                    );
                }
            }
        }
        let Some(directory) = std::env::var_os("HYPE_FLOW_UI_CAPTURE_DIR") else {
            return;
        };
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        // Late enough that every width morph has settled.
        const T: i64 = 1_000_000;
        let mut recording = settled_pill("recording", "Listening");
        recording.levels = [0.05, 0.2, 0.5, 0.9, 1.0, 0.7, 0.4, 0.3, 0.6, 0.15, 0.02];
        let mut growing = settled_pill("delivering", "Copying transcript");
        growing.set_phase("done", "Copied · focus changed before paste", T);
        let mut appearing = settled_pill("recording", "Listening");
        (appearing.wanted, appearing.shown) = (false, 0.0);
        appearing.set_wanted(true, T);
        let mut leaving = settled_pill("done", "Pasted");
        leaving.set_wanted(false, T);
        let long =
            "Copied because the focused window changed before paste · check the target window";
        let frames = [
            ("idle", settled_pill("idle", "Ready"), T),
            (
                "starting",
                settled_pill("starting", "Opening microphone"),
                T,
            ),
            ("recording", recording, T),
            (
                "transcribing-0",
                settled_pill("transcribing", "Transcribing"),
                1_600_000,
            ),
            (
                "transcribing-270",
                settled_pill("transcribing", "Transcribing"),
                2_800_000,
            ),
            ("done", settled_pill("done", "Pasted · 2 words filtered"), T),
            ("done-long", settled_pill("done", long), T),
            ("error", settled_pill("error", error), T),
            ("mid-morph", growing, T + 90_000),
            ("mid-appear", appearing, T + FADE_IN_US / 5),
            ("mid-leave", leaving, T + FADE_OUT_US / 3),
        ];
        for (name, mut pill, now) in frames {
            for (shade, background) in [("dark", 0.118), ("light", 0.95)] {
                let mut surface = render(&mut pill, now, Some(background));
                let stride = surface.stride() as usize;
                let bytes = glib::Bytes::from(&*surface.data().unwrap());
                // cairo ARGB32 is native-endian, so BGRA bytes on little-endian hosts.
                gdk::MemoryTexture::new(
                    340,
                    72,
                    gdk::MemoryFormat::B8g8r8a8Premultiplied,
                    &bytes,
                    stride,
                )
                .save_to_png(directory.join(format!("{name}-{shade}.png")))
                .unwrap();
            }
        }
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
