#![cfg(feature = "desktop")]

use gtk::{gio, glib, prelude::*};
use gtk4_layer_shell::LayerShell;
use hype_flow::{
    config::Config,
    ui::{Control, Home, Overlay, PAGES},
};
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

fn settle() {
    let deadline = Instant::now() + Duration::from_millis(250);
    while Instant::now() < deadline {
        while glib::MainContext::default().iteration(false) {}
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
#[ignore = "run under an isolated D-Bus/Wayland session with tools/check-desktop.sh"]
fn settings_pages_save_validation_compact_layout_and_layer_surface() {
    gtk::init().unwrap();
    let app = gtk::Application::builder()
        .application_id("dev.hype.Flow.UiTest")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.register(gio::Cancellable::NONE).unwrap();
    let saved = Rc::new(RefCell::new(Vec::new()));
    let actions = Rc::new(RefCell::new(Vec::new()));
    let saved_copy = saved.clone();
    let actions_copy = actions.clone();
    let values = Config {
        model: "/test/old-model".into(),
        server: "/test/old-worker".into(),
        threads: 3,
        ..Config::default()
    };
    let home = Home::new(
        &app,
        &values,
        Rc::new(move |name| actions_copy.borrow_mut().push(name.to_string())),
        Rc::new(move |values| {
            saved_copy.borrow_mut().push(values);
            true
        }),
    );
    home.window.present();
    settle();
    for (index, (name, _, _)) in PAGES.iter().enumerate() {
        home.navigation
            .select_row(home.navigation.row_at_index(index as i32).as_ref());
        settle();
        assert_eq!(home.stack.visible_child_name().as_deref(), Some(*name));
        if let Some(directory) = std::env::var_os("HYPE_FLOW_UI_CAPTURE_DIR") {
            let directory = std::path::PathBuf::from(directory);
            std::fs::create_dir_all(&directory).unwrap();
            let snapshot = gtk::Snapshot::new();
            gtk::WidgetPaintable::new(Some(&home.window)).snapshot(
                &snapshot,
                f64::from(home.window.width()),
                f64::from(home.window.height()),
            );
            let node = snapshot.to_node().unwrap();
            home.window
                .renderer()
                .unwrap()
                .render_texture(&node, None)
                .save_to_png(directory.join(format!("{name}.png")))
                .unwrap();
        }
    }
    for (key, control) in &home.controls {
        match (key.as_str(), control) {
            ("prompt", Control::Entry(entry)) => entry.set_text("नमस्ते, niri"),
            ("paste", Control::Switch(switch)) => switch.set_active(false),
            ("boost_score", Control::Number(spin)) => spin.set_value(2.0),
            ("corrections", Control::Text(buffer)) => buffer.set_text("open ai => OpenAI"),
            _ => {}
        }
    }
    assert!(home.save_button.is_sensitive());
    home.save_button.emit_clicked();
    let config = saved.borrow()[0].clone();
    assert_eq!(config.prompt, "नमस्ते, niri");
    assert!(!config.paste);
    assert_eq!(config.boost_score, 2.0);
    assert_eq!(config.corrections, "open ai => OpenAI");
    assert_eq!(config.model, values.model);
    assert_eq!(config.threads, 3);
    assert!(!home.save_button.is_sensitive());
    for (key, control) in &home.controls {
        if key == "corrections"
            && let Control::Text(buffer) = control
        {
            buffer.set_text("invalid rule");
        }
    }
    home.save_button.emit_clicked();
    assert_eq!(saved.borrow().len(), 1);
    assert!(!home.status.text().is_empty());
    home.update("error", "Test error details", true);
    assert_eq!(home.status.text(), "Test error details");
    assert_eq!(home.badge.text(), "Needs attention");
    assert!(home.status.has_css_class("error"));
    home.resize_layout(660);
    assert_eq!(
        home.controls
            .iter()
            .find(|(name, _)| name == "paste")
            .unwrap()
            .0,
        "paste"
    );
    home.resize_layout(960);
    let overlay = Overlay::new(Rc::new(|_| {})).unwrap();
    overlay.update("recording", "Listening", true, 0.4, 3.0);
    settle();
    assert_eq!(
        overlay.window.keyboard_mode(),
        gtk4_layer_shell::KeyboardMode::None
    );
    assert_eq!(overlay.window.layer(), gtk4_layer_shell::Layer::Overlay);
    assert!(overlay.window.is_visible());
    overlay.update("idle", "Ready", false, 0.0, 0.0);
    assert!(!overlay.window.is_visible());
    overlay.window.close();
    home.window.destroy();
}
