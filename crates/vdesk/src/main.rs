use std::os::unix::process::CommandExt;

use clap::Parser;
use vdesk::cli::{Cli, run};

#[tokio::main(worker_threads = 2)]
async fn main() {
    // SAFETY: This restores conventional Unix pipeline behavior before worker threads start.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    if matches!(
        std::env::args_os()
            .next()
            .as_deref()
            .and_then(|value| std::path::Path::new(value).file_name())
            .and_then(|name| name.to_str()),
        Some("chromium" | "vdesk-chromium")
    ) {
        let error = std::process::Command::new("/usr/bin/chromium")
            .arg("--no-sandbox")
            .args(std::env::args_os().skip(1))
            .exec();
        eprintln!("error: could not launch Chromium: {error}");
        std::process::exit(127);
    }
    let cli = Cli::parse();
    if let Err(error) = run(cli).await {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}
