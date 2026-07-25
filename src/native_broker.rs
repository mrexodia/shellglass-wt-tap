//! Out-of-process registry and selection policy for Windows render-tap sources.
//!
//! Pipe tasks decode untrusted adapter messages with [`crate::native_protocol`]
//! and feed them here. The broker chooses at most one foreground source, requests
//! subscriptions, and publishes only complete selected frames through the same
//! `watch::Receiver<Arc<Frame>>` consumed by PTY sessions.

use crate::model::{Color, Frame, Grid};
use crate::native_protocol::{
    Hello, Message, NativeFrame, NativeImageBlob, Packet, Provider, SourceAdded, SourceUpdated,
};
use crate::source::{FramePublisher, SourceSession, external_source};
use anyhow::{Result, bail};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

pub const SOURCE_FOCUSED: u32 = 1;
pub const SOURCE_VISIBLE: u32 = 2;
const IMAGE_STORE_CAP: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceKey {
    pub process_nonce: u64,
    pub source_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokerCommand {
    Subscribe {
        key: SourceKey,
        generation: u64,
        max_fps: u16,
    },
    Unsubscribe {
        key: SourceKey,
        generation: u64,
    },
    RequestFull {
        key: SourceKey,
        generation: u64,
    },
}

struct Source {
    generation: u64,
    provider: Provider,
    owner_hwnd: u64,
    rows: u16,
    cols: u16,
    focused: bool,
    visible: bool,
    last_focus: u64,
    last_frame_sequence: Option<u64>,
    title: String,
    images: ImageStore,
}

#[derive(Default)]
struct ImageStore {
    entries: HashMap<String, crate::model::ImageBlob>,
    order: VecDeque<String>,
    bytes: usize,
}

impl ImageStore {
    fn insert(&mut self, image: NativeImageBlob, protected: &[String]) {
        if self.entries.contains_key(&image.hash) {
            return;
        }
        let len = image.blob.bytes.len();
        if len > IMAGE_STORE_CAP {
            return;
        }
        self.bytes = self.bytes.saturating_add(len);
        self.order.push_back(image.hash.clone());
        self.entries.insert(image.hash, image.blob);
        let mut rotations = 0;
        while self.bytes > IMAGE_STORE_CAP && rotations <= self.order.len() {
            let Some(key) = self.order.pop_front() else {
                break;
            };
            if protected.contains(&key) {
                self.order.push_back(key);
                rotations += 1;
                continue;
            }
            if let Some(blob) = self.entries.remove(&key) {
                self.bytes = self.bytes.saturating_sub(blob.bytes.len());
            }
        }
    }
}

#[derive(Default)]
struct State {
    connections: HashMap<u64, Hello>,
    sources: HashMap<SourceKey, Source>,
    foreground_hwnd: Option<u64>,
    selected: Option<SourceKey>,
    published: Option<(SourceKey, u64)>,
    focus_clock: u64,
    paused: bool,
}

/// Thread-safe native source registry. There is no target-process work here:
/// selection and validation happen in shellglass, outside terminal hosts.
pub struct NativeBroker {
    state: Mutex<State>,
    frames: FramePublisher,
    max_fps: u16,
    keep_last_terminal: bool,
}

impl NativeBroker {
    /// Create a broker and its backend-agnostic source session. The initial
    /// blank frame remains visible until a selected adapter publishes a full.
    pub fn new() -> (Arc<Self>, SourceSession) {
        Self::new_with_policy(true)
    }

    pub fn new_with_policy(keep_last_terminal: bool) -> (Arc<Self>, SourceSession) {
        let (frames, source) = external_source(blank_frame());
        let broker = Arc::new(Self {
            state: Mutex::new(State::default()),
            frames,
            max_fps: 30,
            keep_last_terminal,
        });
        (broker, source)
    }

    /// Process one validated adapter packet and return control messages for pipe
    /// workers to route to the matching adapter connection.
    pub fn handle(&self, packet: Packet) -> Result<Vec<BrokerCommand>> {
        let mut state = self.state.lock().unwrap();
        match packet.message {
            Message::Hello(hello) => {
                if state
                    .connections
                    .insert(packet.process_nonce, hello)
                    .is_some()
                {
                    bail!("duplicate native process nonce");
                }
                Ok(Vec::new())
            }
            message => {
                if !state.connections.contains_key(&packet.process_nonce) {
                    bail!("native source message arrived before HELLO");
                }
                match message {
                    Message::SourceAdded(added) => {
                        self.source_added(&mut state, packet.process_nonce, added)
                    }
                    Message::SourceUpdated(updated) => {
                        self.source_updated(&mut state, packet.process_nonce, updated)
                    }
                    Message::SourceRemoved(removed) => {
                        let key = SourceKey {
                            process_nonce: packet.process_nonce,
                            source_id: removed.source_id,
                        };
                        if state
                            .sources
                            .get(&key)
                            .is_some_and(|source| source.generation == removed.generation)
                        {
                            state.sources.remove(&key);
                            Ok(self.reselect(&mut state))
                        } else {
                            Ok(Vec::new())
                        }
                    }
                    Message::Frame(frame) => {
                        self.frame(&mut state, packet.process_nonce, frame)?;
                        Ok(Vec::new())
                    }
                    Message::ImageBlob(image) => {
                        self.image(&mut state, packet.process_nonce, image);
                        Ok(Vec::new())
                    }
                    Message::Diagnostic(diagnostic) => {
                        // Bounded and control-free logging. The decoder bounds the
                        // text; neuter prevents an injected adapter from writing
                        // terminal escapes into the broker's console.
                        eprintln!(
                            "shellglass native diagnostic {}: {}",
                            diagnostic.code,
                            crate::proto::neuter(&diagnostic.text)
                        );
                        Ok(Vec::new())
                    }
                    Message::Hello(_) => unreachable!(),
                }
            }
        }
    }

    /// Update the foreground top-level HWND. Unless foreground-only policy was
    /// requested, a non-terminal window keeps the last terminal subscribed.
    pub fn foreground_changed(&self, hwnd: Option<u64>) -> Vec<BrokerCommand> {
        let mut state = self.state.lock().unwrap();
        if state.foreground_hwnd == hwnd {
            return Vec::new();
        }
        state.foreground_hwnd = hwnd;
        self.reselect(&mut state)
    }

    /// Remove all sources owned by a disconnected adapter.
    pub fn disconnected(&self, process_nonce: u64) -> Vec<BrokerCommand> {
        let mut state = self.state.lock().unwrap();
        state.connections.remove(&process_nonce);
        state
            .sources
            .retain(|key, _| key.process_nonce != process_nonce);
        self.reselect(&mut state)
    }

    pub fn selected(&self) -> Option<SourceKey> {
        self.state.lock().unwrap().selected
    }

    /// Global privacy pause: unsubscribe immediately and freeze the published
    /// frame; resume reselects the foreground source and requests its fresh full.
    pub fn set_paused(&self, paused: bool) -> Vec<BrokerCommand> {
        let mut state = self.state.lock().unwrap();
        if state.paused == paused {
            return Vec::new();
        }
        state.paused = paused;
        self.reselect(&mut state)
    }

    pub fn status(&self) -> (bool, usize, Option<SourceKey>) {
        let state = self.state.lock().unwrap();
        (state.paused, state.sources.len(), state.selected)
    }

    fn source_added(
        &self,
        state: &mut State,
        process_nonce: u64,
        added: SourceAdded,
    ) -> Result<Vec<BrokerCommand>> {
        let provider = state.connections[&process_nonce].provider;
        let key = SourceKey {
            process_nonce,
            source_id: added.source_id,
        };
        if let Some(current) = state.sources.get(&key)
            && added.generation <= current.generation
        {
            bail!("native source generation did not advance");
        }
        let focused = added.flags & SOURCE_FOCUSED != 0;
        let last_focus = if focused {
            state.focus_clock = state.focus_clock.saturating_add(1);
            state.focus_clock
        } else {
            0
        };
        state.sources.insert(
            key,
            Source {
                generation: added.generation,
                provider,
                owner_hwnd: added.owner_hwnd,
                rows: added.rows,
                cols: added.cols,
                focused,
                visible: added.flags & SOURCE_VISIBLE != 0,
                last_focus,
                last_frame_sequence: None,
                title: added.title,
                images: ImageStore::default(),
            },
        );
        Ok(self.reselect(state))
    }

    fn source_updated(
        &self,
        state: &mut State,
        process_nonce: u64,
        updated: SourceUpdated,
    ) -> Result<Vec<BrokerCommand>> {
        let key = SourceKey {
            process_nonce,
            source_id: updated.source_id,
        };
        let Some(source) = state.sources.get_mut(&key) else {
            return Ok(Vec::new());
        };
        if source.generation != updated.generation {
            return Ok(Vec::new());
        }
        if let Some(hwnd) = updated.owner_hwnd {
            source.owner_hwnd = hwnd;
        }
        if let Some((rows, cols)) = updated.dimensions {
            source.rows = rows;
            source.cols = cols;
        }
        if let Some(visible) = updated.visible {
            source.visible = visible;
        }
        if let Some(title) = updated.title {
            source.title = title;
        }
        if let Some(focused) = updated.focused {
            source.focused = focused;
            if focused {
                state.focus_clock = state.focus_clock.saturating_add(1);
                source.last_focus = state.focus_clock;
            }
        }
        Ok(self.reselect(state))
    }

    fn frame(&self, state: &mut State, process_nonce: u64, mut frame: NativeFrame) -> Result<()> {
        let key = SourceKey {
            process_nonce,
            source_id: frame.source_id,
        };
        let Some(source) = state.sources.get_mut(&key) else {
            return Ok(());
        };
        if source.generation != frame.generation {
            return Ok(());
        }
        if source
            .last_frame_sequence
            .is_some_and(|last| frame.frame_sequence <= last)
        {
            bail!("native frame sequence regressed");
        }
        source.last_frame_sequence = Some(frame.frame_sequence);
        let Frame::Screen(grid) = Arc::make_mut(&mut frame.frame);
        if grid.rows.len() != source.rows as usize || grid.cols != source.cols {
            bail!("native frame dimensions disagree with source metadata");
        }
        for placement in &grid.images {
            let Some(blob) = source.images.entries.get(&placement.hash) else {
                bail!("native frame references an image blob not sent first");
            };
            grid.image_data.insert(placement.hash.clone(), blob.clone());
        }
        if state.selected == Some(key) {
            let identity = (key, frame.generation);
            let frame = Arc::unwrap_or_clone(frame.frame);
            if state.published == Some(identity) {
                self.frames.publish(frame);
            } else {
                self.frames.switch_source(frame);
                state.published = Some(identity);
            }
        }
        Ok(())
    }

    fn image(&self, state: &mut State, process_nonce: u64, image: NativeImageBlob) {
        let key = SourceKey {
            process_nonce,
            source_id: image.source_id,
        };
        let Some(source) = state.sources.get_mut(&key) else {
            return;
        };
        if source.generation != image.generation {
            return;
        }
        let protected = if state.selected == Some(key) {
            let current = self.frames.current();
            let Frame::Screen(grid) = &*current;
            grid.images.iter().map(|p| p.hash.clone()).collect()
        } else {
            Vec::new()
        };
        source.images.insert(image, &protected);
    }

    fn reselect(&self, state: &mut State) -> Vec<BrokerCommand> {
        let next = self.choose(state);
        if next == state.selected {
            return Vec::new();
        }
        let mut commands = Vec::with_capacity(2);
        if let Some(old) = state.selected
            && let Some(source) = state.sources.get(&old)
        {
            commands.push(BrokerCommand::Unsubscribe {
                key: old,
                generation: source.generation,
            });
        }
        state.selected = next;
        if let Some(new) = next {
            let source = &state.sources[&new];
            commands.push(BrokerCommand::Subscribe {
                key: new,
                generation: source.generation,
                max_fps: self.max_fps,
            });
        }
        commands
    }

    fn choose(&self, state: &State) -> Option<SourceKey> {
        if state.paused {
            return None;
        }
        if let Some(hwnd) = state.foreground_hwnd {
            let candidates = |provider| {
                state.sources.iter().filter(move |(_, source)| {
                    source.provider == provider && source.owner_hwnd == hwnd && source.visible
                })
            };

            for provider in [Provider::WindowsTerminal, Provider::Conhost] {
                let mut best: Option<(SourceKey, u64)> = None;
                let mut tied = false;
                for (key, source) in candidates(provider) {
                    match best {
                        None => {
                            best = Some((*key, source.last_focus));
                            tied = false;
                        }
                        Some((_, epoch)) if source.last_focus > epoch => {
                            best = Some((*key, source.last_focus));
                            tied = false;
                        }
                        Some((_, epoch)) if source.last_focus == epoch => tied = true,
                        _ => {}
                    }
                }
                if let Some((key, epoch)) = best {
                    // Multiple never-focused conhosts sharing an HWND are ambiguous.
                    // Retain an already-unambiguous selection; otherwise freeze.
                    if tied && epoch == 0 {
                        if state.selected.is_some_and(|selected| {
                            state.sources.get(&selected).is_some_and(|source| {
                                source.provider == provider
                                    && source.owner_hwnd == hwnd
                                    && source.visible
                            })
                        }) {
                            return state.selected;
                        }
                        continue;
                    }
                    return Some(key);
                }
            }
        }
        if self.keep_last_terminal {
            // Switching to Discord, a browser, the desktop, or an unknown HWND
            // must not replace a known terminal selection. This also avoids
            // downgrading a selected WT pane to its backing conhost provider.
            if state.selected.is_some_and(|selected| {
                state
                    .sources
                    .get(&selected)
                    .is_some_and(|source| source.visible)
            }) {
                return state.selected;
            }
            return state
                .sources
                .iter()
                .filter(|(_, source)| source.visible && source.last_focus != 0)
                .max_by_key(|(_, source)| {
                    (
                        source.last_focus,
                        source.provider == Provider::WindowsTerminal,
                    )
                })
                .map(|(key, _)| *key);
        }
        None
    }
}

fn blank_frame() -> Frame {
    Frame::Screen(Grid {
        source_epoch: 0,
        cols: 80,
        rows: vec![vec![Default::default(); 80]; 24],
        cursor: None,
        cursor_style: 0,
        default_colors: (Color::Default, Color::Default),
        title: String::new(),
        links: Default::default(),
        images: Vec::new(),
        image_data: Default::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_protocol::testwire;
    use crate::native_protocol::{Decoder, MessageType};

    struct Adapter {
        nonce: u64,
        sequence: u64,
        decoder: Decoder,
    }

    impl Adapter {
        fn new(broker: &NativeBroker, nonce: u64, provider: Provider) -> Self {
            let mut adapter = Self {
                nonce,
                sequence: 0,
                decoder: Decoder::default(),
            };
            adapter.send(broker, MessageType::Hello, &testwire::hello(provider));
            adapter
        }

        fn send(
            &mut self,
            broker: &NativeBroker,
            kind: MessageType,
            payload: &[u8],
        ) -> Vec<BrokerCommand> {
            self.sequence += 1;
            let bytes = testwire::packet(kind, self.nonce, self.sequence, payload);
            let packets = self.decoder.push(&bytes).unwrap();
            packets
                .into_iter()
                .flat_map(|packet| broker.handle(packet).unwrap())
                .collect()
        }
    }

    #[test]
    fn foreground_only_policy_unsubscribes_outside_terminals() {
        let (broker, _source) = NativeBroker::new_with_policy(false);
        broker.foreground_changed(Some(77));
        let mut conhost = Adapter::new(&broker, 1, Provider::Conhost);
        let commands = conhost.send(
            &broker,
            MessageType::SourceAdded,
            &testwire::source_added(10, 1, 77),
        );
        assert!(commands.is_empty()); // never-focused conhost is still usable if unique
        // Visibility is explicit; the basic helper's flags are zero.
        let mut visible = Vec::new();
        visible.extend_from_slice(&10u64.to_le_bytes());
        visible.extend_from_slice(&1u64.to_le_bytes());
        visible.push(8); // visible field
        visible.push(1);
        let commands = conhost.send(&broker, MessageType::SourceUpdated, &visible);
        assert!(matches!(
            commands.as_slice(),
            [BrokerCommand::Subscribe { .. }]
        ));

        let mut wt = Adapter::new(&broker, 2, Provider::WindowsTerminal);
        wt.send(
            &broker,
            MessageType::SourceAdded,
            &testwire::source_added(20, 1, 77),
        );
        let mut update = Vec::new();
        update.extend_from_slice(&20u64.to_le_bytes());
        update.extend_from_slice(&1u64.to_le_bytes());
        update.push(12); // focused + visible
        update.push(1);
        update.push(1);
        let commands = wt.send(&broker, MessageType::SourceUpdated, &update);
        assert!(matches!(
            commands.as_slice(),
            [
                BrokerCommand::Unsubscribe { .. },
                BrokerCommand::Subscribe { .. }
            ]
        ));
        assert_eq!(broker.selected().unwrap().process_nonce, 2);

        let commands = broker.foreground_changed(None);
        assert!(matches!(
            commands.as_slice(),
            [BrokerCommand::Unsubscribe { .. }]
        ));
        assert_eq!(broker.selected(), None);
    }

    #[test]
    fn selected_full_frames_publish_and_stale_generations_do_not() {
        let (broker, mut source) = NativeBroker::new();
        broker.foreground_changed(Some(7));
        let mut wt = Adapter::new(&broker, 4, Provider::WindowsTerminal);
        wt.send(
            &broker,
            MessageType::SourceAdded,
            &testwire::source_added(1, 2, 7),
        );
        let mut update = Vec::new();
        update.extend_from_slice(&1u64.to_le_bytes());
        update.extend_from_slice(&2u64.to_le_bytes());
        update.push(12);
        update.push(1);
        update.push(1);
        wt.send(&broker, MessageType::SourceUpdated, &update);

        // Stale generation is ignored.
        wt.send(&broker, MessageType::Frame, &testwire::frame(1, 1, 1, "s"));
        {
            let current = source.frames.borrow_and_update();
            let Frame::Screen(grid) = &**current;
            assert_eq!(grid.cols, 80);
        }

        wt.send(&broker, MessageType::Frame, &testwire::frame(1, 2, 2, "n"));
        let current = source.frames.borrow_and_update();
        let Frame::Screen(grid) = &**current;
        assert_eq!(grid.rows[0][0].text, "n");
    }

    #[test]
    fn focus_switch_remove_and_newest_frame_follow_one_wt_window() {
        let (broker, mut source) = NativeBroker::new();
        broker.foreground_changed(Some(77));
        let mut wt = Adapter::new(&broker, 9, Provider::WindowsTerminal);
        for id in [1u64, 2] {
            wt.send(
                &broker,
                MessageType::SourceAdded,
                &testwire::source_added(id, 1, 77),
            );
            let mut update = Vec::new();
            update.extend_from_slice(&id.to_le_bytes());
            update.extend_from_slice(&1u64.to_le_bytes());
            update.push(12); // focused + visible
            update.push(1);
            update.push(1);
            wt.send(&broker, MessageType::SourceUpdated, &update);
        }
        assert_eq!(broker.selected().unwrap().source_id, 2);

        wt.send(&broker, MessageType::Frame, &testwire::frame(2, 1, 1, "a"));
        wt.send(&broker, MessageType::Frame, &testwire::frame(2, 1, 2, "b"));
        let current = source.frames.borrow_and_update();
        let Frame::Screen(grid) = &**current;
        assert_eq!(
            grid.rows[0][0].text, "b",
            "watch retains the newest full frame"
        );
        drop(current);

        let mut removed = Vec::new();
        removed.extend_from_slice(&2u64.to_le_bytes());
        removed.extend_from_slice(&1u64.to_le_bytes());
        let commands = wt.send(&broker, MessageType::SourceRemoved, &removed);
        assert!(matches!(
            commands.as_slice(),
            [BrokerCommand::Subscribe { .. }]
        ));
        assert_eq!(broker.selected().unwrap().source_id, 1);
    }

    #[test]
    fn source_resize_metadata_rejects_an_old_sized_frame() {
        let (broker, _source) = NativeBroker::new();
        broker.foreground_changed(Some(7));
        let mut wt = Adapter::new(&broker, 3, Provider::WindowsTerminal);
        wt.send(
            &broker,
            MessageType::SourceAdded,
            &testwire::source_added(1, 1, 7),
        );
        let mut update = Vec::new();
        update.extend_from_slice(&1u64.to_le_bytes());
        update.extend_from_slice(&1u64.to_le_bytes());
        update.push(14); // dimensions + focused + visible
        update.extend_from_slice(&2u16.to_le_bytes());
        update.extend_from_slice(&2u16.to_le_bytes());
        update.push(1);
        update.push(1);
        wt.send(&broker, MessageType::SourceUpdated, &update);

        wt.sequence += 1;
        let bytes = testwire::packet(
            MessageType::Frame,
            wt.nonce,
            wt.sequence,
            &testwire::frame(1, 1, 1, "x"),
        );
        let packet = wt.decoder.push(&bytes).unwrap().pop().unwrap();
        assert!(broker.handle(packet).is_err());
    }

    #[test]
    fn default_policy_keeps_the_last_wt_live_outside_terminal_windows() {
        let (broker, _source) = NativeBroker::new();
        broker.foreground_changed(Some(7));
        let mut wt = Adapter::new(&broker, 1, Provider::WindowsTerminal);
        wt.send(
            &broker,
            MessageType::SourceAdded,
            &testwire::source_added(1, 1, 7),
        );
        let mut visible_focused = Vec::new();
        visible_focused.extend_from_slice(&1u64.to_le_bytes());
        visible_focused.extend_from_slice(&1u64.to_le_bytes());
        visible_focused.push(12);
        visible_focused.push(1);
        visible_focused.push(1);
        wt.send(&broker, MessageType::SourceUpdated, &visible_focused);
        let selected = broker.selected().unwrap();

        assert!(broker.foreground_changed(Some(999)).is_empty());
        assert_eq!(broker.selected(), Some(selected));
        assert!(broker.foreground_changed(None).is_empty());
        assert_eq!(broker.selected(), Some(selected));
    }

    #[test]
    fn ambiguous_conhosts_do_not_oscillate() {
        let (broker, _source) = NativeBroker::new();
        broker.foreground_changed(Some(7));
        let mut a = Adapter::new(&broker, 1, Provider::Conhost);
        let mut b = Adapter::new(&broker, 2, Provider::Conhost);
        for adapter in [&mut a, &mut b] {
            adapter.send(
                &broker,
                MessageType::SourceAdded,
                &testwire::source_added(1, 1, 7),
            );
            let mut visible = Vec::new();
            visible.extend_from_slice(&1u64.to_le_bytes());
            visible.extend_from_slice(&1u64.to_le_bytes());
            visible.push(8);
            visible.push(1);
            adapter.send(&broker, MessageType::SourceUpdated, &visible);
        }
        // The first unique selection is retained; adding an equally ambiguous
        // peer does not flap to it.
        assert_eq!(broker.selected().unwrap().process_nonce, 1);
    }
}
