//! Turns walk events into a progress bar.
//!
//! **Rule of this module: no data ever goes through `pb.println()`.** That
//! method silently discards output when there is no terminal, so using it for
//! results would make them vanish when redirecting to a file. Data goes to
//! standard output and this bar to standard error, so the two never collide.

use std::sync::atomic::{AtomicBool, Ordering};
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
    /// Whether the steady tick is currently armed. Shared, like the bar.
    ticking: Arc<AtomicBool>,
    /// Whether this terminal can draw block and braille characters. Worked out
    /// once: it cannot change while the process runs, and it is read on every
    /// style rebuild.
    rich: bool,
}

/// How often the bar redraws itself. It is also how often the countdown moves.
const TICK: Duration = Duration::from_millis(120);

impl Progress {
    pub fn new(enabled: bool) -> Self {
        let bar = if enabled {
            ProgressBar::with_draw_target(None, ProgressDrawTarget::stderr())
        } else {
            ProgressBar::hidden()
        };
        let waiting_until: Deadline = Arc::new(Mutex::new(None));
        let rich = rich_glyphs();
        bar.set_style(style_without_total(&waiting_until, rich));

        let progress = Self {
            // Asked from the bar, not taken from the flag. `indicatif` hides
            // itself when standard error is not a terminal, and a message set
            // on a bar that never draws is a message nobody reads — so
            // `snob unfollowers 2>log` used to swallow every wait
            // announcement, which is the one thing that explains a run
            // standing still.
            quiet: !enabled || bar.is_hidden(),
            bar,
            waiting_until,
            ticking: Arc::new(AtomicBool::new(false)),
            rich,
        };
        progress.animate();
        progress
    }

    /// A bar that draws nowhere but still behaves as though it draws.
    ///
    /// **Tests only.** `Progress::new(true)` would decide `quiet` from whatever
    /// stderr the harness happens to have — hidden on CI, a real terminal when
    /// somebody runs `cargo test` in one — so a test built on it asserts a
    /// different thing depending on where it runs. This fixes the drawing path
    /// without a terminal: `indicatif` tracks prefix, message and position on a
    /// hidden bar just the same.
    #[cfg(test)]
    fn drawing() -> Self {
        let mut progress = Self::new(false);
        progress.quiet = false;
        progress
    }

    /// Arms the steady tick, if it is not already armed.
    ///
    /// It has to be re-armed rather than set once. `indicatif` ends the ticker
    /// thread when the bar is finished and `reset` does not bring it back, and
    /// a crossing finishes one walk before starting the next.
    fn animate(&self) {
        if !self.quiet && !self.ticking.swap(true, Ordering::Relaxed) {
            self.bar.enable_steady_tick(TICK);
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
        self.animate();
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
                // A crossing walks two lists through one shared bar, and the
                // walker emits `Finished` at the end of each. Clearing the bar
                // there left indicatif in `DoneHidden`, where it never draws
                // again and its ticker thread has already exited — so the
                // second and usually slower half of every `unfollowers`,
                // `fans`, `friends` and `scan` ran against a blank terminal.
                // `reset` is the way back, and it also zeroes a position left
                // sitting at the previous list's total.
                self.bar.reset();
                self.animate();
                // The style's tracker has no reset of its own, so a pause left
                // over from the previous list would keep counting down beside
                // the new one.
                self.set_deadline(None);

                match estimated {
                    Some(total) => {
                        self.bar.set_length(*total);
                        self.bar
                            .set_style(style_with_total(&self.waiting_until, self.rich));
                    }
                    // Not "leave it as it was": after a list that had a total,
                    // that would keep drawing `{pos}/{len}` against the
                    // previous list's length.
                    None => {
                        self.bar.unset_length();
                        self.bar
                            .set_style(style_without_total(&self.waiting_until, self.rich));
                    }
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
                // With a total on screen the running count is already the left
                // half of the fraction, and saying it again put the same number
                // twice on one line.
                self.bar.set_message(if self.bar.length().is_some() {
                    format!("page {number}")
                } else {
                    format!("page {number}, {running_total} accounts")
                });
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
            // Deliberately not `finish_and_clear`. One walk ending is not the
            // run ending, and the commands already clear the bar themselves
            // when it is — see `Started` for what finishing here cost.
            Event::Finished { .. } => {
                self.set_deadline(None);
                self.ticking.store(false, Ordering::Relaxed);
            }
        }
    }

    /// Names what is being walked, for as long as it is being walked.
    ///
    /// The prefix rather than the message, because the message is transient: it
    /// is overwritten by the next page and by every pause. Set before
    /// `engine::list`, so it is up during consent, resolution and the counter
    /// poll — the seconds where the bar otherwise says nothing at all.
    ///
    /// It is a label, not a claim: `engine::list` can answer out of storage
    /// without walking anything, in which case the name flashes and `finish`
    /// clears it. `snob scan someone` walks four lists in a row, and without
    /// this every one of them looked identical.
    ///
    /// A label that flashes costs nothing. A **line** that says it does is
    /// different: with no bar to draw on this printed `walking @someone
    /// followers` to standard error, and the two runs that have no bar are
    /// `--no-progress`, where the user asked for silence, and `--cache`, which
    /// walks nothing at all and answers from the database. Both were told about
    /// a walk that never happened, and `snob scan --cache` said it four times.
    /// So without a bar there is nothing to name.
    pub fn begin(&self, subject: &str) {
        self.animate();
        if !self.quiet {
            self.bar.set_prefix(subject.to_string());
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

    /// Runs something that writes to the terminal itself, with the bar out of
    /// the way and put back afterwards.
    ///
    /// For a prompt. `ui::prompt_line` writes to standard error and so does the
    /// bar, so the consent question landed on the line the animation owns and
    /// was erased by the next tick — leaving somebody staring at a spinner,
    /// waiting for input they had not been asked for. `warn` has always gone
    /// through `suspend` for the same reason; the question needs it more,
    /// because it is what the run is waiting on.
    pub fn while_paused<T>(&self, f: impl FnOnce() -> T) -> T {
        if self.quiet {
            f()
        } else {
            self.bar.suspend(f)
        }
    }

    /// Ends the bar for good. Called by the commands, which are the only ones
    /// that know the run is over.
    pub fn finish(&self) {
        self.ticking.store(false, Ordering::Relaxed);
        self.bar.finish_and_clear();
    }
}

/// Named rather than written inline, because the fallback below swallows a
/// broken one: a typo here would not fail, it would quietly draw indicatif's
/// default bar with no countdown in it and nobody would know why. A test
/// checks that both of these parse and that neither has lost a key.
///
/// `{prefix}` is what the run is walking and stays put; `{msg}` is what is
/// happening right now and is overwritten by every page and every pause. Both
/// templates carry both, because a pause has to be sayable whichever one is up
/// — and the one with a total is the common case, since the counter poll
/// usually supplies a length.
const TEMPLATE_WITHOUT_TOTAL: &str = "{spinner:.cyan} {prefix} {msg}{countdown}";
/// No percentage and no ETA: the total comes from a counter that can lie, and a
/// bar going past one hundred percent looks worse than no bar at all.
///
/// `{bar:30}` rather than `{wide_bar}`: the prefix is a username, so it changes
/// width between the two lists of a crossing, and a bar measured against the
/// remaining space would change width with it.
const TEMPLATE_WITH_TOTAL: &str =
    "{prefix} {bar:30.cyan/blue} {human_pos}/{human_len} {msg}{countdown}";

/// Whether stderr can be trusted with block and braille characters.
///
/// A deny-list, not an allow-list, and that is the whole design. `console`'s
/// own `wants_emoji` answers this on Windows with `WT_SESSION.is_ok()`, which
/// is false in VS Code's terminal, in Git Bash and in WezTerm — all three of
/// which draw braille perfectly well. The terminals that genuinely cannot are
/// countable: the Linux kernel console, which sets a UTF-8 locale and then
/// draws from a 512-glyph font, `dumb`, and anything that is not a terminal.
fn rich_glyphs() -> bool {
    if !console::Term::stderr().features().is_attended() {
        return false;
    }
    !matches!(std::env::var("TERM").as_deref(), Ok("linux" | "dumb"))
}

/// The spinner frames. The last one is the "finished" frame and is not part of
/// the cycle, which is why both lists end in a space.
fn tick_chars(rich: bool) -> &'static str {
    if rich {
        "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ "
    } else {
        r"|/-\ "
    }
}

/// Filled, partial, empty. Every entry has to be one column wide or indicatif
/// panics mid-walk, which is why the test builds both styles both ways.
fn progress_chars(rich: bool) -> &'static str {
    if rich {
        "█▉▊▋▌▍▎▏ "
    } else {
        "=> "
    }
}

fn style_without_total(deadline: &Deadline, rich: bool) -> ProgressStyle {
    ProgressStyle::with_template(TEMPLATE_WITHOUT_TOTAL)
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .tick_chars(tick_chars(rich))
        .with_key("countdown", countdown(deadline, rich))
}

fn style_with_total(deadline: &Deadline, rich: bool) -> ProgressStyle {
    ProgressStyle::with_template(TEMPLATE_WITH_TOTAL)
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .tick_chars(tick_chars(rich))
        .progress_chars(progress_chars(rich))
        .with_key("countdown", countdown(deadline, rich))
}

/// Renders the time left in the current wait, or nothing at all.
///
/// Called by the bar on every redraw, which is what makes the number move. It
/// holds the lock for the length of a subtraction, several times a second, and
/// nothing else contends for it.
fn countdown(deadline: &Deadline, rich: bool) -> impl ProgressTracker + 'static {
    let deadline = Arc::clone(deadline);
    let dash = if rich { " — " } else { " - " };
    move |_: &ProgressState, w: &mut dyn std::fmt::Write| {
        let Some(end) = *deadline.lock().unwrap_or_else(|e| e.into_inner()) else {
            return;
        };
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        let _ = write!(w, "{dash}{} s", seconds_left(left));
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

    /// Parsing is not enough: a template that lost a key still parses, and the
    /// thing it lost would simply stop appearing with no test noticing.
    #[test]
    fn both_templates_keep_every_key_they_need() {
        for template in [TEMPLATE_WITHOUT_TOTAL, TEMPLATE_WITH_TOTAL] {
            assert!(
                template.contains("{countdown}"),
                "{template} lost the countdown"
            );
            assert!(
                template.contains("{msg}"),
                "{template} lost the pause announcement"
            );
            assert!(
                template.contains("{prefix}"),
                "{template} lost the name of what is being walked"
            );
        }
    }

    /// `progress_chars` panics at run time on entries of unequal width, and it
    /// would do it mid-walk. Both sets get built both ways here instead.
    #[test]
    fn every_glyph_set_builds() {
        let deadline: Deadline = Arc::new(Mutex::new(None));
        for rich in [true, false] {
            let _ = style_without_total(&deadline, rich);
            let _ = style_with_total(&deadline, rich);
        }
    }

    /// `snob scan someone` walks four lists in a row and every bar looked the
    /// same. The name goes in the prefix rather than the message because the
    /// message is overwritten by the next page and by every pause.
    #[test]
    fn the_bar_says_what_it_is_walking_and_keeps_saying_it() {
        let p = Progress::drawing();
        p.begin("@someone followers");
        assert_eq!(p.bar.prefix(), "@someone followers");

        // A page and a pause both replace the message, and neither touches it.
        p.event(&Event::Page {
            number: 1,
            running_total: 50,
            added: 50,
            received: 50,
        });
        p.waiting("pausing", Duration::from_secs(5));
        assert_eq!(p.bar.prefix(), "@someone followers");

        // The second list of a crossing renames it.
        p.begin("@someone following");
        assert_eq!(p.bar.prefix(), "@someone following");
    }

    /// With a total on screen the fraction already says the running count, so
    /// saying it again put the same number twice on one line.
    #[test]
    fn the_running_count_is_not_printed_beside_the_fraction() {
        let p = Progress::new(false);
        let page = Event::Page {
            number: 7,
            running_total: 350,
            added: 50,
            received: 50,
        };

        p.event(&Event::Started {
            estimated: Some(400),
            resumed: false,
        });
        p.event(&page);
        assert_eq!(p.bar.message(), "page 7");

        // With no total there is no fraction, so the count is the only place
        // the number appears.
        p.event(&Event::Started {
            estimated: None,
            resumed: false,
        });
        p.event(&page);
        assert_eq!(p.bar.message(), "page 7, 350 accounts");
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

    fn finished() -> Event {
        Event::Finished {
            pages: 1,
            users: 50,
            reason: snob_core::model::StopReason::Completed,
        }
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

        // The event that ends a pause: the next page arriving.
        p.event(&Event::Page {
            number: 7,
            running_total: 350,
            added: 50,
            received: 50,
        });
        assert_eq!(deadline_of(&p), None);
    }

    /// A crossing walks two lists through one shared bar. The first walk's
    /// `Finished` used to clear it, which left indicatif in `DoneHidden` —
    /// where it never draws again and its ticker has already exited — so the
    /// second and usually slower half of `unfollowers`, `fans`, `friends` and
    /// `scan` ran against a blank terminal.
    #[test]
    fn a_second_walk_gets_the_bar_back() {
        let p = Progress::new(false);
        p.event(&Event::Started {
            estimated: Some(300),
            resumed: false,
        });
        p.event(&Event::Page {
            number: 1,
            running_total: 50,
            added: 50,
            received: 50,
        });
        p.event(&finished());

        p.event(&Event::Started {
            estimated: None,
            resumed: false,
        });
        assert!(!p.bar.is_finished(), "the second list would draw nothing");
        assert_eq!(p.bar.position(), 0, "the first list's position leaked");
        assert!(
            p.bar.length().is_none(),
            "the first list's total leaked into a list that has none"
        );
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
