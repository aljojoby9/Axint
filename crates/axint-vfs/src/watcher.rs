//! Cross-platform change watcher.
//!
//! Windows uses `ReadDirectoryChangesW` through the `notify` crate. Linux uses
//! inotify or fanotify, and macOS uses FSEvents. The ring is the only structure
//! `status` has to drain.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::Mutex;

use crate::error::VfsError;

/// What the kernel reported.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirtyKind {
    /// A path was created.
    Created,
    /// A path's bytes or metadata changed.
    Modified,
    /// A path was removed.
    Deleted,
    /// The event did not name a more specific action.
    Other,
}

/// One dirty path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirtyEvent {
    /// Repository-relative or absolute path, as supplied by the caller.
    pub path: String,
    /// Kernel action.
    pub kind: DirtyKind,
}

/// Bounded queue of dirty paths.
///
/// When the queue is full the oldest event is dropped and `overflowed` is set.
/// Status must scan after an overflow so a lost path cannot hide an edit.
#[derive(Clone, Debug)]
pub struct DirtyRing {
    inner: Arc<Mutex<RingState>>,
}

#[derive(Debug)]
struct RingState {
    events: VecDeque<DirtyEvent>,
    capacity: usize,
    overflowed: bool,
}

impl DirtyRing {
    /// Creates a ring that keeps at most `capacity` events.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(RingState {
                events: VecDeque::new(),
                capacity: capacity.max(1),
                overflowed: false,
            })),
        }
    }

    /// Records one path.
    pub fn push(&self, event: DirtyEvent) {
        let mut state = self.inner.lock();
        if state.events.len() == state.capacity {
            state.events.pop_front();
            state.overflowed = true;
        }
        state.events.push_back(event);
    }

    /// Reports whether an event was dropped.
    #[must_use]
    pub fn overflowed(&self) -> bool {
        self.inner.lock().overflowed
    }

    /// Removes every queued event.
    ///
    /// # Errors
    ///
    /// Returns [`VfsError::RescanRequired`] when the ring overflowed. The
    /// events are still cleared so the next watch window starts empty.
    pub fn drain(&self) -> Result<Vec<DirtyEvent>, VfsError> {
        let mut state = self.inner.lock();
        let events: Vec<_> = state.events.drain(..).collect();
        let overflowed = state.overflowed;
        state.overflowed = false;
        if overflowed {
            Err(VfsError::RescanRequired)
        } else {
            Ok(events)
        }
    }
}

impl Default for DirtyRing {
    fn default() -> Self {
        Self::new(65_536)
    }
}

/// Watches `root` recursively and pushes paths into `ring`.
///
/// # Errors
///
/// Returns an error when the operating system refuses the watch.
pub fn watch(root: &Path, ring: DirtyRing) -> Result<RecommendedWatcher, VfsError> {
    let ring_for_events = ring.clone();
    let mut watcher = RecommendedWatcher::new(
        move |result: Result<Event, notify::Error>| {
            if let Ok(event) = result {
                let kind = match event.kind {
                    EventKind::Create(_) => DirtyKind::Created,
                    EventKind::Modify(_) => DirtyKind::Modified,
                    EventKind::Remove(_) => DirtyKind::Deleted,
                    _ => DirtyKind::Other,
                };
                for path in event.paths {
                    ring_for_events.push(DirtyEvent {
                        path: path.display().to_string(),
                        kind,
                    });
                }
            }
        },
        notify::Config::default(),
    )
    .map_err(|err| VfsError::Watch(err.to_string()))?;
    watcher
        .watch(root, RecursiveMode::Recursive)
        .map_err(|err| VfsError::Watch(err.to_string()))?;
    Ok(watcher)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overflow_forces_a_rescan() {
        let ring = DirtyRing::new(2);
        ring.push(DirtyEvent {
            path: "a".to_owned(),
            kind: DirtyKind::Modified,
        });
        ring.push(DirtyEvent {
            path: "b".to_owned(),
            kind: DirtyKind::Modified,
        });
        ring.push(DirtyEvent {
            path: "c".to_owned(),
            kind: DirtyKind::Created,
        });
        assert!(ring.overflowed());
        assert!(ring.drain().is_err());
        assert!(!ring.overflowed());
    }

    #[test]
    fn drain_returns_paths_in_order() -> Result<(), VfsError> {
        let ring = DirtyRing::new(8);
        ring.push(DirtyEvent {
            path: "one".to_owned(),
            kind: DirtyKind::Created,
        });
        ring.push(DirtyEvent {
            path: "two".to_owned(),
            kind: DirtyKind::Deleted,
        });
        let events = ring.drain()?;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].path, "one");
        assert_eq!(events[1].kind, DirtyKind::Deleted);
        Ok(())
    }
}
