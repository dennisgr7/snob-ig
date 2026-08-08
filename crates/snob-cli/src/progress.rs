//! Turns walk events into a progress bar.
//!
//! **Rule of this module: no data ever goes through `pb.println()`.** That
//! method silently discards output when there is no terminal, so using it for
//! results would make them vanish when redirecting to a file. Data goes to
//! standard output and this bar to standard error, so the two never collide.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use indicatif::style::ProgressTracker;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressState, ProgressStyle};
use snob_ig::pager::{Event, WaitKind};

/// The moment the wait being drawn right now ends, if one is under way.
///
/// Shared with the style, which reads it on every redraw. That sharing is the
/// whole trick behind the countdown: the bar already ticks several times a
/// second to animate itself, and this gives it something new to say each time
/// rather than the same frozen number.
type Deadline = Arc<Mutex<Option<Instant>>>;

/// Cloning shares the same bar rather than making a second one: the pacer
/// announces its waits from inside the client and has to reach the bar the walk
/// is already drawing.
#[derive(Clone)]
pub struct Progress {
    bar: ProgressBar,
    quiet: bool,
    waiting_until: Deadline,
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
        let waiting_until: Deadline = Arc::new(Mutex::new(None));
        bar.set_style(style_without_total(&waiting_until));
        bar.enable_steady_tick(Duration::from_millis(120));

        Self {
            bar,
            quiet: !enabled,
            waiting_until,
        }
    }

    /// Says that nothing will happen for a while, and keeps saying how much
    /// longer.
    ///
    /// The number is deliberately **not** written into the message. It is read
    /// from the deadline by the style, on every redraw, so it counts down — the
    /// old behaviour put the number in the text once and then left it there,
    /// which meant fifteen seconds of a bar insisting there were fifteen
    /// seconds left.
    ///
    /// With no bar to draw on, it is said once with the number in it, because
    /// there is nothing to animate and a line per second down a pipe would be
    /// noise.
    pub fn waiting(&self, reason: &str, duration: Duration) {
        self.set_deadline(Some(Instant::now() + duration));
        if self.quiet {
            eprintln!("{reason} ({} s)", seconds_left(duration));
        } else {
            self.bar.set_message(reason.to_string());
        }
    }

    fn set_deadline(&self, at: Option<Instant>) {
        *self.waiting_until.lock().unwrap_or_else(|e| e.into_inner()) = at;
    }

    /// Takes a walk event and reflects it.
    pub fn event(&self, e: &Event) {
        match e {
            Event::Started { estimated, resumed } => {
                if let Some(total) = estimated {
                    self.bar.set_length(*total);
                    self.bar.set_style(style_with_total(&self.waiting_until));
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
                // A page arriving is the wait being over.
                self.set_deadline(None);
            }
            Event::Waiting { kind, duration } => {
                // Only the long pause is announced. The micro pause and the
                // cycle wait are a second or two between every page, and a
                // message that appeared and vanished that often would be
                // harder to read than the spinner already saying the same
                // thing.
                if *kind == WaitKind::Long {
                    self.waiting("pausing to keep the pace down", *duration);
                }
            }
            Event::Retrying {
                attempt,
                after,
                error,
            } => {
                // The line scrolls above the bar because a retry is worth
                // keeping; the countdown runs on the bar itself, where it can
                // move.
                self.warn(&format!("retry {attempt} after a failure: {error}"));
                self.waiting(&format!("waiting before retry {attempt}"), *after);
            }
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
        // Whatever this is about, it is not the wait that was being counted
        // down, and leaving the old deadline would keep a number ticking next
        // to a message it has nothing to do with.
        self.set_deadline(None);
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

/// Named rather than written inline, because the fallback below swallows a
/// broken one: a typo here would not fail, it would quietly draw indicatif's
/// default bar with no countdown in it and nobody would know why. A test
/// checks that both of these parse.
const TEMPLATE_WITHOUT_TOTAL: &str = "{spinner} {msg}{countdown}";
/// No percentage and no ETA: the total comes from a counter that can lie, and a
/// bar going past one hundred percent looks worse than no bar at all.
const TEMPLATE_WITH_TOTAL: &str = "{bar:30} {pos}/{len} {msg}{countdown}";

fn style_without_total(deadline: &Deadline) -> ProgressStyle {
    ProgressStyle::with_template(TEMPLATE_WITHOUT_TOTAL)
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .with_key("countdown", countdown(deadline))
}

fn style_with_total(deadline: &Deadline) -> ProgressStyle {
    ProgressStyle::with_template(TEMPLATE_WITH_TOTAL)
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("=> ")
        .with_key("countdown", countdown(deadline))
}

/// Renders the time left in the current wait, or nothing at all.
///
/// Called by the bar on every redraw, which is what makes the number move. It
/// holds the lock for the length of a subtraction, several times a second, and
/// nothing else contends for it.
fn countdown(deadline: &Deadline) -> impl ProgressTracker + 'static {
    let deadline = Arc::clone(deadline);
    move |_: &ProgressState, w: &mut dyn std::fmt::Write| {
        let Some(end) = *deadline.lock().unwrap_or_else(|e| e.into_inner()) else {
            return;
        };
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        let _ = write!(w, " — {} s", seconds_left(left));
    }
}

/// Seconds remaining, rounded up.
///
/// Up rather than down so that the last second of a wait reads as `1 s` and
/// then disappears, instead of sitting on `0 s` for a whole second.
fn seconds_left(left: Duration) -> u128 {
    left.as_millis().div_ceil(1_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A broken template does not fail, it falls back to indicatif's default
    /// bar — which has no `{countdown}` in it, so the countdown would simply
    /// never appear and nothing would say why.
    #[test]
    fn both_templates_parse() {
        for template in [TEMPLATE_WITHOUT_TOTAL, TEMPLATE_WITH_TOTAL] {
            assert!(
                ProgressStyle::with_template(template).is_ok(),
                "{template} does not parse, so the bar would silently lose it"
            );
        }
    }

    /// Rounded up, so the last fraction of a wait reads as `1 s` and then goes
    /// away, rather than sitting on `0 s` for a whole second.
    #[test]
    fn the_seconds_remaining_round_up() {
        assert_eq!(seconds_left(Duration::from_millis(1)), 1);
        assert_eq!(seconds_left(Duration::from_millis(999)), 1);
        assert_eq!(seconds_left(Duration::from_millis(1_000)), 1);
        assert_eq!(seconds_left(Duration::from_millis(1_001)), 2);
        assert_eq!(seconds_left(Duration::from_secs(15)), 15);
        assert_eq!(seconds_left(Duration::ZERO), 0);
    }

    fn deadline_of(p: &Progress) -> Option<Instant> {
        *p.waiting_until.lock().unwrap()
    }

    /// The countdown is driven by a deadline rather than by text, which is the
    /// whole reason it moves. What matters is that the deadline is there while
    /// a wait is on and gone the moment anything else happens — a number
    /// ticking down beside "page 7, 350 accounts" would be worse than no
    /// number at all.
    #[test]
    fn a_wait_sets_a_deadline_and_anything_else_clears_it() {
        let p = Progress::new(false);
        assert_eq!(deadline_of(&p), None, "nothing is being waited for yet");

        p.waiting("pausing", Duration::from_secs(10));
        let set = deadline_of(&p).expect("a wait has to leave a deadline");
        assert!(set > Instant::now(), "the deadline is in the future");

        p.note("page 7, 350 accounts");
        assert_eq!(deadline_of(&p), None, "the note ended the wait");

        // And the event that really ends a pause: the next page arriving.
        p.waiting("pausing", Duration::from_secs(10));
        p.event(&Event::Page {
            number: 7,
            running_total: 350,
            added: 50,
            received: 50,
        });
        assert_eq!(deadline_of(&p), None);
    }

    /// Only the long pause is announced. The micro pause and the cycle wait
    /// land between every single page, and a countdown that appeared and
    /// vanished a second later, twice a page, would be harder to read than the
    /// spinner already saying the same thing.
    #[test]
    fn the_short_waits_do_not_take_over_the_line() {
        let p = Progress::new(false);
        for kind in [WaitKind::Micro, WaitKind::Cycle] {
            p.event(&Event::Waiting {
                kind,
                duration: Duration::from_secs(2),
            });
            assert_eq!(deadline_of(&p), None, "{kind:?} should stay quiet");
        }

        p.event(&Event::Waiting {
            kind: WaitKind::Long,
            duration: Duration::from_secs(12),
        });
        assert!(deadline_of(&p).is_some(), "the long pause is worth saying");
    }
}
