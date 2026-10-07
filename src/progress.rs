//! Progress of a transfer on stderr, for the ones that take long. Nothing in the first second, so
//! a warm sync stays as quiet as it was; after that a line redrawn in place on a terminal, or a
//! line every few seconds anywhere else (the build window of Android Studio keeps every line).

use crate::client::human;
use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};

const SILENT_FOR: Duration = Duration::from_secs(1);
const REDRAW: Duration = Duration::from_millis(100);
const LOG_EVERY: Duration = Duration::from_secs(5);

pub struct Progress {
    label: &'static str,
    /// what the transfer will move in all, when its sender knows (a push); a pull finds out as it goes
    total: Option<u64>,
    done: u64,
    start: Instant,
    drawn: Option<Instant>,
    tty: bool,
    /// length of the line on the terminal now: the next one covers it (no escape codes, for a Windows console)
    width: usize,
    on: bool,
}

impl Progress {
    /// `on: false` (`--quiet`) only counts.
    pub fn new(label: &'static str, total: Option<u64>, on: bool) -> Self {
        Self {
            label,
            total,
            done: 0,
            start: Instant::now(),
            drawn: None,
            tty: io::stderr().is_terminal(),
            width: 0,
            on,
        }
    }

    /// `n` more bytes went through.
    pub fn add(&mut self, n: u64) {
        self.done += n;
        if !self.on {
            return;
        }
        let now = Instant::now();
        let (since, wait) = match self.drawn {
            None => (self.start, SILENT_FOR),
            Some(at) => (at, if self.tty { REDRAW } else { LOG_EVERY }),
        };
        if now.duration_since(since) < wait {
            return;
        }
        let text = line(self.label, self.done, self.total, now.duration_since(self.start));
        let mut err = io::stderr().lock();
        let _ = if self.tty {
            let covered = self.width;
            self.width = text.len();
            write!(err, "\r{text:<covered$}")
        } else {
            writeln!(err, "{text}")
        };
        let _ = err.flush();
        self.drawn = Some(now);
    }
}

impl Drop for Progress {
    /// Takes the line off the terminal again: the summary of the phase, or its error, comes next.
    fn drop(&mut self) {
        if self.width > 0 {
            let mut err = io::stderr().lock();
            let _ = write!(err, "\r{:1$}\r", "", self.width);
            let _ = err.flush();
        }
    }
}

/// `push   42.0 MB of 95.4 MB, 12.1 MB/s`, aligned with the summary lines of a run.
fn line(label: &str, done: u64, total: Option<u64>, elapsed: Duration) -> String {
    let rate = human((done as f64 / elapsed.as_secs_f64().max(0.001)) as u64);
    match total {
        Some(total) => format!("{label:<6} {} of {}, {rate}/s", human(done), human(total)),
        None => format!("{label:<6} {}, {rate}/s", human(done)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_shows_the_total_when_there_is_one_and_the_average_rate() {
        let two_seconds = Duration::from_secs(2);
        assert_eq!(
            line("push", 42 << 20, Some(100 << 20), two_seconds),
            "push   42.0 MB of 100.0 MB, 21.0 MB/s"
        );
        assert_eq!(line("pull", 3 << 20, None, two_seconds), "pull   3.0 MB, 1.5 MB/s");
        // no time at all yet: a rate, not a division by zero
        assert!(line("pull", 1024, None, Duration::ZERO).ends_with("KB/s"));
    }

    #[test]
    fn a_progress_that_is_off_only_counts() {
        let mut p = Progress::new("push", None, false);
        p.start -= Duration::from_secs(10);
        p.add(5);
        p.add(7);
        assert_eq!(p.done, 12);
        assert!(p.drawn.is_none());
    }
}
