//! The bytes held, by 1 MB block, and which range the download fetches next,
//! so a long stream can keep only the blocks around the read position.

use super::{CHUNK, KEEP_AHEAD, KEEP_BEHIND};

#[derive(Default)]
pub(super) struct State {
    /// The stream by `CHUNK`, each block held from its start.
    pub(super) blocks: Vec<Vec<u8>>,
    pub(super) total: u64,
    /// Where the reader last read, which the download serves first.
    pub(super) pos: u64,
    /// Keeps only the blocks around `pos`, as the stream is too long to hold.
    pub(super) window: bool,
    pub(super) readers: usize,
    /// Every reader has gone, so a window has nothing left to follow.
    pub(super) abandoned: bool,
    /// Every byte is held.
    pub(super) done: bool,
    pub(super) cancelled: bool,
    pub(super) error: Option<String>,
    /// The error is the connection's, which outlasted `RIDE_OUT`.
    pub(super) dropped: bool,
}

impl State {
    pub(super) fn held(&self, block: usize) -> usize {
        self.blocks.get(block).map_or(0, Vec::len)
    }

    /// The end of the bytes held unbroken from `pos`, or `pos` if its byte is missing.
    pub(super) fn run_end(&self, pos: u64) -> u64 {
        let mut block = (pos / CHUNK) as usize;
        while self.held(block) == CHUNK as usize {
            block += 1;
        }
        (block as u64 * CHUNK + self.held(block) as u64).max(pos)
    }

    /// Where the next range starts: the read position's block and on from
    /// it, then any gap from the start. `None` once all a window may hold is.
    pub(super) fn next_range(&self) -> Option<u64> {
        if self.total == 0 {
            return Some(self.run_end(0));
        }
        let count = self.total.div_ceil(CHUNK) as usize;
        let from = ((self.pos / CHUNK) as usize).min(count - 1);
        let (ahead, behind) = if self.window {
            ((from + KEEP_AHEAD).min(count), 0)
        } else {
            (count, from)
        };
        let missing =
            |&b: &usize| (self.held(b) as u64) < (self.total - b as u64 * CHUNK).min(CHUNK);
        (from..ahead)
            .chain(0..behind)
            .find(missing)
            .map(|b| b as u64 * CHUNK + self.held(b) as u64)
    }

    /// Drops a window's blocks away from the read position, to be fetched again if read.
    pub(super) fn evict(&mut self) {
        for b in 0..self.blocks.len() {
            if !self.kept(b) {
                self.blocks[b] = Vec::new();
            }
        }
    }

    /// Whether block `b` is within the window around the read position.
    pub(super) fn kept(&self, b: usize) -> bool {
        let from = (self.pos / CHUNK) as usize;
        !self.window || (b + KEEP_BEHIND >= from && b < from + KEEP_AHEAD)
    }

    /// Adds `bytes` at `at`, which follows what its block holds.
    pub(super) fn put(&mut self, mut at: u64, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let b = (at / CHUNK) as usize;
            if self.blocks.len() <= b {
                self.blocks.resize_with(b + 1, Vec::new);
            }
            let size = match self.total {
                0 => CHUNK,
                total => total.saturating_sub(b as u64 * CHUNK).min(CHUNK),
            } as usize;
            let block = &mut self.blocks[b];
            debug_assert_eq!(at, b as u64 * CHUNK + block.len() as u64);
            // Sized once, as growing by doubling can hold twice the block.
            block.reserve_exact(size.saturating_sub(block.len()));
            let n = bytes.len().min(CHUNK as usize - block.len());
            block.extend_from_slice(&bytes[..n]);
            bytes = &bytes[n..];
            at += n as u64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::source::tests::{MB, fill};

    fn sized(total: u64) -> State {
        State {
            total,
            ..State::default()
        }
    }

    #[test]
    fn fetches_a_seek_first_then_fills_the_gaps() {
        let mut state = sized(5 * MB + 100);
        state.put(0, &[0; 64 * 1024]);
        assert_eq!(state.next_range(), Some(64 * 1024), "on from the head");
        state.pos = 3 * MB + 10;
        assert_eq!(state.next_range(), Some(3 * MB), "the seek's block first");
        state.put(3 * MB, &[3; 1000]);
        assert_eq!(
            state.next_range(),
            Some(3 * MB + 1000),
            "then the rest of it"
        );
        fill(&mut state, 3);
        fill(&mut state, 4);
        assert_eq!(state.next_range(), Some(5 * MB), "on to the end");
        fill(&mut state, 5);
        assert_eq!(state.next_range(), Some(64 * 1024), "then the gap before");
        assert_eq!(state.run_end(state.pos), state.total);
        assert_eq!(state.run_end(MB), MB, "nothing held there");
        for block in 0..3 {
            fill(&mut state, block);
        }
        assert_eq!(state.next_range(), None);
        assert_eq!(state.run_end(0), state.total, "all of it");
    }

    #[test]
    fn a_stream_of_unknown_length_runs_on_across_blocks() {
        let mut state = State::default();
        state.put(0, &vec![1; MB as usize + 5]);
        assert_eq!(state.blocks.len(), 2);
        assert_eq!(state.next_range(), Some(MB + 5));
    }

    #[test]
    fn a_window_holds_only_blocks_around_the_reader() {
        let mut state = State {
            window: true,
            ..sized(100 * MB)
        };
        let (at, half) = (30, KEEP_AHEAD / 2);
        for block in 0..(at + half) as u64 {
            fill(&mut state, block);
        }
        state.pos = at as u64 * MB;
        state.evict();
        let held: Vec<_> = (0..100).filter(|&b| state.held(b) > 0).collect();
        assert_eq!(held, (at - KEEP_BEHIND..at + half).collect::<Vec<_>>());
        assert_eq!(
            state.next_range(),
            Some((at + half) as u64 * MB),
            "ahead, not the gap behind"
        );
        for block in (at + half) as u64..(at + KEEP_AHEAD) as u64 {
            fill(&mut state, block);
        }
        assert_eq!(state.next_range(), None, "far enough ahead");
        state.pos = 5 * MB;
        state.evict();
        assert!(
            state.blocks.iter().skip(5 + KEEP_AHEAD).all(Vec::is_empty),
            "dropped ahead too"
        );
        assert_eq!(
            state.next_range(),
            Some(5 * MB),
            "a seek back fetches again"
        );
    }
}
