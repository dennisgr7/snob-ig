//! Which slice of a list too long for the terminal is on screen.
//!
//! Arithmetic, with no terminal in it, which is what makes it testable without
//! one. The browser had none of this: it printed every story it was given, so
//! twenty stories on a twenty-four-row terminal wrote twenty-four rows,
//! `clear_last_lines` cleared at most the terminal height, and the heading
//! walked off into the scrollback where nothing can clear it again. An account
//! with twenty stories up is an ordinary account.

/// Rows of context kept above and below the selection before the window
/// follows it.
///
/// Two, which is within one of what `less -j`, helix's `scrolloff` and lazygit
/// all settle on: enough to see where the next step lands without the window
/// lurching a whole page whenever the selection reaches an edge.
const SCROLLOFF: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Viewport {
    /// Index of the first item drawn.
    pub first: usize,
    /// How many items fit. Reset from the terminal on every frame rather than
    /// updated from a resize event -- see [`Viewport::follow`].
    pub height: usize,
}

impl Viewport {
    pub fn new(height: usize) -> Self {
        Self {
            first: 0,
            height: height.max(1),
        }
    }

    /// Moves the window the least it can so that `selected` is visible with
    /// [`SCROLLOFF`] rows of context, and keeps it inside the list.
    ///
    /// Called on every frame with the height recomputed from the terminal's
    /// *current* size, which is how a resize is handled here: nothing is ever
    /// laid out against a size that has gone, so a resize event that was
    /// coalesced, missed or delivered late cannot leave the window wrong. It is
    /// what `ratatui` does in `autoresize`, for the same reason.
    ///
    /// The order of the clamping is the part that is easy to get wrong. A
    /// window made taller than the tail that is left has to be pulled *back*,
    /// or the list draws blank rows under the last item while items above it
    /// are scrolled out of sight -- which is exactly what happens when somebody
    /// maximizes the window while sitting at the bottom of the list.
    pub fn follow(&mut self, selected: usize, total: usize) {
        let height = self.height.min(total.max(1));
        // A window shorter than twice the margin has no room for a margin.
        // Asking for one anyway makes it fight itself at both edges.
        let margin = if height > SCROLLOFF * 2 { SCROLLOFF } else { 0 };

        if selected < self.first + margin {
            self.first = selected.saturating_sub(margin);
        }
        let last_visible = self.first + height - 1;
        if selected + margin > last_visible {
            self.first = selected + margin + 1 - height;
        }
        self.first = self.first.min(total.saturating_sub(height));
    }

    /// The items to draw.
    pub fn range(&self, total: usize) -> std::ops::Range<usize> {
        let end = (self.first + self.height).min(total);
        self.first..end
    }

    pub fn more_above(&self) -> bool {
        self.first > 0
    }

    pub fn more_below(&self, total: usize) -> bool {
        self.first + self.height < total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn walk(total: usize, height: usize, path: &[usize]) -> Vec<usize> {
        let mut v = Viewport::new(height);
        path.iter()
            .map(|&s| {
                v.follow(s, total);
                v.first
            })
            .collect()
    }

    #[test]
    fn a_list_that_fits_never_scrolls() {
        assert_eq!(walk(5, 10, &[0, 1, 2, 3, 4]), vec![0, 0, 0, 0, 0]);
    }

    #[test]
    fn the_window_follows_downwards_with_a_margin() {
        // Ten items, five visible, margin two: rows 0 to 4 are on screen, so
        // selecting the fourth already has to move the window.
        assert_eq!(walk(10, 5, &[0, 1, 2, 3, 4]), vec![0, 0, 0, 1, 2]);
    }

    #[test]
    fn the_window_follows_upwards_with_a_margin() {
        let mut v = Viewport::new(5);
        v.follow(9, 10);
        assert_eq!(v.first, 5);
        v.follow(6, 10);
        assert_eq!(v.first, 4);
    }

    #[test]
    fn the_window_never_runs_off_the_end() {
        let mut v = Viewport::new(5);
        v.follow(9, 10);
        assert_eq!(v.range(10), 5..10);
        assert!(!v.more_below(10));
        assert!(v.more_above());
    }

    /// Somebody maximizes the window while sitting at the bottom of the list.
    #[test]
    fn growing_taller_than_the_tail_pulls_the_window_back() {
        let mut v = Viewport::new(5);
        v.follow(9, 10);
        assert_eq!(v.first, 5);
        v.height = 10;
        v.follow(9, 10);
        assert_eq!(v.first, 0);
    }

    #[test]
    fn a_window_too_short_for_a_margin_still_works() {
        let mut v = Viewport::new(1);
        v.follow(7, 10);
        assert_eq!(v.range(10), 7..8);
    }

    #[test]
    fn a_single_story_needs_no_window_at_all() {
        let mut v = Viewport::new(20);
        v.follow(0, 1);
        assert_eq!(v.range(1), 0..1);
        assert!(!v.more_above() && !v.more_below(1));
    }
}
