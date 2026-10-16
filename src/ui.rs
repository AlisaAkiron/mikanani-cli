//! Shared terminal helpers: prompt plumbing and the progress spinner.

use anyhow::{Context, Result};
use inquire::validator::Validation;
use inquire::{CustomUserError, InquireError};
use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Collapses inquire's two cancellation signals (Esc ⇒ Ok(None), Ctrl-C ⇒
/// OperationInterrupted) into Ok(None); real errors keep their context.
pub fn take_or_cancel<T>(
    result: Result<Option<T>, InquireError>,
    what: &str,
) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(value),
        Err(InquireError::OperationInterrupted) => Ok(None),
        Err(e) => Err(e).with_context(|| format!("获取{what}")),
    }
}

/// A text validator rejecting blank input with `message`.
pub fn required(
    message: &'static str,
) -> impl Fn(&str) -> Result<Validation, CustomUserError> + Clone {
    move |input: &str| {
        if input.trim().is_empty() {
            Ok(Validation::Invalid(message.into()))
        } else {
            Ok(Validation::Valid)
        }
    }
}

/// Report a user cancellation and return the (successful) exit code for it.
pub fn cancelled() -> i32 {
    println!("已取消");
    0
}

/// An animated one-line status printed to stderr until dropped, then the
/// line is cleared. Reassures the user during blocking network calls — the
/// feed fetch and qBittorrent connect can stall for seconds behind a proxy or
/// a wrong endpoint. A no-op when stderr isn't a terminal, so pipes and logs
/// stay clean.
pub struct Spinner {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Spinner {
    pub fn start(message: &str) -> Spinner {
        let stop = Arc::new(AtomicBool::new(false));
        if !std::io::stderr().is_terminal() {
            return Spinner { stop, handle: None };
        }
        let message = message.to_string();
        let flag = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            let mut err = std::io::stderr();
            let mut frame = 0usize;
            while !flag.load(Ordering::Relaxed) {
                let _ = write!(err, "\r{} {message}", FRAMES[frame % FRAMES.len()]);
                let _ = err.flush();
                frame += 1;
                std::thread::sleep(Duration::from_millis(80));
            }
            let _ = write!(err, "\r\x1b[2K"); // clear the line on the way out
            let _ = err.flush();
        });
        Spinner { stop, handle: Some(handle) }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
