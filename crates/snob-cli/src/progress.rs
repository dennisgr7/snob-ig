//! Turns walk events into a progress bar.
//!
//! **Rule of this module: no data ever goes through `pb.println()`.** That
//! method silently discards output when there is no terminal, so using it for
//! results would make them vanish when redirecting to a file. Data goes to
//! standard output and this bar to standard error, so the two never collide.

use std::time::Duration;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use snob_ig::pager::{Event, WaitKind};

/// Cloning shares the same bar rather than making a second one: the pacer
/// announces its waits from inside the client and has to reach the bar the walk
/// is already drawing.
#[derive(Clone)]
pub struct Progress {
    bar: ProgressBar,
    quiet: bool,
}

impl Progress {
    pub fn new(enabled: bool) -> Self {
        let bar = if enabled {
            // `indicatif` already hides itself when standard error is not a
            // terminal, so this only covers being asked to hide it explicitly.
            ProgressBar::with_draw_target(None, ProgressDrawTarget::stderr())
        } else {
            ProgressBar::hidden()
        };
        bar.set_style(style_without_total());
        bar.enable_steady_tick(Duration::from_millis(120));

        Self {
            bar,
            quiet: !enabled,
        }
    }

    /// Takes a walk event and reflects it.
    pub fn event(&self, e: &Event) {
        match e {
            Event::Started { estimated, resumed } => {
                if let Some(total) = estimated {
                    self.bar.set_length(*total);
                    self.bar.set_style(style_with_total());
                }
                if *resumed {
                    self.warn("continuing an interrupted walk");
                }
            }
            Event::Page {
                number,
                running_total,
                ..
            } => {
                let running_total = *running_total as u64;
                // Instagram's counter sometimes undercounts. If we overshoot,
                // stretch the bar rather than leaving it past one hundred
                // percent.
                if self.bar.length().is_some_and(|l| running_total > l) {
                    self.bar.set_length(running_total);
                }
                self.bar.set_position(running_total);
                self.bar
                    .set_message(format!("page {number}, {running_total} accounts"));
            }
            Event::Waiting { kind, duration } => {
                if *kind == WaitKind::Long {
                    self.note(&format!(
                        "waiting {} s to keep the pace down",
                        duration.as_secs().max(1)
                    ));
                }
            }
            Event::Retrying {
                attempt,
                after,
                error,
            } => self.warn(&format!(
                "retry {attempt} in {} s after a failure: {error}",
                after.as_secs().max(1)
            )),
            Event::Warning(text) => self.warn(text),
            Event::Finished { .. } => self.bar.finish_and_clear(),
        }
    }

    /// Says what the run is doing right now. It replaces the bar's own message
    /// rather than scrolling, because this is transient: it stops being true as
    /// soon as the next page arrives.
    ///
    /// With no bar it goes to standard error, where every other message the
    /// user reads already goes.
    pub fn note(&self, text: &str) {
        if self.quiet {
            eprintln!("{text}");
        } else {
            self.bar.set_message(text.to_string());
        }
    }

    /// A message above the bar, without disturbing the drawing.
    pub fn warn(&self, text: &str) {
        if self.quiet {
            eprintln!("warning: {text}");
        } else {
            self.bar.suspend(|| eprintln!("warning: {text}"));
        }
    }

    pub fn finish(&self) {
        self.bar.finish_and_clear();
    }
}

fn style_without_total() -> ProgressStyle {
    ProgressStyle::with_template("{spinner} {msg}").unwrap_or_else(|_| ProgressStyle::default_bar())
}

fn style_with_total() -> ProgressStyle {
    // No percentage and no ETA: the total comes from a counter that can lie,
    // and a bar going past one hundred percent looks worse than no bar at all.
    ProgressStyle::with_template("{bar:30} {pos}/{len} {msg}")
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("=> ")
}
