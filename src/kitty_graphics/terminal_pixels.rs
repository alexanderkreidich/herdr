//! Owned graphics exports are read off the render thread, with bounded jobs and pixels.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::ImageSignature;

const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 1024;
const MAX_JOBS: usize = 8;
type Key = (String, u32, ImageSignature);

pub(super) enum Pixels {
    Ready(Arc<[u8]>),
    Pending,
    Unavailable,
}
enum Status {
    Loading,
    Ready(Arc<[u8]>),
    Failed,
}
struct Entry {
    status: Status,
    bytes: usize,
    used: u64,
}
#[derive(Default)]
struct State {
    entries: HashMap<Key, Entry>,
    bytes: usize,
    jobs: usize,
    clock: u64,
}

#[derive(Default)]
pub(crate) struct TerminalPixels(Arc<Mutex<State>>);

impl TerminalPixels {
    pub(super) fn request(
        &self,
        terminal_id: &str,
        image_id: u32,
        signature: ImageSignature,
        source: Arc<crate::pane_graphics_files::OwnedExport>,
        notify: impl FnOnce() + Send + 'static,
    ) -> Pixels {
        let key = (terminal_id.to_owned(), image_id, signature);
        let length = source.len();
        if length > MAX_BYTES {
            return Pixels::Unavailable;
        }
        let Ok(mut state) = self.0.lock() else {
            return Pixels::Unavailable;
        };
        state.clock = state.clock.saturating_add(1);
        let clock = state.clock;
        if let Some(entry) = state.entries.get_mut(&key) {
            entry.used = clock;
            return match &entry.status {
                Status::Loading => Pixels::Pending,
                Status::Ready(data) => Pixels::Ready(Arc::clone(data)),
                Status::Failed => Pixels::Unavailable,
            };
        }
        if state.jobs >= MAX_JOBS {
            return Pixels::Pending;
        }
        while state.bytes.saturating_add(length) > MAX_BYTES || state.entries.len() >= MAX_ENTRIES {
            let oldest = state
                .entries
                .iter()
                .filter(|(_, entry)| !matches!(entry.status, Status::Loading))
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| key.clone());
            let Some(oldest) = oldest else {
                return Pixels::Pending;
            };
            if let Some(entry) = state.entries.remove(&oldest) {
                state.bytes -= entry.bytes;
            }
        }
        state.bytes += length;
        state.jobs += 1;
        state.entries.insert(
            key.clone(),
            Entry {
                status: Status::Loading,
                bytes: length,
                used: clock,
            },
        );
        drop(state);
        let shared = Arc::clone(&self.0);
        let task_key = key.clone();
        let task = std::thread::Builder::new().name("herdr-terminal-pixels".into()).spawn(move || {
            let result = source.copy_rgba();
            if let Ok(mut state) = shared.lock() {
                state.jobs -= 1;
                let mut released = 0;
                if let Some(entry) = state.entries.get_mut(&task_key) {
                    entry.status = match result {
                        Ok(data) => Status::Ready(Arc::from(data)),
                        Err(error) => {
                            tracing::warn!(%error, "failed to read owned terminal graphics pixels");
                            released = entry.bytes;
                            entry.bytes = 0;
                            Status::Failed
                        }
                    };
                }
                state.bytes -= released;
            }
            notify();
        });
        if let Err(error) = task {
            tracing::warn!(%error, "failed to start terminal graphics reader");
            if let Ok(mut state) = self.0.lock() {
                state.jobs -= 1;
                if let Some(entry) = state.entries.remove(&key) {
                    state.bytes -= entry.bytes;
                }
                state.entries.insert(
                    key,
                    Entry {
                        status: Status::Failed,
                        bytes: 0,
                        used: clock,
                    },
                );
            }
            return Pixels::Unavailable;
        }
        Pixels::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn signature() -> ImageSignature {
        ImageSignature {
            image_width: 1,
            image_height: 1,
            format_code: 32,
            data_len: 4,
            data_fingerprint: 1,
        }
    }

    #[test]
    fn owned_pixels_are_read_once_off_thread_and_wake_the_renderer() {
        let files = crate::pane_graphics_files::FileStore::default();
        let source = Arc::new(files.export(&[255, 0, 0, 255]).unwrap());
        let cache = TerminalPixels::default();
        let (sent, received) = std::sync::mpsc::channel();
        assert!(matches!(
            cache.request("terminal", 7, signature(), Arc::clone(&source), move || {
                let _ = sent.send(());
            }),
            Pixels::Pending
        ));
        received
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let Pixels::Ready(data) = cache.request("terminal", 7, signature(), source, || {
            panic!("cached pixels must not start another read")
        }) else {
            panic!("image did not become ready");
        };
        assert_eq!(&*data, &[255, 0, 0, 255]);
        let state = cache.0.lock().unwrap();
        assert_eq!(state.jobs, 0);
        assert_eq!(state.bytes, 4);
    }

    #[test]
    fn readers_do_not_queue_jobs_or_allocate_pixels_past_the_global_budget() {
        let files = crate::pane_graphics_files::FileStore::default();
        let source = Arc::new(files.export(&[255, 0, 0, 255]).unwrap());
        let cache = TerminalPixels::default();
        cache.0.lock().unwrap().jobs = MAX_JOBS;
        assert!(matches!(
            cache.request("terminal", 7, signature(), Arc::clone(&source), || panic!(
                "over-budget job"
            )),
            Pixels::Pending
        ));
        let mut state = cache.0.lock().unwrap();
        state.jobs = 0;
        state.bytes = MAX_BYTES;
        drop(state);
        assert!(matches!(
            cache.request("terminal", 7, signature(), source, || panic!(
                "over-budget pixels"
            )),
            Pixels::Pending
        ));
        assert!(cache.0.lock().unwrap().entries.is_empty());
    }
}
