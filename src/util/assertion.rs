use itertools::Itertools;
use jiff::Timestamp;
use std::{collections::HashMap, hash::Hash};

use crate::dto::request::Direction;

pub fn eq<T>(result: &[T], expected: &[T]) -> bool
where
    T: Eq + Hash,
{
    fn count<T>(items: &[T]) -> HashMap<&T, usize>
    where
        T: Eq + Hash,
    {
        let mut cnt = HashMap::new();
        for i in items {
            *cnt.entry(i).or_insert(0) += 1
        }
        cnt
    }
    count(result) == count(expected)
}

pub fn vecs_match<T: PartialEq>(a: &[T], b: &[T]) -> bool {
    a.len() == b.len() && !a.iter().zip(b.iter()).any(|(a, b)| *a != *b)
}

pub fn compare_datetime(left: &Timestamp, right: &Timestamp) -> bool {
    left.strftime("%d/%m/%Y %H:%M").to_string() == right.strftime("%d/%m/%Y %H:%M").to_string()
}

pub fn exist<T>(haystack: &[T], needle: &T) -> bool
where
    T: PartialEq,
{
    haystack.iter().any(|i| i == needle)
}

pub fn exist_all<T>(haystack: &[T], handful: &[T]) -> bool
where
    T: PartialEq,
{
    handful.iter().all(|i| haystack.contains(i))
}

pub fn is_sorted<I>(items: I, direction: Direction) -> bool
where
    I: IntoIterator,
    I::Item: Ord + Clone,
{
    items
        .into_iter()
        .tuple_windows()
        .all(direction.as_closure())
}

#[test]
fn test_exist_assertion() {
    let h = vec![1, 2, 3];
    let n = 2;
    assert!(exist(&h, &n))
}

#[test]
fn test_not_exist_assertion() {
    let h = vec![1, 2, 3];
    let n = 20;
    assert!(!exist(&h, &n))
}

#[test]
fn exist_all_test() {
    let h = vec![1, 2, 3, 4, 5, 6];
    let n = vec![1, 2, 6];
    assert!(exist_all(&h, &n))
}

#[test]
fn test_not_exist_all_assertion() {
    let h = vec![1, 2, 3];
    let n = vec![1, 2, 60];
    assert!(!exist_all(&h, &n))
}

#[test]
fn test_is_sort_assertion() {
    let a = vec![1, 20, 3];
    let b = vec![1, 2, 60];
    let c = vec![100, 20, 6];
    let d = vec![100, 20, 60];
    assert!(!is_sorted(a, Direction::ASC));
    assert!(is_sorted(b, Direction::ASC));
    assert!(is_sorted(c, Direction::DESC));
    assert!(!is_sorted(d, Direction::DESC))
}

#[test]
fn eq_is_multiset_equality_ignoring_order() {
    assert!(eq(&[1, 2, 2, 3], &[3, 2, 1, 2]));
    // Same set of values but different multiplicities — not equal.
    assert!(!eq(&[1, 2, 2], &[1, 1, 2]));
    assert!(!eq(&[1, 2, 3], &[1, 2]));
    assert!(eq::<i32>(&[], &[]));
}

#[test]
fn vecs_match_is_order_sensitive() {
    assert!(vecs_match(&[1, 2, 3], &[1, 2, 3]));
    assert!(!vecs_match(&[1, 2, 3], &[3, 2, 1]));
    assert!(!vecs_match(&[1, 2], &[1, 2, 3]));
}

#[test]
fn compare_datetime_matches_to_the_minute() {
    let base = Timestamp::from_second(1_700_000_000).unwrap();
    // 30 seconds apart — same minute, considered equal.
    let same_minute = Timestamp::from_second(1_700_000_030).unwrap();
    // 120 seconds apart — different minute.
    let next_minute = Timestamp::from_second(1_700_000_120).unwrap();
    assert!(compare_datetime(&base, &same_minute));
    assert!(!compare_datetime(&base, &next_minute));
}
