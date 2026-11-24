fn main() -> gtk::glib::ExitCode {
    // Every runtime file starts private, including diagnostics created by children.
    unsafe {
        libc::umask(0o077);
    }
    if std::env::args_os().skip(1).eq(["--setup"]) {
        return match hype_flow::config::Config::load().and_then(|values| {
            hype_flow::setup::ensure_installed(&values)?;
            println!("{}", hype_flow::shortcuts::install()?);
            Ok(())
        }) {
            Ok(()) => gtk::glib::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("Hype Flow setup failed: {error:#}");
                gtk::glib::ExitCode::FAILURE
            }
        };
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
