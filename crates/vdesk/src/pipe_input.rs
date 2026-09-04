use std::io::{self, BufRead};
use std::os::unix::net::UnixDatagram;
use std::path::Path;

use anyhow::{Context, Result};

pub fn run(socket_path: &Path) -> Result<()> {
    let socket = UnixDatagram::unbound().context("create human-input notifier")?;
    socket.set_nonblocking(true).context("make human-input notifier nonblocking")?;
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        match line {
            Ok(line) if !line.trim().is_empty() => {
                // Input observation must never slow or block the human's RFB path. A full or
                // temporarily missing datagram socket loses a generation hint rather than input.
                let _ = socket.send_to(b"1", socket_path);
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    Ok(())
}
