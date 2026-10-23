//! Selection shared by JSON, rendered transcripts and relevance excerpts. Payloads can stay lazy
//! messages or already bounded excerpts; the selector never resolves or renders them.
use std::collections::VecDeque;

pub struct MessageWindow<T> {
    pub idx: usize,
    pub start: usize,
    pub end: Option<usize>,
    mode: Mode<T>,
}
enum Mode<T> {
    Range,
    Tail {
        n: usize,
        ring: VecDeque<(usize, T)>,
    },
    Best {
        score: i64,
        previous: Option<(usize, T)>,
        selected: Vec<(usize, T)>,
        pending: bool,
    },
}
impl<T> MessageWindow<T> {
    pub fn new(start: usize, end: Option<usize>) -> Self {
        Self {
            idx: 0,
            start,
            end,
            mode: Mode::Range,
        }
    }
    pub fn tail(mut self, n: usize) -> Self {
        self.mode = Mode::Tail {
            n,
            ring: VecDeque::new(),
        };
        self
    }
    pub fn best() -> Self {
        Self {
            mode: Mode::Best {
                score: -1,
                previous: None,
                selected: Vec::new(),
                pending: false,
            },
            ..Self::new(0, None)
        }
    }
    pub fn is_tail(&self) -> bool {
        matches!(self.mode, Mode::Tail { .. })
    }
    pub fn contains(&self, index: usize) -> bool {
        index >= self.start && self.end.is_none_or(|end| index < end)
    }
    /// Advance the source ordinal even when a relevance projection excludes this message.
    pub fn skip(&mut self) {
        self.idx += 1;
    }
    pub fn push(&mut self, value: T) -> Option<(usize, T)> {
        let i = self.idx;
        self.idx += 1;
        match &mut self.mode {
            Mode::Range => self.contains(i).then_some((i, value)),
            Mode::Tail { n, ring } => {
                if *n > 0 {
                    ring.push_back((i, value));
                    if ring.len() > *n {
                        ring.pop_front();
                    }
                }
                None
            }
            Mode::Best { .. } => unreachable!("relevance windows need a score"),
        }
    }
    /// Finish a deferred selection; the returned source ordinals retain gaps in relevance excerpts.
    pub fn finish(&mut self) -> Vec<(usize, T)> {
        match std::mem::replace(&mut self.mode, Mode::Range) {
            Mode::Range => Vec::new(),
            Mode::Tail { ring, .. } => {
                self.start = self.idx - ring.len();
                ring.into_iter().collect()
            }
            Mode::Best { selected, .. } => selected,
        }
    }
}
impl<T: Clone> MessageWindow<T> {
    /// Highest scoring eligible message, its preceding eligible neighbor and one following
    /// neighbor. Ties retain the first window; a new winner replaces the pending old window.
    pub fn scored(&mut self, value: T, score: i64) {
        let i = self.idx;
        self.idx += 1;
        let Mode::Best {
            score: best,
            previous,
            selected,
            pending,
        } = &mut self.mode
        else {
            unreachable!("scored messages require a relevance window")
        };
        if score > *best {
            *best = score;
            selected.clear();
            selected.extend(previous.take());
            selected.push((i, value.clone()));
            *pending = true;
        } else if *pending {
            selected.push((i, value.clone()));
            *pending = false;
        }
        *previous = Some((i, value));
    }
}
