//! The song document: tracks made of clips, plus every editing operation and undo/redo.
//!
//! Clips are cheap views into shared recorded audio (`source`), so split, duplicate,
//! trim and undo never copy sample data.

use crate::fx::{FxKind, FxSlot};
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
    /// Effects, in order.
    pub fx: Vec<FxSlot>,
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

/// Which effect chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FxTarget {
    /// What you hear while playing and what new takes start with.
    Input,
    Master,
    Track(u64),
}

/// A named spot on the timeline (verse, chorus, "good take starts here"...).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Marker {
    pub id: u64,
    pub pos: usize,
    pub name: String,
}

/// Everything about the song that's saved and undoable.
#[derive(Clone, Default)]
pub struct Song {
    pub tracks: Vec<Track>,
    pub input_fx: Vec<FxSlot>,
    pub master_fx: Vec<FxSlot>,
    pub markers: Vec<Marker>,
}

#[derive(Default)]
pub struct Doc {
    pub tracks: Vec<Track>,
    pub input_fx: Vec<FxSlot>,
    pub master_fx: Vec<FxSlot>,
    /// Sorted by position.
    pub markers: Vec<Marker>,
    next_id: u64,
    undo: Vec<Song>,
    redo: Vec<Song>,
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

    pub fn song(&self) -> Song {
        Song {
            tracks: self.tracks.clone(),
            input_fx: self.input_fx.clone(),
            master_fx: self.master_fx.clone(),
            markers: self.markers.clone(),
        }
    }

    fn restore(&mut self, s: Song) -> Song {
        let old = self.song();
        self.tracks = s.tracks;
        self.input_fx = s.input_fx;
        self.master_fx = s.master_fx;
        self.markers = s.markers;
        old
    }

    /// Call before an edit so it can be undone.
    pub fn checkpoint(&mut self) {
        self.undo.push(self.song());
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
        let cur = self.restore(prev);
        self.redo.push(cur);
        self.touch();
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else {
            return false;
        };
        let cur = self.restore(next);
        self.undo.push(cur);
        self.touch();
        true
    }

    /// Replace everything (new/open project). Clears history.
    pub fn reset(&mut self, song: Song) {
        self.restore(song);
        let fx_ids = self
            .input_fx
            .iter()
            .chain(&self.master_fx)
            .chain(self.tracks.iter().flat_map(|t| &t.fx))
            .map(|f| f.id)
            .chain(self.markers.iter().map(|m| m.id));
        let max = self
            .tracks
            .iter()
            .flat_map(|t| std::iter::once(t.id).chain(t.clips.iter().map(|c| c.id)))
            .chain(fx_ids)
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

    /// Copies of `fx` with fresh ids (for a new track or a duplicate).
    pub fn copy_fx(&mut self, fx: &[FxSlot]) -> Vec<FxSlot> {
        fx.iter()
            .map(|f| FxSlot {
                id: self.new_id(),
                ..f.clone()
            })
            .collect()
    }

    pub fn fx_chain(&self, target: FxTarget) -> Option<&Vec<FxSlot>> {
        match target {
            FxTarget::Input => Some(&self.input_fx),
            FxTarget::Master => Some(&self.master_fx),
            FxTarget::Track(id) => self.track(id).map(|t| &t.fx),
        }
    }

    pub fn fx_chain_mut(&mut self, target: FxTarget) -> Option<&mut Vec<FxSlot>> {
        match target {
            FxTarget::Input => Some(&mut self.input_fx),
            FxTarget::Master => Some(&mut self.master_fx),
            FxTarget::Track(id) => self.track_mut(id).map(|t| &mut t.fx),
        }
    }

    /// Adds an effect to the end of a chain. Returns its id.
    pub fn add_fx(&mut self, target: FxTarget, kind: FxKind) -> Option<u64> {
        self.fx_chain(target)?;
        self.checkpoint();
        let slot = FxSlot::new(self.new_id(), kind);
        let id = slot.id;
        self.fx_chain_mut(target)?.push(slot);
        self.touch();
        Some(id)
    }

    /// Adds a plugin to the end of a chain. Returns its slot id.
    pub fn add_plugin_fx(
        &mut self,
        target: FxTarget,
        plugin: crate::plugins::PluginRef,
    ) -> Option<u64> {
        self.fx_chain(target)?;
        self.checkpoint();
        let slot = FxSlot::new_plugin(self.new_id(), plugin);
        let id = slot.id;
        self.fx_chain_mut(target)?.push(slot);
        self.touch();
        Some(id)
    }

    /// Adds a track holding one clip, with effects `fx`. Returns the track id.
    pub fn add_track(&mut self, name: String, clip: Option<Clip>, fx: Vec<FxSlot>) -> u64 {
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
            fx,
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
                copy.fx = self.copy_fx(&copy.fx);
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

    /// Joins a clip with the next clip on its track into one clip (undoes a split).
    /// Returns the joined clip's id.
    pub fn join_with_next(&mut self, id: u64) -> Option<u64> {
        let (t, a) = self.find_clip(id)?;
        let track = &self.tracks[t];
        let first = &track.clips[a];
        let b = track
            .clips
            .iter()
            .enumerate()
            .filter(|(i, c)| *i != a && c.start >= first.start)
            .min_by_key(|(_, c)| c.start)
            .map(|(i, _)| i)?;
        self.checkpoint();
        let (ca, cb) = (
            self.tracks[t].clips[a].clone(),
            self.tracks[t].clips[b].clone(),
        );
        let seamless = Arc::ptr_eq(&ca.source, &cb.source)
            && cb.start == ca.end()
            && cb.offset == ca.offset + ca.len;
        let joined = if seamless {
            Clip {
                len: ca.len + cb.len,
                ..ca
            }
        } else {
            // Different recordings (or a gap): render the two into one new piece of audio.
            // Where they overlap, the one played on top wins.
            let (top, under) = if b > a { (&cb, &ca) } else { (&ca, &cb) };
            let end = ca.end().max(cb.end());
            let samples: Vec<f32> = (ca.start..end)
                .map(|p| {
                    top.sample_at(p)
                        .or_else(|| under.sample_at(p))
                        .unwrap_or(0.0)
                })
                .collect();
            let mut c = self.make_clip(samples, ca.start);
            c.id = ca.id;
            c
        };
        let clips = &mut self.tracks[t].clips;
        clips[a] = joined;
        clips.remove(b);
        self.touch();
        Some(id)
    }

    /// Adds a marker at `pos` (unless there's one right there). Returns its id.
    pub fn add_marker(&mut self, pos: usize, near: usize) -> Option<u64> {
        if self.markers.iter().any(|m| m.pos.abs_diff(pos) <= near) {
            return None;
        }
        self.checkpoint();
        let id = self.new_id();
        let name = format!("Marker {}", self.markers.len() + 1);
        self.markers.push(Marker { id, pos, name });
        self.markers.sort_by_key(|m| m.pos);
        self.touch();
        Some(id)
    }

    pub fn rename_marker(&mut self, id: u64, name: String) {
        if self.markers.iter().any(|m| m.id == id && m.name != name) {
            self.checkpoint();
            if let Some(m) = self.markers.iter_mut().find(|m| m.id == id) {
                m.name = name;
            }
            self.touch();
        }
    }

    pub fn delete_marker(&mut self, id: u64) {
        if self.markers.iter().any(|m| m.id == id) {
            self.checkpoint();
            self.markers.retain(|m| m.id != id);
            self.touch();
        }
    }

    /// The next marker strictly after `pos`.
    pub fn next_marker(&self, pos: usize) -> Option<usize> {
        self.markers.iter().map(|m| m.pos).find(|&p| p > pos)
    }

    /// The previous marker strictly before `pos` (with a little slack so pressing it
    /// twice while playing keeps going back).
    pub fn prev_marker(&self, pos: usize, slack: usize) -> Option<usize> {
        self.markers
            .iter()
            .rev()
            .map(|m| m.pos)
            .find(|&p| p + slack < pos)
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
        let tid = d.add_track("t".into(), Some(clip), vec![]);
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
    fn fx_edits_are_undoable() {
        let (mut d, tid, _) = doc_with_clip();
        d.add_fx(FxTarget::Track(tid), FxKind::Reverb).unwrap();
        d.add_fx(FxTarget::Master, FxKind::Limiter).unwrap();
        assert_eq!(d.track(tid).unwrap().fx.len(), 1);
        d.undo();
        assert!(d.master_fx.is_empty());
        d.undo();
        assert!(d.track(tid).unwrap().fx.is_empty());
        d.redo();
        let Some(Selection::Track(t2)) = d.duplicate(Selection::Track(tid)) else {
            panic!()
        };
        assert_ne!(
            d.track(t2).unwrap().fx[0].id,
            d.track(tid).unwrap().fx[0].id
        );
    }

    #[test]
    fn join_heals_a_split() {
        let (mut d, tid, cid) = doc_with_clip();
        let right = d.split(cid, 600).unwrap();
        assert_eq!(d.join_with_next(cid), Some(cid));
        let t = d.track(tid).unwrap();
        assert_eq!(t.clips.len(), 1);
        assert_eq!(
            (t.clips[0].start, t.clips[0].len, t.clips[0].offset),
            (100, 1000, 0)
        );
        assert!(
            Arc::ptr_eq(&t.clips[0].source, &t.clips[0].source),
            "no new audio for a clean join"
        );
        assert!(d.find_clip(right).is_none());
        d.undo();
        assert_eq!(d.track(tid).unwrap().clips.len(), 2);
    }

    #[test]
    fn join_renders_different_recordings_with_a_gap() {
        let (mut d, tid, cid) = doc_with_clip(); // 100..1100, values 0..1000
        let other = d.make_clip(vec![-1.0; 100], 1200);
        d.tracks[0].clips.push(other);
        d.join_with_next(cid).unwrap();
        let t = d.track(tid).unwrap();
        assert_eq!(t.clips.len(), 1);
        assert_eq!((t.clips[0].start, t.clips[0].end()), (100, 1300));
        assert_eq!(t.sample_at(500), 400.0);
        assert_eq!(t.sample_at(1150), 0.0, "the gap is silence");
        assert_eq!(t.sample_at(1250), -1.0);
    }

    #[test]
    fn markers_add_jump_rename_delete_undo() {
        let mut d = Doc::new();
        let a = d.add_marker(4800, 100).unwrap();
        assert!(
            d.add_marker(4850, 100).is_none(),
            "no duplicates right next to each other"
        );
        d.add_marker(1000, 100).unwrap();
        assert_eq!(
            d.markers.iter().map(|m| m.pos).collect::<Vec<_>>(),
            [1000, 4800],
            "kept sorted"
        );
        assert_eq!(d.next_marker(1000), Some(4800));
        assert_eq!(d.prev_marker(4800, 0), Some(1000));
        assert_eq!(d.prev_marker(1000, 0), None);
        d.rename_marker(a, "Chorus".into());
        assert_eq!(d.markers[1].name, "Chorus");
        d.delete_marker(a);
        assert_eq!(d.markers.len(), 1);
        d.undo();
        assert_eq!(d.markers[1].name, "Chorus");
    }

    #[test]
    fn move_between_tracks() {
        let (mut d, t1, cid) = doc_with_clip();
        let t2 = d.add_track("b".into(), None, vec![]);
        d.move_clip(cid, 50, Some(t2));
        assert!(d.track(t1).unwrap().clips.is_empty());
        assert_eq!(d.track(t2).unwrap().clips[0].start, 50);
    }

    #[test]
    fn split_all_at_playhead() {
        let (mut d, _, _) = doc_with_clip();
        let c2 = d.make_clip(vec![0.0; 1000], 0);
        d.add_track("b".into(), Some(c2), vec![]);
        assert_eq!(d.split_all_at(500, None), 2);
        assert_eq!(d.tracks.iter().map(|t| t.clips.len()).sum::<usize>(), 4);
        d.undo(); // one undo step for the whole split
        assert_eq!(d.tracks.iter().map(|t| t.clips.len()).sum::<usize>(), 2);
    }
}
