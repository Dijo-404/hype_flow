fn main() -> gtk::glib::ExitCode {
    // Every runtime file starts private, including diagnostics created by children.
    unsafe {
        libc::umask(0o077);
    }
    match hype_flow::config::Config::load().and_then(hype_flow::app::run) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("Hype Flow could not start: {error:#}");
            if gtk::init().is_ok() {
                let dialog = gtk::AlertDialog::builder()
                    .message("Hype Flow could not start")
                    .detail(format!("{error:#}"))
                    .buttons(["Close"])
                    .build();
                let loop_ = gtk::glib::MainLoop::new(None, false);
                let close = loop_.clone();
                dialog.choose(gtk::Window::NONE, gtk::gio::Cancellable::NONE, move |_| {
                    close.quit()
                });
                loop_.run();
            }
            gtk::glib::ExitCode::FAILURE
        }
    }
}
