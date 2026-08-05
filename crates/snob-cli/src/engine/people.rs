//! Who you both know.
//!
//! When the account being looked at is not yours, the useful first line is not
//! a number — it is a name you recognize. Instagram itself leads with it
//! ("Followed by so-and-so and 4 others"), and the tool can work it out with no
//! request at all: the answer is the accounts you follow that also follow them.
//!
//! Strictly from storage, and strictly from a **complete** snapshot. A partial
//! list of your own following would leave people out of the overlap, and naming
//! two mutual acquaintances when there are nine is worse than naming none.

use anyhow::Result;
use snob_core::model::{ListKind, User};
use snob_core::sets;
use snob_core::store::snapshots;

use crate::app::App;

/// The accounts you follow that are among `followers`.
///
/// `None` means the question could not be answered — no stored list of your own
/// following — which reads differently from `Some(vec![])`, "nobody you follow
/// is in there". The caller has to keep them apart: one is silence, the other
/// is an answer.
pub fn in_common(app: &App, followers: &[User]) -> Result<Option<Vec<User>>> {
    let viewer = app.viewer();
    let Some(snapshot) =
        snapshots::latest_complete(app.db().conn(), viewer.pk, ListKind::Following)?
    else {
        return Ok(None);
    };

    let mine = snapshots::members(app.db().conn(), snapshot.id)?;
    // Ordered by the list of people you follow rather than by their followers:
    // the names are there to be recognized, and that is the list you know.
    Ok(Some(sets::intersection(&mine, followers)))
}

/// "pepito, carlos and 4 others", or `None` when there is nobody to name.
///
/// The cap is not about width. Past a handful the line stops being "people you
/// know" and becomes a list, and a list is what the `friends` command is for.
pub fn name_a_few(people: &[User], cap: usize) -> Option<String> {
    // A cap of zero would name nobody and count everybody, which is not a
    // sentence anyone wants to read. At least one name, always.
    let cap = cap.max(1);
    let (first, rest) = match people.len() {
        0 => return None,
        n if n <= cap => (people, 0),
        _ => (&people[..cap], people.len() - cap),
    };

    let names: Vec<String> = first.iter().map(|u| format!("@{}", u.username)).collect();
    let listed = match names.as_slice() {
        [one] => one.clone(),
        // The last one joins with "and" rather than a comma, because this is a
        // sentence rather than a column.
        [start @ .., last] => format!("{} and {last}", start.join(", ")),
        [] => unreachable!("the empty case returned above"),
    };

    Some(match rest {
        0 => listed,
        1 => format!("{listed} and 1 other"),
        n => format!("{listed} and {n} others"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn people(names: &[&str]) -> Vec<User> {
        names
            .iter()
            .enumerate()
            .map(|(i, name)| User {
                pk: i as u64 + 1,
                username: (*name).into(),
                full_name: None,
                is_private: None,
                is_verified: None,
                pfp_url: None,
            })
            .collect()
    }

    #[test]
    fn nobody_is_not_a_sentence() {
        assert_eq!(name_a_few(&[], 3), None);
    }

    #[test]
    fn one_name_stands_alone() {
        assert_eq!(name_a_few(&people(&["ana"]), 3).unwrap(), "@ana");
    }

    #[test]
    fn the_last_one_joins_with_and() {
        assert_eq!(
            name_a_few(&people(&["ana", "luis"]), 3).unwrap(),
            "@ana and @luis"
        );
        assert_eq!(
            name_a_few(&people(&["ana", "luis", "eva"]), 3).unwrap(),
            "@ana, @luis and @eva"
        );
    }

    #[test]
    fn past_the_cap_the_rest_are_counted() {
        let five = people(&["ana", "luis", "eva", "juan", "sara"]);
        assert_eq!(
            name_a_few(&five, 3).unwrap(),
            "@ana, @luis and @eva and 2 others"
        );
        assert_eq!(
            name_a_few(&five, 4).unwrap(),
            "@ana, @luis, @eva and @juan and 1 other"
        );
        // Exactly at the cap nothing is left over to count.
        assert_eq!(
            name_a_few(&five, 5).unwrap(),
            "@ana, @luis, @eva, @juan and @sara"
        );
    }
}
