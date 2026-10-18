fn main() {
    // `flashtester list | head`: stdout closing early ends the program quietly
    // (141, as a shell reports SIGPIPE) instead of a "failed printing" panic.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = info.payload().downcast_ref::<String>().map(String::as_str).or_else(|| info.payload().downcast_ref::<&str>().copied()).unwrap_or("");
        // EPIPE on Unix; ERROR_NO_DATA and ERROR_BROKEN_PIPE on Windows.
        if msg.starts_with("failed printing to std") && ["(os error 32)", "(os error 232)", "(os error 109)"].iter().any(|c| msg.ends_with(c)) {
            std::process::exit(141);
        }
        hook(info)
    }));
    // No arguments: GUI (on Linux and FreeBSD only inside an X11 or XWayland desktop). Any arguments: CLI.
    #[cfg(any(windows, target_os = "linux", target_os = "freebsd"))]
    if std::env::args_os().len() <= 1 && (cfg!(windows) || std::env::var_os("DISPLAY").is_some_and(|d| !d.is_empty())) {
        std::process::exit(flashtester::gui::run());
    }
    std::process::exit(flashtester::cli::main());
}
