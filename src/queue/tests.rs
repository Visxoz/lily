use super::*;

const NAMES: [&str; 10] = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"];

impl Spread for u32 {
    fn artist(&self) -> &str {
        NAMES[*self as usize % 3]
    }

    fn album(&self) -> &str {
        ""
    }
}

/// An artist and an album, by index into `NAMES`.
impl Spread for (usize, usize) {
    fn artist(&self) -> &str {
        NAMES[self.0]
    }

    fn album(&self) -> &str {
        NAMES[self.1]
    }
}

fn neighbours(
    order: &[(usize, usize)],
    same: impl Fn(&(usize, usize), &(usize, usize)) -> bool,
) -> usize {
    order.windows(2).filter(|w| same(&w[0], &w[1])).count()
}

/// Ten artists of uneven sizes, none much more than a fifth, each across three albums.
fn library() -> Vec<(usize, usize)> {
    [60, 47, 41, 33, 29, 23, 19, 13, 11, 7]
        .into_iter()
        .enumerate()
        .flat_map(|(artist, n)| (0..n).map(move |i| (artist, i % 3)))
        .collect()
}

#[test]
fn shuffle_spreads_artists_and_albums() {
    let songs = library();
    let n = songs.len() as f64;
    let mut by_artist = [0usize; 10];
    songs.iter().for_each(|s| by_artist[s.0] += 1);
    // What a plain shuffle averages: (n - 1) times the chance two songs share an artist.
    let plain: f64 = (n - 1.0)
        * by_artist
            .iter()
            .map(|&k| (k as f64 / n).powi(2))
            .sum::<f64>();
    let (mut artists, mut albums) = (0, 0);
    for seed in 0..20 {
        let mut q = Queue::with_seed(seed);
        q.set(songs.clone(), 0, true);
        let order: Vec<_> = q.items().copied().collect();
        artists += neighbours(&order[1..], |a, b| a.0 == b.0);
        for artist in 0..10 {
            let own: Vec<_> = order.iter().copied().filter(|s| s.0 == artist).collect();
            albums += neighbours(&own, |a, b| a.1 == b.1);
        }
    }
    assert!(
        artists as f64 * 10.0 < plain * 20.0,
        "{artists} same-artist neighbours, where a plain shuffle averages {}",
        plain * 20.0
    );
    // A plain shuffle puts two of an artist's songs from one album together a third of the time.
    let plain_albums = (songs.len() - 10) as f64 / 3.0 * 20.0;
    assert!(
        albums as f64 * 5.0 < plain_albums,
        "{albums} same-album neighbours, where a plain shuffle averages {plain_albums}"
    );
}

#[test]
fn spreading_keeps_the_current_and_restores_the_order() {
    let songs = library();
    let mut q = Queue::with_seed(3);
    q.set(songs.clone(), 0, false);
    q.jump(40);
    q.set_shuffle(true);
    let order: Vec<_> = q.items().copied().collect();
    assert_eq!(&order[..41], &songs[..41], "what has played stays put");
    let mut sorted = order.clone();
    sorted.sort();
    let mut all = songs.clone();
    all.sort();
    assert_eq!(sorted, all, "nothing lost or doubled");
    q.advance(false);
    let playing = *q.current().unwrap();
    q.set_shuffle(false);
    assert_eq!(q.items().copied().collect::<Vec<_>>(), songs);
    assert_eq!(q.current(), Some(&playing));
}

#[test]
fn a_playlist_arriving_in_pages_is_spread_too() {
    let songs = library();
    let mut same = 0;
    for seed in 0..20 {
        let mut q = Queue::with_seed(seed);
        q.set(songs[..100].to_vec(), 0, true);
        for page in songs[100..].chunks(100) {
            q.extend(page.iter().copied());
        }
        let order: Vec<_> = q.items().copied().collect();
        same += neighbours(&order, |a, b| a.0 == b.0);
    }
    // A plain shuffle of these averages about 37 per queue.
    assert!(same < 20 * 12, "{same} same-artist neighbours in 20 queues");
}

#[test]
fn shuffle_is_the_same_for_the_same_seed() {
    let shuffled = || {
        let mut q = Queue::with_seed(7);
        q.set(library(), 3, true);
        q.extend((0..50).map(|i| (i % 10, 0)));
        q.items().copied().collect::<Vec<_>>()
    };
    assert_eq!(shuffled(), shuffled());
}

fn queue(n: u32) -> Queue<u32> {
    let mut q = Queue::with_seed(42);
    q.set((0..n).collect(), 0, false);
    q
}

fn items(q: &Queue<u32>) -> Vec<u32> {
    q.items().copied().collect()
}

#[test]
fn advances_and_stops_at_end() {
    let mut q = queue(3);
    assert_eq!(q.advance(false), Some(&1));
    assert_eq!(q.advance(false), Some(&2));
    assert_eq!(q.advance(false), None);
    assert_eq!(q.current(), Some(&2));
}

#[test]
fn repeat_modes() {
    let mut q = queue(2);
    q.repeat = Repeat::All;
    q.advance(false);
    assert_eq!(q.advance(false), Some(&0));

    q.repeat = Repeat::One;
    assert_eq!(q.advance(false), Some(&0));
    assert_eq!(q.advance(true), Some(&1), "skipping leaves repeat-one");
}

#[test]
fn back_wraps_only_with_repeat_all() {
    let mut q = queue(3);
    assert_eq!(q.back(), Some(&0));
    q.repeat = Repeat::All;
    assert_eq!(q.back(), Some(&2));
}

#[test]
fn replaces_in_place_through_shuffle() {
    let mut q = queue(10);
    q.jump(2);
    q.set_shuffle(true);
    let revision = q.revision();
    let at = q.items().position(|&i| i == 7).unwrap();
    assert_eq!(q.replace(|&i| i == 7, &70), [at]);
    assert_eq!(q.revision(), revision, "nothing moved");
    assert_eq!(q.current(), Some(&2));
    assert!(q.replace(|&i| i == 99, &0).is_empty(), "nothing to replace");

    q.set_shuffle(false);
    assert_eq!(items(&q), [0, 1, 2, 3, 4, 5, 6, 70, 8, 9]);
}

#[test]
fn shuffle_keeps_current_and_restores_order() {
    let mut q = queue(20);
    q.jump(5);
    q.set_shuffle(true);
    let shuffled = items(&q);
    assert_eq!(&shuffled[..6], &[0, 1, 2, 3, 4, 5]);
    assert_ne!(shuffled, (0..20).collect::<Vec<_>>());
    let mut sorted = shuffled.clone();
    sorted.sort();
    assert_eq!(sorted, (0..20).collect::<Vec<_>>());

    q.advance(false);
    let playing = *q.current().unwrap();
    q.set_shuffle(false);
    assert_eq!(items(&q), (0..20).collect::<Vec<_>>());
    assert_eq!(q.current(), Some(&playing));
}

#[test]
fn a_shuffled_start_part_way_through_plays_every_track() {
    let mut q = Queue::with_seed(42);
    q.set((0..20).collect(), 10, true);
    let mut heard = vec![*q.current().unwrap()];
    while let Some(&t) = q.advance(false) {
        heard.push(t);
    }
    assert_eq!(heard[0], 10);
    heard.sort();
    assert_eq!(heard, (0..20).collect::<Vec<_>>());
    q.set_shuffle(false);
    assert_eq!(items(&q), (0..20).collect::<Vec<_>>());
}

#[test]
fn tracks_added_while_shuffled_mix_in_and_go_last_unshuffled() {
    let mut q = queue(10);
    q.set_shuffle(true);
    q.advance(false);
    q.advance(false);
    let played = items(&q)[..3].to_vec();
    q.extend(10..20);
    let order = items(&q);
    assert_eq!(&order[..3], played, "what has played stays put");
    assert!(
        order[3..10].iter().any(|&t| t >= 10),
        "mixed in among those still to play"
    );
    let mut sorted = order.clone();
    sorted.sort();
    assert_eq!(sorted, (0..20).collect::<Vec<_>>());
    q.set_shuffle(false);
    assert_eq!(items(&q), (0..20).collect::<Vec<_>>());
}

#[test]
fn tracks_added_while_shuffled_go_behind_one_played_next() {
    let mut q = queue(10);
    q.set_shuffle(true);
    q.play_next(99);
    q.extend(10..400);
    assert_eq!(q.peek_next(), Some(&99));
}

#[test]
fn unshuffle_keeps_tracks_added_while_shuffled_behind_their_neighbour() {
    let mut q = queue(5);
    q.set_shuffle(true);
    q.push(99);
    let after = items(&q)[4];
    q.set_shuffle(false);
    let order = items(&q);
    assert_eq!(order.len(), 6);
    let at = order.iter().position(|&i| i == 99).unwrap();
    assert_eq!(
        order[at - 1],
        after,
        "still follows the track it was added after"
    );

    let mut q = queue(5);
    q.jump(2);
    q.set_shuffle(true);
    q.play_next(99);
    q.advance(false);
    assert_eq!(q.current(), Some(&99));
    q.set_shuffle(false);
    assert_eq!(items(&q), [0, 1, 2, 99, 3, 4]);
    assert_eq!(
        q.current(),
        Some(&99),
        "the playing track stays where it is"
    );
    assert_eq!(q.advance(false), Some(&3));
}

#[test]
fn recommendations_go_after_the_current_track_only() {
    let mut q = queue(4);
    q.jump(1);
    let revision = q.revision();
    q.insert_recommended(1, 50);
    q.insert_recommended(0, 50);
    assert_eq!(items(&q), [0, 1, 2, 3], "never at or before the current");
    assert_eq!(q.revision(), revision);
    q.insert_recommended(2, 50);
    q.insert_recommended(5, 51);
    assert_eq!(items(&q), [0, 1, 50, 2, 3, 51]);
    assert_eq!(q.current(), Some(&1));
    assert!(q.is_recommended(2) && q.is_recommended(5));
    assert!(!q.is_recommended(1));
    assert_eq!(q.recommended().collect::<Vec<_>>(), [2, 5]);
    assert_ne!(q.revision(), revision);
}

#[test]
fn removing_recommendations_restores_the_users_order() {
    let mut q = queue(6);
    q.jump(1);
    q.set_shuffle(true);
    let shuffled = items(&q);
    q.insert_recommended(2, 50);
    q.insert_recommended(5, 51);
    q.advance(false);
    q.advance(false);
    assert_eq!(q.current(), Some(&shuffled[2]));
    let revision = q.revision();
    assert!(q.remove_recommended());
    assert_eq!(items(&q), shuffled, "the user's shuffled order is back");
    assert_eq!(
        q.current(),
        Some(&shuffled[2]),
        "the index moved with the track"
    );
    assert_ne!(q.revision(), revision);
    assert!(!q.remove_recommended(), "nothing left to remove");

    q.set_shuffle(false);
    assert_eq!(items(&q), [0, 1, 2, 3, 4, 5]);
}

#[test]
fn a_playing_recommendation_is_kept() {
    let mut q = queue(3);
    q.insert_recommended(1, 50);
    q.insert_recommended(3, 51);
    q.advance(false);
    assert_eq!(q.current(), Some(&50));
    assert!(q.remove_recommended());
    assert_eq!(items(&q), [0, 50, 1, 2]);
    assert_eq!(q.current(), Some(&50));
    assert!(q.is_recommended(1), "still marked as one");
}

#[test]
fn moves_entries_and_the_current_index_follows() {
    let mut q = queue(6);
    q.jump(2);
    let revision = q.revision();
    assert!(q.move_to(4, 1), "a later entry to before the current");
    assert_eq!(items(&q), [0, 4, 1, 2, 3, 5]);
    assert_eq!(q.current(), Some(&2));
    assert_eq!(q.current_index(), Some(3));
    assert_ne!(q.revision(), revision);

    assert!(q.move_to(0, 5), "an earlier entry to after the current");
    assert_eq!(items(&q), [4, 1, 2, 3, 5, 0]);
    assert_eq!(q.current_index(), Some(2));

    assert!(q.move_to(2, 0), "the current track itself");
    assert_eq!(items(&q), [2, 4, 1, 3, 5, 0]);
    assert_eq!(q.current_index(), Some(0));
    assert_eq!(q.current(), Some(&2));

    let revision = q.revision();
    assert!(!q.move_to(1, 1));
    assert!(!q.move_to(1, 9));
    assert_eq!(q.revision(), revision, "nothing to move changes nothing");
}

#[test]
fn moving_while_shuffled_restores_the_original_order() {
    let mut q = queue(8);
    q.jump(1);
    q.set_shuffle(true);
    q.move_to(7, 2);
    q.move_to(3, 6);
    q.advance(false);
    let playing = *q.current().unwrap();
    q.set_shuffle(false);
    assert_eq!(items(&q), (0..8).collect::<Vec<_>>());
    assert_eq!(q.current(), Some(&playing));
}

#[test]
fn removes_many_but_never_the_current() {
    let mut q = queue(7);
    q.jump(3);
    assert_eq!(q.remove_many([5, 0, 3, 1, 99, 5]), [0, 1, 5]);
    assert_eq!(items(&q), [2, 3, 4, 6]);
    assert_eq!(q.current(), Some(&3));
    assert_eq!(q.current_index(), Some(1));
    let revision = q.revision();
    assert!(q.remove_many([1]).is_empty(), "only the current: nothing");
    assert_eq!(q.revision(), revision);
}

#[test]
fn plays_a_selection_next_in_queue_order() {
    let mut q = queue(8);
    q.jump(3);
    assert!(q.play_next_many([6, 1, 3, 7]));
    assert_eq!(items(&q), [0, 2, 3, 1, 6, 7, 4, 5]);
    assert_eq!(q.current(), Some(&3));
    assert_eq!(q.current_index(), Some(2));
    assert_eq!(q.peek_next(), Some(&1));

    let revision = q.revision();
    assert!(!q.play_next_many([3, 4, 5]), "already next, in that order");
    assert!(!q.play_next_many([2]), "the current track never moves");
    assert!(!q.play_next_many([]));
    assert_eq!(q.revision(), revision);
}

#[test]
fn plays_a_selection_next_through_shuffle() {
    let mut q = queue(10);
    q.jump(4);
    q.set_shuffle(true);
    let shuffled = items(&q);
    assert!(q.play_next_many([8, 9]));
    assert_eq!(q.peek_next(), Some(&shuffled[8]));
    q.set_shuffle(false);
    assert_eq!(items(&q), (0..10).collect::<Vec<_>>());
    assert_eq!(q.current(), Some(&4));
}

#[test]
fn marks_restored_recommendations() {
    let mut q = queue(4);
    q.mark_recommended([1, 3, 9]);
    assert_eq!(q.recommended().collect::<Vec<_>>(), [1, 3]);
    assert_eq!(
        q.entries()
            .filter(|(_, r)| *r)
            .map(|(i, _)| *i)
            .collect::<Vec<_>>(),
        [1, 3]
    );
}

#[test]
fn play_next_and_remove() {
    let mut q = queue(3);
    q.play_next(7);
    assert_eq!(items(&q), [0, 7, 1, 2]);
    q.jump(2);
    assert_eq!(q.remove_many([0]), [0]);
    assert_eq!(q.current(), Some(&1));
    assert!(
        q.remove_many([q.current_index().unwrap()]).is_empty(),
        "cannot remove the current track"
    );
}

#[test]
fn empty_queue_starts_with_first_push() {
    let mut q: Queue<u32> = Queue::with_seed(1);
    assert_eq!(q.advance(false), None);
    q.push(4);
    assert_eq!(q.current(), Some(&4));
}

#[test]
fn revision_tracks_changes_to_entries_only() {
    let mut q = queue(3);
    let r = q.revision();
    q.advance(false);
    q.jump(0);
    q.back();
    assert_eq!(q.revision(), r, "moving through the queue changes nothing");
    q.push(9);
    assert_ne!(q.revision(), r);
    let r = q.revision();
    q.set_shuffle(true);
    assert_ne!(q.revision(), r);
}

#[test]
fn a_song_rejected_at_the_end_skips_to_nothing_and_drops_its_radio() {
    let mut q = queue(2);
    q.jump(1);
    assert_eq!(q.peek_skip(), None, "the queue's end");
    q.repeat = Repeat::One;
    assert_eq!(q.peek_next(), Some(&1));
    assert_eq!(q.peek_skip(), Some(&0), "past a repeat of it");
    let mut lone = queue(1);
    lone.repeat = Repeat::All;
    assert_eq!(lone.peek_skip(), Some(&0), "itself, which a skip stops on");

    // Autoplay's radio, then a song the user queued and a pick among the radio.
    let first = lone.uid_mark();
    lone.extend([10, 11]);
    let radio = first..lone.uid_mark();
    lone.push(20);
    lone.insert_recommended(2, 30);
    assert_eq!(items(&lone), [0, 10, 30, 11, 20]);
    assert_eq!(lone.added(&radio), [1, 2, 3]);
    let playing = lone.current_uid().unwrap();
    lone.move_to(0, 4);
    assert_eq!(lone.find_uid(playing), Some(4), "followed as it moves");
}

#[test]
fn clears_upcoming() {
    let mut q = queue(5);
    q.jump(1);
    q.clear_upcoming();
    assert_eq!(items(&q), [0, 1]);
}
