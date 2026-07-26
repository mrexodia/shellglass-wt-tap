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

#[derive(Debug, Clone, PartialEq, Eq)]
enum PublishedSource {
    Terminal(SourceKey, u64),
    Accessibility(String),
}

#[derive(Default)]
struct State {
    connections: HashMap<u64, Hello>,
    sources: HashMap<SourceKey, Source>,
    foreground_hwnd: Option<u64>,
    selected: Option<SourceKey>,
    published: Option<PublishedSource>,
    focus_clock: u64,
    foreground_epoch: u64,
    privacy_hold_terminal: bool,
    pending_full: Option<(SourceKey, u64)>,
    paused: bool,
}

/// Thread-safe native source registry. There is no target-process work here:
/// selection and validation happen in shellglass, outside terminal hosts.
pub struct NativeBroker {
    state: Mutex<State>,
    frames: FramePublisher,
    max_fps: u16,
    keep_last_terminal: bool,
    accessibility_fallback: bool,
}

impl NativeBroker {
    /// Create a broker and its backend-agnostic source session. The initial
    /// blank frame remains visible until a selected adapter publishes a full.
    pub fn new() -> (Arc<Self>, SourceSession) {
        Self::new_with_policy(true)
    }

    pub fn new_with_policy(keep_last_terminal: bool) -> (Arc<Self>, SourceSession) {
        Self::new_with_modes(keep_last_terminal, false)
    }

    pub fn new_hybrid() -> (Arc<Self>, SourceSession) {
        Self::new_with_modes(true, true)
    }

    fn new_with_modes(
        keep_last_terminal: bool,
        accessibility_fallback: bool,
    ) -> (Arc<Self>, SourceSession) {
        let (frames, source) = external_source(blank_frame());
        let broker = Arc::new(Self {
            state: Mutex::new(State::default()),
            frames,
            max_fps: 30,
            keep_last_terminal,
            accessibility_fallback,
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
        state.foreground_epoch = state.foreground_epoch.wrapping_add(1);
        state.privacy_hold_terminal = false;
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

    /// Return a foreground-generation ticket when accessibility may capture.
    /// A retained terminal subscription does not block accessibility unless it
    /// actually belongs to the foreground HWND.
    pub fn accessibility_ticket(&self) -> Option<u64> {
        let state = self.state.lock().unwrap();
        (!state.paused && !state.privacy_hold_terminal && self.choose_foreground(&state).is_none())
            .then_some(state.foreground_epoch)
    }

    pub fn wants_accessibility(&self) -> bool {
        self.accessibility_ticket().is_some()
    }

    /// Publish an accessibility reconstruction only if the foreground has not
    /// changed since capture began and has no native terminal source.
    pub fn publish_accessibility(&self, ticket: u64, identity: String, frame: Frame) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.paused
            || state.foreground_epoch != ticket
            || self.choose_foreground(&state).is_some()
        {
            return false;
        }
        state.privacy_hold_terminal = false;
        state.pending_full = None;
        let source = PublishedSource::Accessibility(identity);
        if state.published.as_ref() == Some(&source) {
            self.frames.publish(frame);
        } else {
            self.frames.switch_source(frame);
            state.published = Some(source);
        }
        true
    }

    /// Keep the retained terminal live when the matching accessibility app is
    /// privacy-blocked. The ticket prevents a stale capture from changing the
    /// policy after another foreground transition.
    pub fn accessibility_blocked(&self, ticket: u64) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.paused
            || state.foreground_epoch != ticket
            || self.choose_foreground(&state).is_some()
        {
            return false;
        }
        state.privacy_hold_terminal = true;
        true
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

    pub fn status(&self) -> (bool, usize, Option<SourceKey>, bool) {
        let state = self.state.lock().unwrap();
        let terminal_presented = !self.accessibility_fallback
            || state.privacy_hold_terminal
            || self.choose_foreground(&state).is_some();
        let selected = terminal_presented.then_some(state.selected).flatten();
        (
            state.paused,
            state.sources.len(),
            selected,
            selected.is_none()
                && matches!(state.published, Some(PublishedSource::Accessibility(_))),
        )
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
            // Metadata packets always carry the current focus bit, including
            // unrelated title/resize updates. Count only a false -> true edge;
            // otherwise a late repeated `true` from the old tab can outrank the
            // newly focused tab and make selection snap back intermittently.
            let gained_focus = focused && !source.focused;
            source.focused = focused;
            if gained_focus {
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
        let publish_native = !self.accessibility_fallback
            || state.privacy_hold_terminal
            || self
                .choose_foreground(state)
                .is_some_and(|foreground| foreground == key);
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
        if state.pending_full == Some((key, frame.generation)) {
            state.pending_full = None;
        }
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
        if state.selected == Some(key) && publish_native {
            let identity = PublishedSource::Terminal(key, frame.generation);
            let frame = Arc::unwrap_or_clone(frame.frame);
            if state.published.as_ref() == Some(&identity) {
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
        let mut commands = Vec::with_capacity(3);
        if next != state.selected {
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
        }

        let foreground = self.choose_foreground(state);
        let restore = next.filter(|key| {
            self.accessibility_fallback
                && (state.privacy_hold_terminal || foreground == Some(*key))
                && state.sources.get(key).is_some_and(|source| {
                    state.published != Some(PublishedSource::Terminal(*key, source.generation))
                })
        });
        if let Some(key) = restore {
            let generation = state.sources[&key].generation;
            if state.pending_full != Some((key, generation)) {
                commands.push(BrokerCommand::RequestFull { key, generation });
                state.pending_full = Some((key, generation));
            }
        } else {
            state.pending_full = None;
        }
        commands
    }

    fn choose_foreground(&self, state: &State) -> Option<SourceKey> {
        let hwnd = state.foreground_hwnd?;
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
        None
    }

    fn choose(&self, state: &State) -> Option<SourceKey> {
        if state.paused {
            return None;
        }
        if let Some(foreground) = self.choose_foreground(state) {
            return Some(foreground);
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
    fn accessibility_publishes_only_without_a_selected_native_terminal() {
        let (broker, mut source) = NativeBroker::new_with_policy(false);
        assert!(broker.wants_accessibility());
        let mut accessibility = blank_frame();
        let Frame::Screen(grid) = &mut accessibility;
        grid.rows[0][0].text = "a11y".into();
        let initial_ticket = broker.accessibility_ticket().unwrap();
        assert!(broker.publish_accessibility(initial_ticket, "window-1".into(), accessibility));
        {
            let current = source.frames.borrow_and_update();
            let Frame::Screen(grid) = &**current;
            assert_eq!(grid.rows[0][0].text, "a11y");
        }

        broker.foreground_changed(Some(77));
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
        wt.send(&broker, MessageType::SourceUpdated, &update);
        assert!(!broker.wants_accessibility());
        assert!(!broker.publish_accessibility(0, "window-2".into(), blank_frame()));
        {
            let current = source.frames.borrow_and_update();
            let Frame::Screen(grid) = &**current;
            assert_eq!(grid.rows[0][0].text, "a11y");
        }

        wt.send(
            &broker,
            MessageType::Frame,
            &testwire::frame(20, 1, 1, "native"),
        );
        {
            let current = source.frames.borrow_and_update();
            let Frame::Screen(grid) = &**current;
            assert_eq!(grid.rows[0][0].text, "native");
        }

        broker.foreground_changed(Some(999));
        assert!(broker.wants_accessibility());
        let mut next_accessibility = blank_frame();
        let Frame::Screen(grid) = &mut next_accessibility;
        grid.rows[0][0].text = "next-a11y".into();
        let next_ticket = broker.accessibility_ticket().unwrap();
        assert!(broker.publish_accessibility(next_ticket, "window-2".into(), next_accessibility));
        let current = source.frames.borrow_and_update();
        let Frame::Screen(grid) = &**current;
        assert_eq!(grid.rows[0][0].text, "next-a11y");
    }

    #[test]
    fn blocked_accessibility_keeps_the_retained_terminal_live() {
        let (broker, mut source) = NativeBroker::new_hybrid();
        broker.foreground_changed(Some(77));
        let mut wt = Adapter::new(&broker, 9, Provider::WindowsTerminal);
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
        wt.send(&broker, MessageType::SourceUpdated, &update);
        wt.send(
            &broker,
            MessageType::Frame,
            &testwire::frame(20, 1, 1, "terminal-1"),
        );

        broker.foreground_changed(Some(999));
        let ticket = broker
            .accessibility_ticket()
            .expect("non-terminal foreground should request accessibility");
        wt.send(
            &broker,
            MessageType::Frame,
            &testwire::frame(20, 1, 2, "suppressed-before-policy"),
        );
        {
            let current = source.frames.borrow_and_update();
            let Frame::Screen(grid) = &**current;
            assert_eq!(grid.rows[0][0].text, "terminal-1");
        }

        assert!(broker.accessibility_blocked(ticket));
        assert!(!broker.wants_accessibility());
        wt.send(
            &broker,
            MessageType::Frame,
            &testwire::frame(20, 1, 3, "terminal-continues"),
        );
        {
            let current = source.frames.borrow_and_update();
            let Frame::Screen(grid) = &**current;
            assert_eq!(grid.rows[0][0].text, "terminal-continues");
        }

        broker.foreground_changed(Some(1_000));
        let next_ticket = broker
            .accessibility_ticket()
            .expect("new foreground should clear the privacy hold");
        assert!(!broker.accessibility_blocked(ticket));
        let mut accessibility = blank_frame();
        let Frame::Screen(grid) = &mut accessibility;
        grid.rows[0][0].text = "accessibility".into();
        assert!(broker.publish_accessibility(next_ticket, "window".into(), accessibility));
        wt.send(
            &broker,
            MessageType::Frame,
            &testwire::frame(20, 1, 4, "terminal-suppressed"),
        );
        let current = source.frames.borrow_and_update();
        let Frame::Screen(grid) = &**current;
        assert_eq!(grid.rows[0][0].text, "accessibility");
    }

    #[test]
    fn returning_from_accessibility_requests_a_full_retained_terminal_frame() {
        let (broker, _source) = NativeBroker::new_hybrid();
        broker.foreground_changed(Some(77));
        let mut wt = Adapter::new(&broker, 9, Provider::WindowsTerminal);
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
        wt.send(&broker, MessageType::SourceUpdated, &update);
        wt.send(
            &broker,
            MessageType::Frame,
            &testwire::frame(20, 1, 1, "native"),
        );

        assert!(broker.foreground_changed(Some(999)).is_empty());
        let ticket = broker.accessibility_ticket().unwrap();
        assert!(broker.publish_accessibility(ticket, "window".into(), blank_frame()));

        let commands = broker.foreground_changed(Some(77));
        assert!(matches!(
            commands.as_slice(),
            [BrokerCommand::RequestFull {
                key: SourceKey {
                    process_nonce: 9,
                    source_id: 20
                },
                generation: 1
            }]
        ));
        // Metadata arriving before the requested frame must not flood the
        // adapter with duplicate full-frame requests.
        assert!(broker.foreground_changed(Some(77)).is_empty());
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
    fn repeated_true_metadata_cannot_steal_focus_back_from_a_new_tab() {
        let (broker, _source) = NativeBroker::new();
        broker.foreground_changed(Some(77));
        let mut wt = Adapter::new(&broker, 12, Provider::WindowsTerminal);
        for id in [1u64, 2] {
            wt.send(
                &broker,
                MessageType::SourceAdded,
                &testwire::source_added(id, 1, 77),
            );
        }
        let focused = |id: u64| {
            let mut update = Vec::new();
            update.extend_from_slice(&id.to_le_bytes());
            update.extend_from_slice(&1u64.to_le_bytes());
            update.push(12); // focused + visible
            update.push(1);
            update.push(1);
            update
        };
        wt.send(&broker, MessageType::SourceUpdated, &focused(1));
        wt.send(&broker, MessageType::SourceUpdated, &focused(2));
        assert_eq!(broker.selected().unwrap().source_id, 2);

        // Worker metadata is level-triggered, so an unrelated update from the
        // old engine can still report true before its loss edge is observed.
        // It is not a new focus event and must not advance its focus epoch.
        wt.send(&broker, MessageType::SourceUpdated, &focused(1));
        assert_eq!(broker.selected().unwrap().source_id, 2);
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
