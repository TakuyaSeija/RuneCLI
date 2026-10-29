//! The desktop owns the browser. No shell is involved and no file is opened.
use std::io;
use std::process::{Command, Stdio};

pub fn open_browser(url: &crate::platform::BrowserUrl) -> io::Result<()> {
    let program = match crate::platform::OS {
        crate::platform::Os::MacOs => std::path::PathBuf::from("/usr/bin/open"),
        crate::platform::Os::Linux => super::find_program("xdg-open").ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "No desktop URL opener is available. Copy the destination into your browser.",
            )
        })?,
        crate::platform::Os::Windows => unreachable!("Unix browser opener"),
    };
    let mut child = Command::new(program)
        .arg(url.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match child.try_wait()? {
            Some(status) if status.success() => return Ok(()),
            Some(status) => {
                return Err(io::Error::other(format!(
                "The desktop URL opener failed ({status}). Copy the destination into your browser."
            )))
            }
            None if std::time::Instant::now() >= deadline => {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return Ok(());
            }
            None => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    }
}
