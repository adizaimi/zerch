//! The on-screen row sequence: merge matching lines, bookmarks, revealed ranges,
//! and ±5 context around expanded lines into one sorted, de-duped list, inserting
//! a "gap" wherever consecutive displayed lines are more than one apart. Ported
//! from `src/client/lib/displayedItems.ts`; tests mirror `displayedItems.test.ts`.

use std::collections::BTreeSet;

/// A row in the virtual list: a real line, or a collapsed gap (which renders the
/// expand controls) spanning `start..=end` hidden lines.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DisplayedItem {
    Line(usize),
    Gap { start: usize, end: usize },
}

/// Build the displayed sequence. `total_lines > 0` enables boundary gaps before
/// the first / after the last displayed line so edge expansion stays reachable;
/// `0` means "unknown" (no boundary gaps, upper context bound unclamped).
pub fn build_displayed_items(
    matching: &[usize],
    bookmarked: &[usize],
    expanded: &[usize],
    visible: &[usize],
    hidden: &[usize],
    total_lines: usize,
) -> Vec<DisplayedItem> {
    let mut set: BTreeSet<usize> = BTreeSet::new();
    for &l in matching.iter().chain(bookmarked).chain(visible) {
        set.insert(l);
    }
    for &n in expanded {
        let lo = n.saturating_sub(5).max(1);
        let hi = if total_lines > 0 {
            (n + 5).min(total_lines)
        } else {
            n + 5
        };
        for i in lo..=hi {
            set.insert(i);
        }
    }
    // Explicitly hidden lines are removed last, so a hide wins over a match,
    // bookmark, reveal, or expand-context — and collapses into a gap.
    for &h in hidden {
        set.remove(&h);
    }

    let sorted: Vec<usize> = set.into_iter().collect();
    let mut items: Vec<DisplayedItem> = Vec::new();
    for (idx, &cur) in sorted.iter().enumerate() {
        if idx > 0 {
            let prev = sorted[idx - 1];
            if cur - prev > 1 {
                items.push(DisplayedItem::Gap {
                    start: prev + 1,
                    end: cur - 1,
                });
            }
        }
        items.push(DisplayedItem::Line(cur));
    }

    if total_lines > 0 && !sorted.is_empty() {
        let first = sorted[0];
        let last = *sorted.last().unwrap();
        if first > 1 {
            items.insert(0, DisplayedItem::Gap { start: 1, end: first - 1 });
        }
        if last < total_lines {
            items.push(DisplayedItem::Gap { start: last + 1, end: total_lines });
        }
    }

    items
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(items: &[DisplayedItem]) -> Vec<usize> {
        items
            .iter()
            .filter_map(|i| match i {
                DisplayedItem::Line(n) => Some(*n),
                _ => None,
            })
            .collect()
    }
    fn gaps(items: &[DisplayedItem]) -> Vec<(usize, usize)> {
        items
            .iter()
            .filter_map(|i| match i {
                DisplayedItem::Gap { start, end } => Some((*start, *end)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn no_gaps_for_consecutive() {
        let items = build_displayed_items(&[1, 2, 3, 4], &[], &[], &[], &[], 0);
        assert_eq!(lines(&items), vec![1, 2, 3, 4]);
        assert!(gaps(&items).is_empty());
    }

    #[test]
    fn inserts_gap_between_non_consecutive() {
        let items = build_displayed_items(&[1, 10], &[], &[], &[], &[], 0);
        assert_eq!(
            items,
            vec![
                DisplayedItem::Line(1),
                DisplayedItem::Gap { start: 2, end: 9 },
                DisplayedItem::Line(10),
            ]
        );
    }

    #[test]
    fn merges_dedupes_sorts_matching_and_bookmarks() {
        let items = build_displayed_items(&[5, 3, 5], &[3, 7], &[], &[], &[], 0);
        assert_eq!(lines(&items), vec![3, 5, 7]);
        assert_eq!(gaps(&items), vec![(4, 4), (6, 6)]);
    }

    #[test]
    fn context_around_expanded_clamped_low() {
        let items = build_displayed_items(&[], &[], &[3], &[], &[], 0);
        assert_eq!(lines(&items), vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(gaps(&items).is_empty());
    }

    #[test]
    fn context_clamped_to_total() {
        let items = build_displayed_items(&[], &[], &[10], &[], &[], 12);
        assert_eq!(lines(&items), vec![5, 6, 7, 8, 9, 10, 11, 12]);
    }

    #[test]
    fn context_unclamped_when_total_unknown() {
        let items = build_displayed_items(&[], &[], &[10], &[], &[], 0);
        assert_eq!(lines(&items), vec![5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
    }

    #[test]
    fn visible_ranges_merge_like_bookmarks() {
        let items = build_displayed_items(&[3], &[], &[], &[8, 3], &[], 0);
        assert_eq!(lines(&items), vec![3, 8]);
        assert_eq!(gaps(&items), vec![(4, 7)]);
    }

    #[test]
    fn boundary_gaps_top_and_bottom() {
        let items = build_displayed_items(&[5], &[], &[], &[], &[], 1000);
        assert_eq!(
            items,
            vec![
                DisplayedItem::Gap { start: 1, end: 4 },
                DisplayedItem::Line(5),
                DisplayedItem::Gap { start: 6, end: 1000 },
            ]
        );
    }

    #[test]
    fn no_boundary_gaps_when_spanning_file() {
        let items = build_displayed_items(&[1, 2, 3], &[], &[], &[], &[], 3);
        assert!(gaps(&items).is_empty());
    }

    #[test]
    fn hidden_lines_become_gaps_and_win_over_bookmarks() {
        // 1..=5 shown, hide 3 → gap at 3; hide wins even when 3 is bookmarked.
        let items = build_displayed_items(&[1, 2, 3, 4, 5], &[3], &[], &[], &[3], 0);
        assert_eq!(lines(&items), vec![1, 2, 4, 5]);
        assert_eq!(gaps(&items), vec![(3, 3)]);
    }

    #[test]
    fn empty_when_nothing_displayed() {
        assert_eq!(build_displayed_items(&[], &[], &[], &[], &[], 0), vec![]);
        assert_eq!(build_displayed_items(&[], &[], &[], &[], &[], 1000), vec![]);
    }
}
