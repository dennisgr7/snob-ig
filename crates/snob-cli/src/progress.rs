//! Turns walk events into a stack of progress bars.
//!
//! **Rule of this module: no data ever goes through the bars.** `println` on a
//! bar silently discards output when there is no terminal, so using it for
//! results would make them vanish when redirecting to a file. Data goes to
//! standard output and these bars to standard error, so the two never collide.
//!
//! **One bar per walk, and a finished walk keeps its line.** A crossing walks
//! two lists, and the first design drew both through one bar, resetting it in
//! between — so the moment the followers list finished, its bar vanished and a
//! new empty one appeared in its place. Someone glancing back at the terminal
//! saw a bar that had been nearly full sitting near zero, which reads as a
//! restart, not as progress. Now the finished walk freezes into a plain line —
//! the name, a full bar, the count and how long it took — and the next walk
//! draws below it. `finish()` still clears the whole stack: the summary lines
//! the commands print carry the same numbers, and the last thing on screen
//! should be the answer, not the scaffolding.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use indicatif::style::ProgressTracker;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressState, ProgressStyle};
use snob_core::model::StopReason;
use snob_ig::pager::{Event, WaitKind};

/// The moment the wait being drawn right now ends, if one is under way.
///
/// Shared with the style, which reads it on every redraw. That sharing is the
/// whole trick behind the countdown: the bar already ticks several times a
/// second to animate itself, and this gives it something new to say each time
/// rather than the same frozen number.
type Deadline = Arc<Mutex<Option<Instant>>>;

/// Cloning shares the same stack rather than making a second one: the pacer
/// announces its waits from inside the client and has to reach the bar the walk
/// is already drawing.
#[derive(Clone)]
pub struct Progress {
    /// The stack: finished walks above, the live bar at the bottom.
    stack: MultiProgress,
    /// The bar events land on. Swapped for a fresh one after a freeze, which
    /// is why it sits behind a lock rather than being a field.
    bar: Arc<Mutex<ProgressBar>>,
    /// Whether the current bar has been frozen as a finished walk. A frozen
    /// bar is done drawing; the next thing that needs a bar gets a new one.
    frozen: Arc<AtomicBool>,
    /// The frozen bars still on screen. Kept so [`Progress::finish`] can
    /// take them out of the stack — indicatif keeps a finished bar's line
    /// until it is removed, and a run that walks again later (the profile
    /// browser does) must not resurrect the previous walk's receipts.
    done: Arc<Mutex<Vec<ProgressBar>>>,
    quiet: bool,
    waiting_until: Deadline,
    /// Whether this terminal can draw block and braille characters. Worked out
    /// once: it cannot change while the process runs, and it is read on every
    /// style rebuild.
    rich: bool,
}

/// How often a bar redraws itself. It is also how often the countdown moves.
const TICK: Duration = Duration::from_millis(120);

impl Progress {
    pub fn new(enabled: bool) -> Self {
        let stack = if enabled {
            MultiProgress::with_draw_target(ProgressDrawTarget::stderr())
        } else {
            MultiProgress::with_draw_target(ProgressDrawTarget::hidden())
        };
        let waiting_until: Deadline = Arc::new(Mutex::new(None));
        let rich = rich_glyphs();

        // Asked from the stack, not taken from the flag. `indicatif` hides
        // itself when standard error is not a terminal, and a message set on
        // a bar that never draws is a message nobody reads — so
        // `snob unfollowers 2>log` used to swallow every wait announcement,
        // which is the one thing that explains a run standing still.
        let quiet = !enabled || stack.is_hidden();

        let progress = Self {
            bar: Arc::new(Mutex::new(ProgressBar::no_length())),
            stack,
            frozen: Arc::new(AtomicBool::new(false)),
            done: Arc::new(Mutex::new(Vec::new())),
            quiet,
            waiting_until,
            rich,
        };
        *progress.lock() = progress.fresh();
        progress
    }

    /// A stack that draws nowhere but still behaves as though it draws.
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

    fn lock(&self) -> std::sync::MutexGuard<'_, ProgressBar> {
        self.bar.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The bar events land on right now. `ProgressBar` is a handle over shared
    /// state, so the clone is the same bar.
    fn current(&self) -> ProgressBar {
        self.lock().clone()
    }

    /// A new bar at the bottom of the stack — added, but **not yet drawing**.
    ///
    /// The steady tick is what makes indicatif render, and it is deliberately
    /// not armed here: a `Progress` exists from `App::open` onward, and a
    /// command that walks nothing for a while — the interactive profile, a
    /// run waiting on a prompt — must not sit behind an idle spinner.
    /// [`Progress::animate`] arms it at the moments work really starts.
    fn fresh(&self) -> ProgressBar {
        let bar = self.stack.add(ProgressBar::no_length());
        bar.set_style(style(
            TEMPLATE_WITHOUT_TOTAL,
            &self.waiting_until,
            self.rich,
        ));
        bar
    }

    /// Arms the current bar's steady tick, which is what makes it draw.
    ///
    /// Called at every point work becomes visible — a walk named, a walk
    /// started, a wait announced — rather than once, because each fresh bar
    /// needs its own ticker: `indicatif` ends the ticker thread when a bar
    /// finishes and `reset` does not bring it back.
    fn animate(&self) {
        if !self.quiet {
            self.current().enable_steady_tick(TICK);
        }
    }

    /// Swaps a frozen bar for a fresh one, leaving the frozen line on screen.
    fn renew_if_frozen(&self) {
        if self.frozen.swap(false, Ordering::Relaxed) {
            *self.lock() = self.fresh();
        }
    }

    /// Says that nothing will happen for a while, and keeps saying how much
    /// longer.
    ///
    /// The number is deliberately **not** written into the message. It is read
    /// from the deadline by the style, on every redraw, so it counts down — the
    /// old behavior put the number in the text once and then left it there,
    /// which meant fifteen seconds of a bar insisting there were fifteen
    /// seconds left.
    ///
    /// The whole announcement is dimmed. A rest is the walk doing exactly what
    /// it should, and it used to be drawn with the same weight as the pages —
    /// so the line's loudest moments were the ones where nothing was wrong and
    /// nothing was happening. Muted, it reads as idle rather than stuck.
    ///
    /// With no bar to draw on, it is said once with the number in it, because
    /// there is nothing to animate and a line per second down a pipe would be
    /// noise.
    pub fn waiting(&self, reason: &str, duration: Duration) {
        self.set_deadline(Some(Instant::now() + duration));
        if self.quiet {
            eprintln!("{reason} ({} s)", seconds_left(duration));
        } else {
            self.renew_if_frozen();
            self.current().set_message(muted(reason));
            self.animate();
        }
    }

    fn set_deadline(&self, at: Option<Instant>) {
        *self.waiting_until.lock().unwrap_or_else(|e| e.into_inner()) = at;
    }

    /// Takes a walk event and reflects it.
    pub fn event(&self, e: &Event) {
        match e {
            Event::Started { estimated, resumed } => {
                // A finished walk's bar stays where it is; this walk gets its
                // own. `begin` has usually renewed already — it runs first and
                // names the walk — so this is for the caller that never named
                // one. The reset underneath is for the *first* bar, which may
                // carry a position from a wait announced before any walk.
                self.renew_if_frozen();
                self.animate();
                let bar = self.current();
                bar.reset();
                // The style's tracker has no reset of its own, so a pause left
                // over from before would keep counting down beside a fresh
                // walk.
                self.set_deadline(None);

                match estimated {
                    Some(total) => {
                        bar.set_length(*total);
                        bar.set_style(style(TEMPLATE_WITH_TOTAL, &self.waiting_until, self.rich));
                    }
                    // Not "leave it as it was": after a list that had a total,
                    // that would keep drawing `{pos}/{len}` against the
                    // previous list's length.
                    None => {
                        bar.unset_length();
                        bar.set_style(style(
                            TEMPLATE_WITHOUT_TOTAL,
                            &self.waiting_until,
                            self.rich,
                        ));
                    }
                }

                if *resumed {
                    self.warn("continuing an interrupted walk");
                }
            }
            Event::Page { running_total, .. } => {
                let running_total = *running_total as u64;
                let bar = self.current();
                // Instagram's counter sometimes undercounts. If we overshoot,
                // stretch the bar rather than leaving it past one hundred
                // percent.
                if bar.length().is_some_and(|l| running_total > l) {
                    bar.set_length(running_total);
                }
                bar.set_position(running_total);
                // The page number used to be written here, and it said nothing
                // a person could use: pages are how the API paginates, not how
                // anybody counts their followers. With a total on screen the
                // fraction already moves; without one, the running count is
                // the number that means something.
                bar.set_message(if bar.length().is_some() {
                    String::new()
                } else {
                    format!("{running_total} accounts")
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
                    self.waiting("resting to keep the pace down", *duration);
                }
            }
            Event::Retrying {
                attempt,
                after,
                error,
            } => {
                // The line scrolls above the bars because a retry is worth
                // keeping; the countdown runs on the bar itself, where it can
                // move.
                self.warn(&format!("retry {attempt} after a failure: {error}"));
                self.waiting(&format!("waiting before retry {attempt}"), *after);
            }
            // The pager says what it saw; `report` says it in English. The six
            // sentences that used to arrive already written came out of the
            // HTTP crate, which is the one place in the tool that has no
            // business deciding how anything reads.
            Event::Warning(warning) => self.warn(&crate::report::pager_warning(*warning)),
            Event::Finished { users, reason, .. } => {
                self.set_deadline(None);
                self.freeze(*users, *reason);
            }
        }
    }

    /// Ends the current bar as a plain line that stays on screen: the name, a
    /// bar, the count and how long the walk took.
    ///
    /// On a complete walk the length is set to what really arrived, so the bar
    /// draws full: the counter that estimated the total can lie in both
    /// directions, and a finished list behind a bar stuck at ninety per cent
    /// reads as a walk that gave up. An incomplete walk keeps its honest
    /// fraction — the gap is the news.
    fn freeze(&self, users: usize, reason: StopReason) {
        let bar = self.current();
        let complete = reason == StopReason::Completed;
        if complete {
            let n = users as u64;
            bar.set_length(n);
            bar.set_position(n);
        }
        bar.set_style(style(
            done_template(bar.length().is_some(), complete),
            &self.waiting_until,
            self.rich,
        ));
        bar.set_message(String::new());
        // `abandon`, not `finish`: `finish` helpfully moves the position to
        // the length, which would draw a rate-limited walk as if it had
        // completed. The fraction is the news; it stays where it stopped.
        bar.abandon();
        self.done
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(bar);
        self.frozen.store(true, Ordering::Relaxed);
    }

    /// Names what is being walked, for as long as it is being walked.
    ///
    /// The prefix rather than the message, because the message is transient: it
    /// is overwritten by every pause. Set before `engine::list`, so it is up
    /// during consent, resolution and the counter poll — the seconds where the
    /// bar otherwise says nothing at all.
    ///
    /// It is a label, not a claim: `engine::list` can answer out of storage
    /// without walking anything, in which case the name flashes and `finish`
    /// clears it. `snob scan someone` walks four lists in a row, and without
    /// this every one of them looked identical.
    ///
    /// A label that flashes costs nothing. A **line** that says it does is
    /// different: with no bar to draw on this printed `walking @someone
    /// followers` to standard error, and the two runs that have no bar are
    /// `--no-progress`, where the user asked for silence, and `--offline`, which
    /// walks nothing at all and answers from the database. Both were told about
    /// a walk that never happened, and `snob scan --offline` said it four times.
    /// So without a bar there is nothing to name.
    pub fn begin(&self, subject: &str) {
        if !self.quiet {
            self.renew_if_frozen();
            self.current().set_prefix(subject.to_string());
            self.animate();
        }
    }

    /// A message above the bars, without disturbing the drawing.
    pub fn warn(&self, text: &str) {
        if self.quiet {
            eprintln!("warning: {text}");
        } else {
            self.stack.suspend(|| eprintln!("warning: {text}"));
        }
    }

    /// Runs something that writes to the terminal itself, with the bars out of
    /// the way and put back afterwards.
    ///
    /// For a prompt. `ui::prompt_line` writes to standard error and so do the
    /// bars, so the consent question landed on the line the animation owns and
    /// was erased by the next tick — leaving somebody staring at a spinner,
    /// waiting for input they had not been asked for. `warn` has always gone
    /// through `suspend` for the same reason; the question needs it more,
    /// because it is what the run is waiting on.
    pub fn while_paused<T>(&self, f: impl FnOnce() -> T) -> T {
        if self.quiet {
            f()
        } else {
            self.stack.suspend(f)
        }
    }

    /// Ends the stack, frozen lines included, and leaves it ready for the
    /// next walk. Called by the commands when a run is over — and by the
    /// interactive profile after each suspended walk, which is why the bars
    /// are really *removed*: indicatif keeps a finished bar's line until
    /// then, and a later walk would redraw the previous walk's receipts
    /// above its own.
    pub fn finish(&self) {
        let bar = self.current();
        bar.finish_and_clear();
        self.stack.remove(&bar);
        for done in self
            .done
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            self.stack.remove(&done);
        }
        self.stack.clear().ok();
        // The removed bar cannot draw again; whoever needs one next gets a
        // fresh one, exactly as after a freeze.
        self.frozen.store(true, Ordering::Relaxed);
    }
}

/// Named rather than written inline, because the fallback below swallows a
/// broken one: a typo here would not fail, it would quietly draw indicatif's
/// default bar with no countdown in it and nobody would know why. A test
/// checks that all of these parse and that none has lost a key.
///
/// `{prefix}` is what the walk is walking and stays put; `{msg}` is what is
/// happening right now and is overwritten by every page and every pause. Both
/// live templates carry both, because a pause has to be sayable whichever one
/// is up — and the one with a total is the common case, since the counter poll
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

/// A finished walk, frozen in place while the next one draws below it.
///
/// No spinner: a finished line that still animates reads as work still going
/// on. Green, full (`freeze` set the length to what arrived), the count and
/// the time — the shape of a receipt rather than of an activity.
const TEMPLATE_DONE: &str = "{prefix} {bar:30.green/blue} {human_pos} accounts in {elapsed}";
/// A walk that stopped early, with the honest fraction kept. The warning lines
/// above the stack have already said why.
const TEMPLATE_DONE_STOPPED: &str =
    "{prefix} {bar:30.yellow/blue} {human_pos}/{human_len} in {elapsed}";
/// A stopped walk that never had a total: the count is all there is.
const TEMPLATE_DONE_NO_TOTAL: &str = "{prefix} {human_pos} accounts in {elapsed}";

/// Which frozen line a finished walk leaves behind.
///
/// A complete walk always has a length — `freeze` sets it from the real count
/// — so the no-total case only arises for a walk that stopped early.
fn done_template(has_total: bool, complete: bool) -> &'static str {
    if complete {
        TEMPLATE_DONE
    } else if has_total {
        TEMPLATE_DONE_STOPPED
    } else {
        TEMPLATE_DONE_NO_TOTAL
    }
}

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

/// Dimmed for standard error, where the bars draw. A wait announcement is the
/// walk behaving well, and it should not carry the same weight as the pages.
fn muted(text: &str) -> String {
    console::Style::new()
        .dim()
        .for_stderr()
        .apply_to(text)
        .to_string()
}

/// The style for any of the templates.
///
/// One function rather than one per template: the glyphs and the countdown key
/// are the same in all of them, and those are exactly the pair a test exists
/// to catch drifting apart. `progress_chars` is set unconditionally — indicatif
/// only consults it to draw a `{bar}`, so on a template that has none it is
/// inert.
fn style(template: &str, deadline: &Deadline, rich: bool) -> ProgressStyle {
    ProgressStyle::with_template(template)
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .tick_chars(tick_chars(rich))
        .progress_chars(progress_chars(rich))
        .with_key("countdown", countdown(deadline, rich))
}

/// Renders the time left in the current wait, or nothing at all.
///
/// Called by the bar on every redraw, which is what makes the number move. It
/// holds the lock for the length of a subtraction, several times a second, and
/// nothing else contends for it. Dimmed like the message it follows, so the
/// whole announcement reads as one quiet clause.
fn countdown(deadline: &Deadline, rich: bool) -> impl ProgressTracker + 'static {
    let deadline = Arc::clone(deadline);
    let separator = if rich { " · " } else { " - " };
    let quiet_ink = console::Style::new().dim().for_stderr();
    move |_: &ProgressState, w: &mut dyn std::fmt::Write| {
        let Some(end) = *deadline.lock().unwrap_or_else(|e| e.into_inner()) else {
            return;
        };
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        let _ = write!(
            w,
            "{}",
            quiet_ink.apply_to(format!("{separator}{} s", seconds_left(left)))
        );
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

    const EVERY_TEMPLATE: [&str; 5] = [
        TEMPLATE_WITHOUT_TOTAL,
        TEMPLATE_WITH_TOTAL,
        TEMPLATE_DONE,
        TEMPLATE_DONE_STOPPED,
        TEMPLATE_DONE_NO_TOTAL,
    ];

    /// A broken template does not fail, it falls back to indicatif's default
    /// bar — which has no `{countdown}` in it, so the countdown would simply
    /// never appear and nothing would say why.
    #[test]
    fn every_template_parses() {
        for template in EVERY_TEMPLATE {
            assert!(
                ProgressStyle::with_template(template).is_ok(),
                "{template} does not parse, so the bar would silently lose it"
            );
        }
    }

    /// Parsing is not enough: a template that lost a key still parses, and the
    /// thing it lost would simply stop appearing with no test noticing.
    #[test]
    fn the_live_templates_keep_every_key_they_need() {
        for template in [TEMPLATE_WITHOUT_TOTAL, TEMPLATE_WITH_TOTAL] {
            assert!(
                template.contains("{countdown}"),
                "{template} lost the pause countdown"
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
        for template in [TEMPLATE_DONE, TEMPLATE_DONE_STOPPED, TEMPLATE_DONE_NO_TOTAL] {
            assert!(
                template.contains("{prefix}"),
                "{template} lost the name of what was walked"
            );
            assert!(
                !template.contains("{spinner"),
                "{template} animates, and a finished line must not"
            );
        }
    }

    /// `progress_chars` panics at run time on entries of unequal width, and it
    /// would do it mid-walk. Both sets get built both ways here instead.
    #[test]
    fn every_glyph_set_builds() {
        let deadline: Deadline = Arc::new(Mutex::new(None));
        for rich in [true, false] {
            for template in EVERY_TEMPLATE {
                let _ = style(template, &deadline, rich);
            }
        }
    }

    fn page(number: u32, running_total: usize) -> Event {
        Event::Page {
            number,
            running_total,
            added: 50,
            received: 50,
        }
    }

    /// `snob scan someone` walks four lists in a row and every bar looked the
    /// same. The name goes in the prefix rather than the message because the
    /// message is overwritten by every pause.
    #[test]
    fn the_bar_says_what_it_is_walking_and_keeps_saying_it() {
        let p = Progress::drawing();
        p.begin("@someone followers");
        assert_eq!(p.current().prefix(), "@someone followers");

        // A page and a pause both replace the message, and neither touches it.
        p.event(&page(1, 50));
        p.waiting("resting", Duration::from_secs(5));
        assert_eq!(p.current().prefix(), "@someone followers");
    }

    /// The page number is not part of the display: pages are how the API
    /// paginates, not how anybody counts their followers. With a total the
    /// fraction is the progress; without one, the running count is.
    #[test]
    fn the_pages_are_counted_in_accounts_and_never_in_pages() {
        let p = Progress::new(false);

        p.event(&Event::Started {
            estimated: Some(400),
            resumed: false,
        });
        p.event(&page(7, 350));
        assert_eq!(p.current().message(), "");

        p.event(&Event::Finished {
            pages: 7,
            users: 350,
            reason: StopReason::Completed,
        });
        p.event(&Event::Started {
            estimated: None,
            resumed: false,
        });
        p.event(&page(7, 350));
        assert_eq!(p.current().message(), "350 accounts");
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

    fn finished(users: usize, reason: StopReason) -> Event {
        Event::Finished {
            pages: 1,
            users,
            reason,
        }
    }

    fn deadline_of(p: &Progress) -> Option<Instant> {
        *p.waiting_until.lock().unwrap()
    }

    /// The countdown is driven by a deadline rather than by text, which is the
    /// whole reason it moves. What matters is that the deadline is there while
    /// a wait is on and gone the moment anything else happens — a number
    /// ticking down beside a fresh page would be worse than no number at all.
    #[test]
    fn a_wait_sets_a_deadline_and_anything_else_clears_it() {
        let p = Progress::new(false);
        assert_eq!(deadline_of(&p), None, "nothing is being waited for yet");

        p.waiting("resting", Duration::from_secs(10));
        let set = deadline_of(&p).expect("a wait has to leave a deadline");
        assert!(set > Instant::now(), "the deadline is in the future");

        // The event that ends a pause: the next page arriving.
        p.event(&page(7, 350));
        assert_eq!(deadline_of(&p), None);
    }

    /// The user's complaint, verbatim: when the followers bar finished, the
    /// following bar replaced it — so a glance away and back showed a bar that
    /// had been nearly full sitting at zero, which reads as a restart. The
    /// finished walk keeps its own line now, and the next walk draws on a new
    /// one below it.
    #[test]
    fn a_finished_walk_keeps_its_line_and_the_next_walk_gets_its_own() {
        let p = Progress::drawing();
        p.begin("@someone followers");
        p.event(&Event::Started {
            estimated: Some(300),
            resumed: false,
        });
        p.event(&page(1, 300));
        let first = p.current();
        p.event(&finished(300, StopReason::Completed));
        assert!(first.is_finished(), "the finished walk's line froze");

        p.begin("@someone following");
        let second = p.current();
        assert_eq!(second.prefix(), "@someone following");
        assert!(
            !second.is_finished(),
            "the second walk needs a bar that draws"
        );
        assert_eq!(second.position(), 0, "the first list's position leaked");
        assert!(
            second.length().is_none(),
            "the first list's total leaked into a list that has none"
        );
        // And the first one is untouched by the rename.
        assert_eq!(first.prefix(), "@someone followers");
    }

    /// The same swap when nothing called `begin` in between: the walker's own
    /// `Started` is enough to get a fresh bar.
    #[test]
    fn a_second_walk_gets_a_fresh_bar_even_unnamed() {
        let p = Progress::new(false);
        p.event(&Event::Started {
            estimated: Some(300),
            resumed: false,
        });
        p.event(&page(1, 50));
        p.event(&finished(50, StopReason::Canceled));

        p.event(&Event::Started {
            estimated: None,
            resumed: false,
        });
        let bar = p.current();
        assert!(!bar.is_finished(), "the second list would draw nothing");
        assert_eq!(bar.position(), 0, "the first list's position leaked");
        assert!(
            bar.length().is_none(),
            "the first list's total leaked into a list that has none"
        );
    }

    /// A complete walk freezes full. The counter that estimated the total can
    /// lie in both directions, and a finished list behind a bar stuck at
    /// ninety per cent reads as a walk that gave up.
    #[test]
    fn a_complete_walk_freezes_full_and_a_stopped_one_keeps_its_fraction() {
        let p = Progress::drawing();
        p.event(&Event::Started {
            estimated: Some(400),
            resumed: false,
        });
        p.event(&page(7, 350));
        p.event(&finished(350, StopReason::Completed));
        let done = p.current();
        assert_eq!(done.length(), Some(350), "the estimate was the lie");
        assert_eq!(done.position(), 350);

        p.event(&Event::Started {
            estimated: Some(400),
            resumed: false,
        });
        p.event(&page(2, 100));
        p.event(&finished(100, StopReason::RateLimit));
        let stopped = p.current();
        assert_eq!(
            stopped.length(),
            Some(400),
            "an incomplete walk keeps its honest fraction"
        );
        assert_eq!(stopped.position(), 100);
    }

    /// A wait announced between two walks — the pacer speaks during the
    /// counter poll, before `Started` — must not land on the frozen line.
    #[test]
    fn a_wait_between_walks_draws_on_a_fresh_bar() {
        let p = Progress::drawing();
        p.event(&Event::Started {
            estimated: None,
            resumed: false,
        });
        p.event(&finished(10, StopReason::Completed));
        let frozen = p.current();

        p.waiting("the request budget is rationing", Duration::from_secs(3));
        assert!(
            !p.current().is_finished(),
            "the announcement needs a bar that draws"
        );
        assert!(
            frozen.message().is_empty(),
            "the frozen line stays a receipt"
        );
    }

    /// Which frozen line each ending leaves behind.
    #[test]
    fn the_frozen_line_matches_how_the_walk_ended() {
        assert_eq!(done_template(true, true), TEMPLATE_DONE);
        assert_eq!(done_template(false, true), TEMPLATE_DONE);
        assert_eq!(done_template(true, false), TEMPLATE_DONE_STOPPED);
        assert_eq!(done_template(false, false), TEMPLATE_DONE_NO_TOTAL);
    }

    /// `finish` used to be the end of the story; the interactive profile
    /// made it a comma — a walk, the card again, another walk. The stack has
    /// to come back from it: a fresh, unfinished bar, with nothing of the
    /// finished walk leaking in.
    #[test]
    fn the_stack_survives_a_finish_and_walks_again() {
        let p = Progress::drawing();
        p.begin("@someone followers");
        p.event(&Event::Started {
            estimated: Some(300),
            resumed: false,
        });
        p.event(&page(1, 300));
        p.event(&finished(300, StopReason::Completed));
        p.finish();

        p.begin("@someone following");
        let bar = p.current();
        assert!(!bar.is_finished(), "the next walk needs a bar that draws");
        assert_eq!(bar.prefix(), "@someone following");
        assert_eq!(bar.position(), 0, "the finished walk's position leaked");
        assert!(
            p.done.lock().unwrap().is_empty(),
            "finish left receipts behind to redraw over the next walk"
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
