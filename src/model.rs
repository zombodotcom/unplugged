//! The song document: tracks made of clips, plus every editing operation and undo/redo.
//!
//! Clips are cheap views into shared recorded audio (`source`), so split, duplicate,
//! trim and undo never copy sample data.

use std::sync::Arc;

/// Samples per waveform peak bucket.
pub const PEAK_BUCKET: usize = 256;
/// Shortest clip a trim or split may leave behind, in samples.
pub const MIN_CLIP: usize = 64;
const UNDO_LIMIT: usize = 200;

pub fn compute_peaks(samples: &[f32]) -> Vec<[f32; 2]> {
    samples
        .chunks(PEAK_BUCKET)
        .map(|c| {
            c.iter()
                .fold([0.0f32, 0.0f32], |[lo, hi], &s| [lo.min(s), hi.max(s)])
        })
        .collect()
}

#[derive(Clone, Debug)]
pub struct Clip {
    pub id: u64,
    /// The recorded audio this clip plays a window of.
    pub source: Arc<Vec<f32>>,
    pub peaks: Arc<Vec<[f32; 2]>>,
    /// Timeline position of the clip's first sample.
    pub start: usize,
    /// First sample used from `source`.
    pub offset: usize,
    pub len: usize,
}

impl Clip {
    pub fn end(&self) -> usize {
        self.start + self.len
    }

    #[inline]
    pub fn sample_at(&self, pos: usize) -> Option<f32> {
        (pos >= self.start && pos < self.end()).then(|| self.source[self.offset + pos - self.start])
    }
}

#[derive(Clone, Debug)]
pub struct Track {
    pub id: u64,
    pub name: String,
    /// Later clips play on top of earlier ones where they overlap.
    pub clips: Vec<Clip>,
    pub volume: f32,
    /// -1 = left, 0 = centre, 1 = right.
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
    /// Play back through the acoustic simulator.
    pub acoustic: bool,
    /// Flip polarity (phase), e.g. to line up two takes that cancel each other.
    pub invert: bool,
}

impl Track {
    pub fn end(&self) -> usize {
        self.clips.iter().map(Clip::end).max().unwrap_or(0)
    }

    pub fn start(&self) -> usize {
        self.clips.iter().map(|c| c.start).min().unwrap_or(0)
    }

    #[inline]
    pub fn sample_at(&self, pos: usize) -> f32 {
        self.clips
            .iter()
            .rev()
            .find_map(|c| c.sample_at(pos))
            .unwrap_or(0.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    Track(u64),
    Clip(u64),
}

#[derive(Default)]
pub struct Doc {
    pub tracks: Vec<Track>,
    next_id: u64,
    undo: Vec<Vec<Track>>,
    redo: Vec<Vec<Track>>,
    /// Bumped on every change so the engine knows to resync.
    pub revision: u64,
}

impl Doc {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            ..Default::default()
        }
    }

    pub fn new_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Call before an edit so it can be undone.
    pub fn checkpoint(&mut self) {
        self.undo.push(self.tracks.clone());
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    /// Mark that tracks changed (the engine resyncs on the next frame).
    pub fn touch(&mut self) {
        self.revision += 1;
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn undo(&mut self) -> bool {
        let Some(prev) = self.undo.pop() else {
            return false;
        };
        self.redo.push(std::mem::replace(&mut self.tracks, prev));
        self.touch();
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else {
            return false;
        };
        self.undo.push(std::mem::replace(&mut self.tracks, next));
        self.touch();
        true
    }

    /// Replace everything (new/open project). Clears history.
    pub fn reset(&mut self, tracks: Vec<Track>) {
        self.tracks = tracks;
        let max = self
            .tracks
            .iter()
            .flat_map(|t| std::iter::once(t.id).chain(t.clips.iter().map(|c| c.id)))
            .max()
            .unwrap_or(0);
        self.next_id = self.next_id.max(max);
        self.undo.clear();
        self.redo.clear();
        self.touch();
    }

    pub fn end(&self) -> usize {
        self.tracks.iter().map(Track::end).max().unwrap_or(0)
    }

    pub fn track(&self, id: u64) -> Option<&Track> {
        self.tracks.iter().find(|t| t.id == id)
    }

    pub fn track_mut(&mut self, id: u64) -> Option<&mut Track> {
        self.tracks.iter_mut().find(|t| t.id == id)
    }

    /// (track index, clip index) of a clip.
    pub fn find_clip(&self, clip_id: u64) -> Option<(usize, usize)> {
        self.tracks.iter().enumerate().find_map(|(ti, t)| {
            t.clips
                .iter()
                .position(|c| c.id == clip_id)
                .map(|ci| (ti, ci))
        })
    }

    pub fn clip(&self, clip_id: u64) -> Option<&Clip> {
        self.find_clip(clip_id)
            .map(|(t, c)| &self.tracks[t].clips[c])
    }

    pub fn make_clip(&mut self, samples: Vec<f32>, start: usize) -> Clip {
        let len = samples.len();
        Clip {
            id: self.new_id(),
            peaks: Arc::new(compute_peaks(&samples)),
            source: Arc::new(samples),
            start,
            offset: 0,
            len,
        }
    }

    /// Adds a track holding one clip. Returns the track id.
    pub fn add_track(&mut self, name: String, clip: Option<Clip>, acoustic: bool) -> u64 {
        self.checkpoint();
        let id = self.new_id();
        self.tracks.push(Track {
            id,
            name,
            clips: clip.into_iter().collect(),
            volume: 0.8,
            pan: 0.0,
            mute: false,
            solo: false,
            acoustic,
            invert: false,
        });
        self.touch();
        id
    }

    pub fn delete(&mut self, sel: Selection) -> bool {
        match sel {
            Selection::Track(id) => {
                let Some(i) = self.tracks.iter().position(|t| t.id == id) else {
                    return false;
                };
                self.checkpoint();
                self.tracks.remove(i);
            }
            Selection::Clip(id) => {
                let Some((t, c)) = self.find_clip(id) else {
                    return false;
                };
                self.checkpoint();
                self.tracks[t].clips.remove(c);
            }
        }
        self.touch();
        true
    }

    /// Duplicates a clip right after itself, or a whole track below itself.
    /// Returns the new item.
    pub fn duplicate(&mut self, sel: Selection) -> Option<Selection> {
        match sel {
            Selection::Clip(id) => {
                let (t, c) = self.find_clip(id)?;
                self.checkpoint();
                let mut copy = self.tracks[t].clips[c].clone();
                copy.id = self.new_id();
                copy.start = copy.end();
                let new = copy.id;
                self.tracks[t].clips.push(copy);
                self.touch();
                Some(Selection::Clip(new))
            }
            Selection::Track(id) => {
                let i = self.tracks.iter().position(|t| t.id == id)?;
                self.checkpoint();
                let mut copy = self.tracks[i].clone();
                copy.id = self.new_id();
                copy.name = format!("{} (copy)", copy.name);
                copy.solo = false;
                for c in &mut copy.clips {
                    c.id = self.new_id();
                }
                let new = copy.id;
                self.tracks.insert(i + 1, copy);
                self.touch();
                Some(Selection::Track(new))
            }
        }
    }

    /// Splits clip `id` at timeline position `at`. Returns the id of the right half.
    pub fn split(&mut self, id: u64, at: usize) -> Option<u64> {
        let (t, c) = self.find_clip(id)?;
        let clip = &self.tracks[t].clips[c];
        if at < clip.start + MIN_CLIP || at + MIN_CLIP > clip.end() {
            return None;
        }
        self.checkpoint();
        self.split_unchecked(t, c, at)
    }

    fn split_unchecked(&mut self, t: usize, c: usize, at: usize) -> Option<u64> {
        let new_id = self.new_id();
        let clip = &mut self.tracks[t].clips[c];
        let left_len = at - clip.start;
        let mut right = clip.clone();
        right.id = new_id;
        right.start = at;
        right.offset += left_len;
        right.len -= left_len;
        clip.len = left_len;
        self.tracks[t].clips.insert(c + 1, right);
        self.touch();
        Some(new_id)
    }

    /// Splits every clip under `at` (optionally only on one track). Returns how many were split.
    pub fn split_all_at(&mut self, at: usize, track: Option<u64>) -> usize {
        let ids: Vec<u64> = self
            .tracks
            .iter()
            .filter(|t| track.is_none_or(|id| t.id == id))
            .flat_map(|t| t.clips.iter())
            .filter(|c| at > c.start + MIN_CLIP && at + MIN_CLIP < c.end())
            .map(|c| c.id)
            .collect();
        if !ids.is_empty() {
            self.checkpoint();
        }
        for &id in &ids {
            if let Some((t, c)) = self.find_clip(id) {
                self.split_unchecked(t, c, at);
            }
        }
        ids.len()
    }

    /// Moves a clip (no checkpoint: call `checkpoint` once when a drag starts).
    pub fn move_clip(&mut self, id: u64, start: usize, to_track: Option<u64>) {
        let Some((t, c)) = self.find_clip(id) else {
            return;
        };
        let mut clip = self.tracks[t].clips.remove(c);
        clip.start = start;
        let dest = to_track
            .and_then(|tid| self.tracks.iter().position(|t| t.id == tid))
            .unwrap_or(t);
        if dest == t {
            self.tracks[t].clips.insert(c, clip);
        } else {
            self.tracks[dest].clips.push(clip);
        }
        self.touch();
    }

    /// Moves a clip's left edge, keeping its audio in place (no checkpoint).
    pub fn trim_start(&mut self, id: u64, new_start: usize) {
        let Some((t, c)) = self.find_clip(id) else {
            return;
        };
        let clip = &mut self.tracks[t].clips[c];
        let earliest = clip.start - clip.offset;
        let latest = clip.end() - MIN_CLIP;
        let s = new_start.clamp(earliest, latest);
        let end = clip.end();
        clip.offset = s - earliest;
        clip.start = s;
        clip.len = end - s;
        self.touch();
    }

    /// Moves a clip's right edge (no checkpoint).
    pub fn trim_end(&mut self, id: u64, new_end: usize) {
        let Some((t, c)) = self.find_clip(id) else {
            return;
        };
        let clip = &mut self.tracks[t].clips[c];
        let latest = clip.start + clip.source.len() - clip.offset;
        let e = new_end.clamp(clip.start + MIN_CLIP, latest);
        clip.len = e - clip.start;
        self.touch();
    }

    pub fn move_track(&mut self, id: u64, delta: isize) {
        let Some(i) = self.tracks.iter().position(|t| t.id == id) else {
            return;
        };
        let j = (i as isize + delta).clamp(0, self.tracks.len() as isize - 1) as usize;
        if i != j {
            self.checkpoint();
            let t = self.tracks.remove(i);
            self.tracks.insert(j, t);
            self.touch();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc_with_clip() -> (Doc, u64, u64) {
        let mut d = Doc::new();
        let clip = d.make_clip((0..1000).map(|i| i as f32).collect(), 100);
        let cid = clip.id;
        let tid = d.add_track("t".into(), Some(clip), false);
        (d, tid, cid)
    }

    #[test]
    fn split_keeps_audio_continuous() {
        let (mut d, tid, cid) = doc_with_clip();
        let right = d.split(cid, 600).unwrap();
        let t = d.track(tid).unwrap();
        assert_eq!(t.clips.len(), 2);
        for pos in [100, 599, 600, 1099] {
            assert_eq!(t.sample_at(pos), (pos - 100) as f32);
        }
        assert_eq!(d.clip(right).unwrap().offset, 500);
        assert!(d.split(cid, 101).is_none(), "too close to the edge");
    }

    #[test]
    fn delete_and_undo_redo() {
        let (mut d, tid, cid) = doc_with_clip();
        assert!(d.delete(Selection::Clip(cid)));
        assert!(d.track(tid).unwrap().clips.is_empty());
        assert!(d.undo());
        assert_eq!(d.track(tid).unwrap().clips.len(), 1);
        assert!(d.redo());
        assert!(d.track(tid).unwrap().clips.is_empty());
        assert!(d.delete(Selection::Track(tid)));
        assert!(d.tracks.is_empty());
        d.undo();
        assert_eq!(d.tracks.len(), 1);
    }

    #[test]
    fn duplicate_clip_lands_after_original() {
        let (mut d, tid, cid) = doc_with_clip();
        let Some(Selection::Clip(n)) = d.duplicate(Selection::Clip(cid)) else {
            panic!()
        };
        assert_eq!(d.clip(n).unwrap().start, 1100);
        assert_eq!(d.track(tid).unwrap().sample_at(1100), 0.0);
        assert_eq!(d.track(tid).unwrap().sample_at(1101), 1.0);
        let Some(Selection::Track(t2)) = d.duplicate(Selection::Track(tid)) else {
            panic!()
        };
        assert_eq!(d.tracks[1].id, t2);
        assert_ne!(d.tracks[1].clips[0].id, d.tracks[0].clips[0].id);
    }

    #[test]
    fn trim_respects_source_bounds() {
        let (mut d, _, cid) = doc_with_clip();
        d.trim_start(cid, 0); // can't go before the recording started
        assert_eq!(d.clip(cid).unwrap().start, 100);
        d.trim_start(cid, 300);
        let c = d.clip(cid).unwrap().clone();
        assert_eq!((c.start, c.offset, c.len), (300, 200, 800));
        assert_eq!(c.sample_at(300), Some(200.0));
        d.trim_end(cid, 5000);
        assert_eq!(d.clip(cid).unwrap().end(), 1100);
        d.trim_end(cid, 500);
        assert_eq!(d.clip(cid).unwrap().len, 200);
        d.trim_start(cid, 100); // extend back out again
        assert_eq!(d.clip(cid).unwrap().offset, 0);
    }

    #[test]
    fn move_between_tracks() {
        let (mut d, t1, cid) = doc_with_clip();
        let t2 = d.add_track("b".into(), None, false);
        d.move_clip(cid, 50, Some(t2));
        assert!(d.track(t1).unwrap().clips.is_empty());
        assert_eq!(d.track(t2).unwrap().clips[0].start, 50);
    }

    #[test]
    fn split_all_at_playhead() {
        let (mut d, _, _) = doc_with_clip();
        let c2 = d.make_clip(vec![0.0; 1000], 0);
        d.add_track("b".into(), Some(c2), false);
        assert_eq!(d.split_all_at(500, None), 2);
        assert_eq!(d.tracks.iter().map(|t| t.clips.len()).sum::<usize>(), 4);
        d.undo(); // one undo step for the whole split
        assert_eq!(d.tracks.iter().map(|t| t.clips.len()).sum::<usize>(), 2);
    }
}
