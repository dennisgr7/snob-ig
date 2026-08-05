//! Set operations between two account lists.
//!
//! Comparison is **always by `pk`**, never by username: usernames change, and
//! crossing two lists by name would make anyone who renamed themselves between
//! captures look like they left and someone else arrived.

use std::collections::HashSet;

use crate::Pk;
use crate::model::User;

/// Those in `a` and not in `b`, keeping the order of `a`.
pub fn difference(a: &[User], b: &[User]) -> Vec<User> {
    let in_b: HashSet<Pk> = b.iter().map(|u| u.pk).collect();
    a.iter()
        .filter(|u| !in_b.contains(&u.pk))
        .cloned()
        .collect()
}

/// Those in both, keeping the order of `a`.
pub fn intersection(a: &[User], b: &[User]) -> Vec<User> {
    let in_b: HashSet<Pk> = b.iter().map(|u| u.pk).collect();
    a.iter().filter(|u| in_b.contains(&u.pk)).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn users(pks: &[Pk]) -> Vec<User> {
        pks.iter()
            .map(|&pk| User {
                pk,
                username: format!("u{pk}"),
                full_name: None,
                is_private: None,
                is_verified: None,
                pfp_url: None,
            })
            .collect()
    }

    fn pks(us: &[User]) -> Vec<Pk> {
        us.iter().map(|u| u.pk).collect()
    }

    #[test]
    fn difference_keeps_the_order_of_the_first_list() {
        let a = users(&[3, 1, 2, 4]);
        let b = users(&[2, 4]);
        assert_eq!(pks(&difference(&a, &b)), vec![3, 1]);
    }

    #[test]
    fn intersection_keeps_the_order_of_the_first_list() {
        let a = users(&[3, 1, 2, 4]);
        let b = users(&[4, 2]);
        assert_eq!(pks(&intersection(&a, &b)), vec![2, 4]);
    }

    #[test]
    fn an_empty_list_gives_the_expected_result() {
        let a = users(&[1, 2]);
        assert_eq!(pks(&difference(&a, &[])), vec![1, 2]);
        assert!(intersection(&a, &[]).is_empty());
        assert!(difference(&[], &a).is_empty());
    }

    /// Someone who renamed themselves between captures is still the same
    /// person: crossing by username would show them as a departure and an
    /// arrival at once.
    #[test]
    fn lists_are_crossed_by_id_and_not_by_username() {
        let before = users(&[7]);
        let mut after = users(&[7]);
        after[0].username = "renamed_themselves".into();

        assert!(difference(&before, &after).is_empty());
        assert_eq!(pks(&intersection(&before, &after)), vec![7]);
    }

    #[test]
    fn two_identical_lists_leave_no_difference() {
        let a = users(&[1, 2, 3]);
        assert!(difference(&a, &a).is_empty());
        assert_eq!(pks(&intersection(&a, &a)), vec![1, 2, 3]);
    }
}
