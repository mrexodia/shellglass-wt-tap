//! Versioned local IPC used by Windows render-tap adapters.
//!
//! The transport is a byte stream (a Windows named pipe in production). Every
//! message has the fixed little-endian envelope described in
//! `docs/windows-render-taps.md`; payloads use bounded length-prefixed fields and
//! contain no C++ objects or pointers. This module is platform-independent so the
//! decoder and broker can be fuzzed/tested on every CI host.

use crate::model::{Color, Frame, Grid, ImageBlob, ImagePlacement, StyledCell};
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

pub const MAGIC: [u8; 4] = *b"SGNT";
pub const PROTOCOL_VERSION: u16 = 1;
pub const ENVELOPE_LEN: usize = 28;
pub const MAX_PAYLOAD: usize = 16 * 1024 * 1024;
pub const MAX_ROWS: u16 = 500;
pub const MAX_COLS: u16 = 1_000;
pub const MAX_CELLS: usize = MAX_ROWS as usize * MAX_COLS as usize;
pub const MAX_STRING: usize = 64 * 1024;
pub const MAX_IMAGE: usize = 16 * 1024 * 1024;
pub const MAX_STYLES: usize = 4_096;
pub const MAX_LINKS: usize = 4_096;
pub const MAX_IMAGES: usize = 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum MessageType {
    Hello = 1,
    SourceAdded = 2,
    SourceUpdated = 3,
    SourceRemoved = 4,
    Frame = 5,
    ImageBlob = 6,
    Diagnostic = 7,
    Subscribe = 0x101,
    Unsubscribe = 0x102,
    RequestFull = 0x103,
    Ping = 0x104,
    Shutdown = 0x105,
}

impl TryFrom<u16> for MessageType {
    type Error = anyhow::Error;

    fn try_from(value: u16) -> Result<Self> {
        Ok(match value {
            1 => Self::Hello,
            2 => Self::SourceAdded,
            3 => Self::SourceUpdated,
            4 => Self::SourceRemoved,
            5 => Self::Frame,
            6 => Self::ImageBlob,
            7 => Self::Diagnostic,
            0x101 => Self::Subscribe,
            0x102 => Self::Unsubscribe,
            0x103 => Self::RequestFull,
            0x104 => Self::Ping,
            0x105 => Self::Shutdown,
            _ => bail!("unknown native message type {value}"),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    WindowsTerminal,
    Conhost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Architecture {
    X64,
    Arm64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub provider: Provider,
    pub pid: u32,
    pub architecture: Architecture,
    pub capabilities: u32,
    pub adapter_family: String,
    pub module_hashes: Vec<[u8; 32]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceAdded {
    pub source_id: u64,
    pub generation: u64,
    pub owner_hwnd: u64,
    pub rows: u16,
    pub cols: u16,
    pub flags: u32,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceUpdated {
    pub source_id: u64,
    pub generation: u64,
    pub owner_hwnd: Option<u64>,
    pub dimensions: Option<(u16, u16)>,
    pub focused: Option<bool>,
    pub visible: Option<bool>,
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRemoved {
    pub source_id: u64,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NativeFrame {
    pub source_id: u64,
    pub generation: u64,
    pub frame_sequence: u64,
    pub frame: Arc<Frame>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NativeImageBlob {
    pub source_id: u64,
    pub generation: u64,
    pub hash: String,
    pub blob: ImageBlob,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub source_id: Option<u64>,
    pub code: u16,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Hello(Hello),
    SourceAdded(SourceAdded),
    SourceUpdated(SourceUpdated),
    SourceRemoved(SourceRemoved),
    Frame(NativeFrame),
    ImageBlob(NativeImageBlob),
    Diagnostic(Diagnostic),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Subscribe {
        source_id: u64,
        generation: u64,
        max_fps: u16,
    },
    Unsubscribe {
        source_id: u64,
        generation: u64,
    },
    RequestFull {
        source_id: u64,
        generation: u64,
    },
    Ping,
    Shutdown,
}

/// Encode one broker-to-adapter command. Pipe workers supply a monotonically
/// increasing sequence and the destination adapter's process nonce.
pub fn encode_control(control: Control, process_nonce: u64, sequence: u64) -> Vec<u8> {
    let (kind, payload) = match control {
        Control::Subscribe {
            source_id,
            generation,
            max_fps,
        } => {
            let mut payload = Vec::with_capacity(18);
            payload.extend_from_slice(&source_id.to_le_bytes());
            payload.extend_from_slice(&generation.to_le_bytes());
            payload.extend_from_slice(&max_fps.to_le_bytes());
            (MessageType::Subscribe, payload)
        }
        Control::Unsubscribe {
            source_id,
            generation,
        } => {
            let mut payload = Vec::with_capacity(16);
            payload.extend_from_slice(&source_id.to_le_bytes());
            payload.extend_from_slice(&generation.to_le_bytes());
            (MessageType::Unsubscribe, payload)
        }
        Control::RequestFull {
            source_id,
            generation,
        } => {
            let mut payload = Vec::with_capacity(16);
            payload.extend_from_slice(&source_id.to_le_bytes());
            payload.extend_from_slice(&generation.to_le_bytes());
            (MessageType::RequestFull, payload)
        }
        Control::Ping => (MessageType::Ping, Vec::new()),
        Control::Shutdown => (MessageType::Shutdown, Vec::new()),
    };
    encode_packet(kind, process_nonce, sequence, &payload)
}

fn encode_packet(kind: MessageType, process_nonce: u64, sequence: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ENVELOPE_LEN + payload.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    out.extend_from_slice(&(kind as u16).to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&process_nonce.to_le_bytes());
    out.extend_from_slice(&sequence.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

#[derive(Debug, Clone, PartialEq)]
pub struct Packet {
    pub process_nonce: u64,
    pub sequence: u64,
    pub message: Message,
}

/// Incremental envelope decoder. A connection has one nonce and strictly
/// increasing sequence numbers; its first message must be `HELLO`.
#[derive(Default)]
pub struct Decoder {
    buffer: Vec<u8>,
    nonce: Option<u64>,
    sequence: Option<u64>,
    saw_hello: bool,
}

impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Packet>> {
        if self.buffer.len().saturating_add(bytes.len()) > MAX_PAYLOAD + ENVELOPE_LEN {
            bail!("native IPC receive buffer exceeds limit");
        }
        self.buffer.extend_from_slice(bytes);
        let mut packets = Vec::new();
        loop {
            if self.buffer.len() < ENVELOPE_LEN {
                break;
            }
            if self.buffer[..4] != MAGIC {
                bail!("bad native IPC magic");
            }
            let protocol = u16::from_le_bytes([self.buffer[4], self.buffer[5]]);
            if protocol != PROTOCOL_VERSION {
                bail!("unsupported native IPC protocol {protocol}");
            }
            let kind = MessageType::try_from(u16::from_le_bytes([self.buffer[6], self.buffer[7]]))?;
            let payload_len = u32::from_le_bytes(self.buffer[8..12].try_into().unwrap()) as usize;
            if payload_len > MAX_PAYLOAD {
                bail!("native IPC payload exceeds limit");
            }
            let total = ENVELOPE_LEN
                .checked_add(payload_len)
                .context("native IPC length overflow")?;
            if self.buffer.len() < total {
                break;
            }
            let nonce = u64::from_le_bytes(self.buffer[12..20].try_into().unwrap());
            let sequence = u64::from_le_bytes(self.buffer[20..28].try_into().unwrap());
            if let Some(expected) = self.nonce {
                if nonce != expected {
                    bail!("native IPC process nonce changed");
                }
            } else {
                self.nonce = Some(nonce);
            }
            if self.sequence.is_some_and(|last| sequence <= last) {
                bail!("native IPC sequence regressed");
            }
            if !self.saw_hello && kind != MessageType::Hello {
                bail!("native IPC connection did not start with HELLO");
            }
            if self.saw_hello && kind == MessageType::Hello {
                bail!("duplicate native IPC HELLO");
            }
            let message = decode_payload(kind, &self.buffer[ENVELOPE_LEN..total])?;
            self.saw_hello = true;
            self.sequence = Some(sequence);
            packets.push(Packet {
                process_nonce: nonce,
                sequence,
                message,
            });
            self.buffer.drain(..total);
        }
        Ok(packets)
    }
}

fn decode_payload(kind: MessageType, payload: &[u8]) -> Result<Message> {
    let mut r = Reader::new(payload);
    let message = match kind {
        MessageType::Hello => Message::Hello(decode_hello(&mut r)?),
        MessageType::SourceAdded => Message::SourceAdded(decode_source_added(&mut r)?),
        MessageType::SourceUpdated => Message::SourceUpdated(decode_source_updated(&mut r)?),
        MessageType::SourceRemoved => Message::SourceRemoved(SourceRemoved {
            source_id: r.u64()?,
            generation: r.u64()?,
        }),
        MessageType::Frame => Message::Frame(decode_frame(&mut r)?),
        MessageType::ImageBlob => Message::ImageBlob(decode_image(&mut r)?),
        MessageType::Diagnostic => Message::Diagnostic(Diagnostic {
            source_id: match r.u8()? {
                0 => None,
                1 => Some(r.u64()?),
                _ => bail!("invalid diagnostic source presence"),
            },
            code: r.u16()?,
            text: r.string_u16(4_096)?,
        }),
        _ => bail!("broker command received on adapter input"),
    };
    r.finish()?;
    Ok(message)
}

fn decode_hello(r: &mut Reader<'_>) -> Result<Hello> {
    let provider = match r.u8()? {
        1 => Provider::WindowsTerminal,
        2 => Provider::Conhost,
        _ => bail!("invalid native provider"),
    };
    let architecture = match r.u8()? {
        1 => Architecture::X64,
        2 => Architecture::Arm64,
        _ => bail!("invalid native architecture"),
    };
    let pid = r.u32()?;
    let capabilities = r.u32()?;
    let adapter_family = r.string_u16(256)?;
    let count = r.u8()? as usize;
    if count > 16 {
        bail!("too many native module hashes");
    }
    let mut module_hashes = Vec::with_capacity(count);
    for _ in 0..count {
        module_hashes.push(r.array()?);
    }
    Ok(Hello {
        provider,
        pid,
        architecture,
        capabilities,
        adapter_family,
        module_hashes,
    })
}

fn dimensions(rows: u16, cols: u16) -> Result<()> {
    if rows == 0 || cols == 0 || rows > MAX_ROWS || cols > MAX_COLS {
        bail!("impossible native grid dimensions {cols}x{rows}");
    }
    Ok(())
}

fn decode_source_added(r: &mut Reader<'_>) -> Result<SourceAdded> {
    let source_id = r.u64()?;
    let generation = r.u64()?;
    let owner_hwnd = r.u64()?;
    let rows = r.u16()?;
    let cols = r.u16()?;
    dimensions(rows, cols)?;
    let flags = r.u32()?;
    let title = r.string_u16(4_096)?;
    Ok(SourceAdded {
        source_id,
        generation,
        owner_hwnd,
        rows,
        cols,
        flags,
        title,
    })
}

fn decode_source_updated(r: &mut Reader<'_>) -> Result<SourceUpdated> {
    let source_id = r.u64()?;
    let generation = r.u64()?;
    let mask = r.u8()?;
    if mask & !0x1f != 0 {
        bail!("unknown SOURCE_UPDATED field mask");
    }
    let owner_hwnd = (mask & 1 != 0).then(|| r.u64()).transpose()?;
    let dimensions = if mask & 2 != 0 {
        let rows = r.u16()?;
        let cols = r.u16()?;
        self::dimensions(rows, cols)?;
        Some((rows, cols))
    } else {
        None
    };
    let focused = (mask & 4 != 0).then(|| r.bool()).transpose()?;
    let visible = (mask & 8 != 0).then(|| r.bool()).transpose()?;
    let title = (mask & 16 != 0).then(|| r.string_u16(4_096)).transpose()?;
    Ok(SourceUpdated {
        source_id,
        generation,
        owner_hwnd,
        dimensions,
        focused,
        visible,
        title,
    })
}

#[derive(Clone)]
struct Style {
    fg: Color,
    bg: Color,
    bold: bool,
    dim: bool,
    italic: bool,
    strike: bool,
    concealed: bool,
    blink: bool,
    inverse: bool,
    underline: u8,
    ulcolor: Color,
    link: Option<u32>,
}

fn decode_frame(r: &mut Reader<'_>) -> Result<NativeFrame> {
    let source_id = r.u64()?;
    let generation = r.u64()?;
    let frame_sequence = r.u64()?;
    let rows = r.u16()?;
    let cols = r.u16()?;
    dimensions(rows, cols)?;
    let default_colors = (r.color()?, r.color()?);

    let style_count = r.u16()? as usize;
    if style_count == 0 || style_count > MAX_STYLES {
        bail!("invalid native style table length");
    }
    let mut styles = Vec::with_capacity(style_count);
    for _ in 0..style_count {
        let fg = r.color()?;
        let bg = r.color()?;
        let flags = r.u16()?;
        if flags & !0x7f != 0 {
            bail!("unknown native style flags");
        }
        let underline = r.u8()?;
        if underline > 5 {
            bail!("invalid underline style");
        }
        let ulcolor = r.color()?;
        let raw_link = r.u32()?;
        styles.push(Style {
            fg,
            bg,
            bold: flags & 1 != 0,
            dim: flags & 2 != 0,
            italic: flags & 4 != 0,
            strike: flags & 8 != 0,
            concealed: flags & 16 != 0,
            blink: flags & 32 != 0,
            inverse: flags & 64 != 0,
            underline,
            ulcolor,
            link: (raw_link != u32::MAX).then_some(raw_link),
        });
    }

    let link_count = r.u16()? as usize;
    if link_count > MAX_LINKS {
        bail!("native link table exceeds limit");
    }
    let mut links = BTreeMap::new();
    for _ in 0..link_count {
        let id = r.u32()?;
        let uri = r.string_u16(8_192)?;
        if links.insert(id, uri).is_some() {
            bail!("duplicate native link id");
        }
    }
    if styles
        .iter()
        .filter_map(|s| s.link)
        .any(|id| !links.contains_key(&id))
    {
        bail!("native style references an unknown link");
    }

    let mut grid_rows = Vec::with_capacity(rows as usize);
    let mut total_cells = 0usize;
    for _ in 0..rows {
        let count = r.u16()? as usize;
        if count > cols as usize {
            bail!("native row has too many cells");
        }
        total_cells = total_cells
            .checked_add(count)
            .context("native cell count overflow")?;
        if total_cells > MAX_CELLS {
            bail!("native frame has too many cells");
        }
        let mut row = Vec::with_capacity(count);
        let mut expected_col = 0u16;
        for _ in 0..count {
            let col = r.u16()?;
            if col != expected_col {
                bail!("native row is sparse, overlapping, or out of order");
            }
            let columns = r.u8()?;
            if columns != 1 && columns != 2 {
                bail!("native cell occupies an invalid column count");
            }
            expected_col = expected_col
                .checked_add(u16::from(columns))
                .context("native row width overflow")?;
            if expected_col > cols {
                bail!("native cell extends beyond the row");
            }
            let style_id = r.u16()? as usize;
            let style = styles
                .get(style_id)
                .context("native cell references an unknown style")?;
            let mut text = r.string_u16(MAX_STRING)?;
            if text.is_empty() {
                text.push(' ');
            }
            row.push(StyledCell {
                text,
                fg: style.fg,
                bg: style.bg,
                bold: style.bold,
                dim: style.dim,
                italic: style.italic,
                underline: style.underline,
                strike: style.strike,
                concealed: style.concealed,
                blink: style.blink,
                ulcolor: style.ulcolor,
                inverse: style.inverse,
                link: style.link,
                wide: columns == 2,
            });
        }
        if expected_col != cols {
            bail!("native row does not cover the full viewport");
        }
        grid_rows.push(row);
    }

    let cursor = match r.u8()? {
        0 => None,
        1 => {
            let row = r.u16()?;
            let col = r.u16()?;
            if row >= rows || col >= cols {
                bail!("native cursor is outside the viewport");
            }
            Some((row, col))
        }
        _ => bail!("invalid native cursor visibility"),
    };
    let cursor_style = r.u8()?;
    if cursor_style > 6 {
        bail!("invalid native cursor style");
    }
    let title = r.string_u16(4_096)?;

    let image_count = r.u16()? as usize;
    if image_count > MAX_IMAGES {
        bail!("too many native image placements");
    }
    let mut images = Vec::with_capacity(image_count);
    for _ in 0..image_count {
        let row = r.i16()?;
        let col = r.u16()?;
        let image_cols = r.f32_option()?;
        let image_rows = r.f32_option()?;
        let hash_bytes: [u8; 64] = r.array()?;
        if !hash_bytes.iter().all(u8::is_ascii_hexdigit) {
            bail!("invalid native image content key");
        }
        let hash = String::from_utf8(hash_bytes.to_vec())
            .unwrap()
            .to_ascii_lowercase();
        images.push(ImagePlacement {
            row,
            col,
            cols: image_cols,
            rows: image_rows,
            hash,
        });
    }

    Ok(NativeFrame {
        source_id,
        generation,
        frame_sequence,
        frame: Arc::new(Frame::Screen(Grid {
            source_epoch: 0,
            cols,
            rows: grid_rows,
            cursor,
            cursor_style,
            default_colors,
            title,
            links,
            images,
            image_data: HashMap::new(),
        })),
    })
}

fn decode_image(r: &mut Reader<'_>) -> Result<NativeImageBlob> {
    let source_id = r.u64()?;
    let generation = r.u64()?;
    let hash_bytes: [u8; 64] = r.array()?;
    if !hash_bytes.iter().all(u8::is_ascii_hexdigit) {
        bail!("invalid native image content key");
    }
    let hash = String::from_utf8(hash_bytes.to_vec())
        .unwrap()
        .to_ascii_lowercase();
    let mime = r.string_u16(128)?;
    if !matches!(
        mime.as_str(),
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    ) {
        bail!("unsupported native image MIME type");
    }
    let length = r.u32()? as usize;
    if length > MAX_IMAGE {
        bail!("native image exceeds limit");
    }
    let bytes = Bytes::copy_from_slice(r.take(length)?);
    if crate::proto::content_key(&mime, &bytes) != hash {
        bail!("native image content key mismatch");
    }
    Ok(NativeImageBlob {
        source_id,
        generation,
        hash,
        blob: ImageBlob { mime, bytes },
    })
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(len).context("native field overflow")?;
        let value = self
            .bytes
            .get(self.at..end)
            .context("truncated native IPC payload")?;
        self.at = end;
        Ok(value)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().unwrap())
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn bool(&mut self) -> Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => bail!("invalid native boolean"),
        }
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn i16(&mut self) -> Result<i16> {
        Ok(i16::from_le_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    fn color(&mut self) -> Result<Color> {
        Ok(match self.u8()? {
            0 => Color::Default,
            1 => Color::Idx(self.u8()?),
            2 => Color::Rgb(self.u8()?, self.u8()?, self.u8()?),
            _ => bail!("invalid native color encoding"),
        })
    }

    fn f32_option(&mut self) -> Result<Option<f32>> {
        let value = f32::from_bits(self.u32()?);
        if value.is_nan() {
            Ok(None)
        } else if value.is_finite() && value > 0.0 && value <= 10_000.0 {
            Ok(Some(value))
        } else {
            bail!("invalid native image extent")
        }
    }

    fn string_u16(&mut self, max: usize) -> Result<String> {
        let len = self.u16()? as usize;
        if len > max {
            bail!("native string exceeds limit");
        }
        String::from_utf8(self.take(len)?.to_vec()).context("native string is not valid UTF-8")
    }

    fn finish(&self) -> Result<()> {
        if self.at != self.bytes.len() {
            bail!("trailing bytes in native IPC payload");
        }
        Ok(())
    }
}

/// Test/mock-adapter helpers. Production adapters implement the same simple
/// little-endian layout in native code.
#[cfg(test)]
pub(crate) mod testwire {
    use super::*;

    pub fn packet(kind: MessageType, nonce: u64, sequence: u64, payload: &[u8]) -> Vec<u8> {
        encode_packet(kind, nonce, sequence, payload)
    }

    pub fn string(out: &mut Vec<u8>, value: &str) {
        out.extend_from_slice(&(value.len() as u16).to_le_bytes());
        out.extend_from_slice(value.as_bytes());
    }

    pub fn hello(provider: Provider) -> Vec<u8> {
        let mut p = vec![
            match provider {
                Provider::WindowsTerminal => 1,
                Provider::Conhost => 2,
            },
            1,
        ];
        p.extend_from_slice(&123u32.to_le_bytes());
        p.extend_from_slice(&0u32.to_le_bytes());
        string(&mut p, "test");
        p.push(0);
        p
    }

    pub fn source_added(id: u64, generation: u64, hwnd: u64) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&id.to_le_bytes());
        p.extend_from_slice(&generation.to_le_bytes());
        p.extend_from_slice(&hwnd.to_le_bytes());
        p.extend_from_slice(&1u16.to_le_bytes());
        p.extend_from_slice(&1u16.to_le_bytes());
        p.extend_from_slice(&0u32.to_le_bytes());
        string(&mut p, "test");
        p
    }

    pub fn rich_frame(id: u64, generation: u64, frame_sequence: u64) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&id.to_le_bytes());
        p.extend_from_slice(&generation.to_le_bytes());
        p.extend_from_slice(&frame_sequence.to_le_bytes());
        p.extend_from_slice(&1u16.to_le_bytes());
        p.extend_from_slice(&3u16.to_le_bytes());
        p.extend_from_slice(&[2, 10, 20, 30]); // default fg RGB
        p.extend_from_slice(&[1, 4]); // default bg index
        p.extend_from_slice(&2u16.to_le_bytes());
        // Style zero: defaults.
        p.extend_from_slice(&[0, 0]);
        p.extend_from_slice(&0u16.to_le_bytes());
        p.extend_from_slice(&[0, 0]);
        p.extend_from_slice(&u32::MAX.to_le_bytes());
        // Style one: RGB/index, every boolean, curly + RGB underline, link 7.
        p.extend_from_slice(&[2, 1, 2, 3]);
        p.extend_from_slice(&[1, 5]);
        p.extend_from_slice(&0x7fu16.to_le_bytes());
        p.push(3);
        p.extend_from_slice(&[2, 4, 5, 6]);
        p.extend_from_slice(&7u32.to_le_bytes());
        p.extend_from_slice(&1u16.to_le_bytes());
        p.extend_from_slice(&7u32.to_le_bytes());
        string(&mut p, "https://example.test/path");
        p.extend_from_slice(&2u16.to_le_bytes()); // two stored cells, three columns
        p.extend_from_slice(&0u16.to_le_bytes());
        p.push(2);
        p.extend_from_slice(&1u16.to_le_bytes());
        string(&mut p, "e\u{301}");
        p.extend_from_slice(&2u16.to_le_bytes());
        p.push(1);
        p.extend_from_slice(&0u16.to_le_bytes());
        string(&mut p, "");
        p.push(1);
        p.extend_from_slice(&0u16.to_le_bytes());
        p.extend_from_slice(&2u16.to_le_bytes());
        p.push(6);
        string(&mut p, "rich title");
        p.extend_from_slice(&0u16.to_le_bytes());
        p
    }

    pub fn frame(id: u64, generation: u64, frame_sequence: u64, text: &str) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&id.to_le_bytes());
        p.extend_from_slice(&generation.to_le_bytes());
        p.extend_from_slice(&frame_sequence.to_le_bytes());
        p.extend_from_slice(&1u16.to_le_bytes());
        p.extend_from_slice(&1u16.to_le_bytes());
        p.push(0); // default fg
        p.push(0); // default bg
        p.extend_from_slice(&1u16.to_le_bytes()); // styles
        p.push(0); // fg
        p.push(0); // bg
        p.extend_from_slice(&0u16.to_le_bytes()); // flags
        p.push(0); // underline
        p.push(0); // ul color
        p.extend_from_slice(&u32::MAX.to_le_bytes());
        p.extend_from_slice(&0u16.to_le_bytes()); // links
        p.extend_from_slice(&1u16.to_le_bytes()); // row cell count
        p.extend_from_slice(&0u16.to_le_bytes()); // col
        p.push(1); // columns
        p.extend_from_slice(&0u16.to_le_bytes()); // style id
        string(&mut p, text);
        p.push(0); // cursor hidden
        p.push(0); // cursor style
        string(&mut p, "title");
        p.extend_from_slice(&0u16.to_le_bytes()); // images
        p
    }
}

#[cfg(test)]
mod tests {
    use super::testwire::*;
    use super::*;

    #[test]
    fn incremental_decoder_reassembles_and_converts_a_frame() {
        let mut bytes = packet(MessageType::Hello, 9, 1, &hello(Provider::WindowsTerminal));
        bytes.extend(packet(MessageType::Frame, 9, 2, &frame(7, 3, 1, "x")));
        let split = bytes.len() / 2;
        let mut decoder = Decoder::default();
        let mut got = decoder.push(&bytes[..split]).unwrap();
        got.extend(decoder.push(&bytes[split..]).unwrap());
        assert_eq!(got.len(), 2);
        let Message::Frame(frame) = &got[1].message else {
            panic!("expected frame")
        };
        let Frame::Screen(grid) = &*frame.frame;
        assert_eq!(grid.rows[0][0].text, "x");
    }

    #[test]
    fn native_to_grid_preserves_graphemes_wide_styles_links_cursor_title_and_defaults() {
        let packets = decoder_after_hello(MessageType::Frame, &rich_frame(7, 3, 9)).unwrap();
        let Message::Frame(native) = &packets[0].message else {
            panic!("expected frame")
        };
        let Frame::Screen(grid) = &*native.frame;
        assert_eq!(grid.cols, 3);
        assert_eq!(grid.rows[0].len(), 2);
        let styled = &grid.rows[0][0];
        assert_eq!(styled.text, "e\u{301}");
        assert!(styled.wide && styled.bold && styled.dim && styled.italic);
        assert!(styled.strike && styled.concealed && styled.blink && styled.inverse);
        assert_eq!(styled.underline, 3);
        assert_eq!(styled.fg, Color::Rgb(1, 2, 3));
        assert_eq!(styled.bg, Color::Idx(5));
        assert_eq!(styled.ulcolor, Color::Rgb(4, 5, 6));
        assert_eq!(styled.link, Some(7));
        assert_eq!(grid.rows[0][1].text, " ");
        assert_eq!(grid.links[&7], "https://example.test/path");
        assert_eq!(grid.cursor, Some((0, 2)));
        assert_eq!(grid.cursor_style, 6);
        assert_eq!(grid.title, "rich title");
        assert_eq!(grid.default_colors, (Color::Rgb(10, 20, 30), Color::Idx(4)));
    }

    fn decoder_after_hello(kind: MessageType, payload: &[u8]) -> Result<Vec<Packet>> {
        let mut decoder = Decoder::default();
        decoder.push(&packet(
            MessageType::Hello,
            44,
            1,
            &hello(Provider::Conhost),
        ))?;
        decoder.push(&packet(kind, 44, 2, payload))
    }

    #[test]
    fn decoder_rejects_invalid_cells_styles_links_cursor_and_dimensions() {
        let valid = frame(1, 1, 1, "x");

        let mut invalid_columns = valid.clone();
        invalid_columns[48] = 3;
        assert!(decoder_after_hello(MessageType::Frame, &invalid_columns).is_err());

        let mut unknown_style = valid.clone();
        unknown_style[49..51].copy_from_slice(&1u16.to_le_bytes());
        assert!(decoder_after_hello(MessageType::Frame, &unknown_style).is_err());

        let mut no_styles = valid.clone();
        no_styles[30..32].copy_from_slice(&0u16.to_le_bytes());
        assert!(decoder_after_hello(MessageType::Frame, &no_styles).is_err());

        let mut duplicate_links = valid.clone();
        let mut links = Vec::new();
        for _ in 0..2 {
            links.extend_from_slice(&7u32.to_le_bytes());
            string(&mut links, "https://example.test");
        }
        duplicate_links.splice(44..44, links);
        duplicate_links[42..44].copy_from_slice(&2u16.to_le_bytes());
        assert!(decoder_after_hello(MessageType::Frame, &duplicate_links).is_err());

        let mut cursor_outside = valid.clone();
        cursor_outside[54] = 1;
        cursor_outside.splice(55..55, [1, 0, 0, 0]); // row 1, col 0 on a 1-row grid
        assert!(decoder_after_hello(MessageType::Frame, &cursor_outside).is_err());

        let mut impossible = source_added(1, 1, 1);
        impossible[24..26].copy_from_slice(&0u16.to_le_bytes());
        assert!(decoder_after_hello(MessageType::SourceAdded, &impossible).is_err());
    }

    #[test]
    fn valid_content_addressed_image_blob_decodes() {
        let bytes = b"fixture-png-bytes";
        let hash = crate::proto::content_key("image/png", bytes);
        let mut image = Vec::new();
        image.extend_from_slice(&1u64.to_le_bytes());
        image.extend_from_slice(&2u64.to_le_bytes());
        image.extend_from_slice(hash.as_bytes());
        string(&mut image, "image/png");
        image.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        image.extend_from_slice(bytes);
        let packets = decoder_after_hello(MessageType::ImageBlob, &image).unwrap();
        let Message::ImageBlob(image) = &packets[0].message else {
            panic!("expected image")
        };
        assert_eq!(image.hash, hash);
        assert_eq!(&image.blob.bytes[..], bytes);
    }

    #[test]
    fn decoder_rejects_oversized_and_mismatched_images_without_allocating_them() {
        let mut image = Vec::new();
        image.extend_from_slice(&1u64.to_le_bytes());
        image.extend_from_slice(&1u64.to_le_bytes());
        image.extend_from_slice(&[b'0'; 64]);
        string(&mut image, "image/png");
        image.extend_from_slice(&((MAX_IMAGE + 1) as u32).to_le_bytes());
        assert!(decoder_after_hello(MessageType::ImageBlob, &image).is_err());

        let length_at = image.len() - 4;
        image[length_at..].copy_from_slice(&1u32.to_le_bytes());
        image.push(0);
        assert!(
            decoder_after_hello(MessageType::ImageBlob, &image).is_err(),
            "a client-chosen content key must not authenticate different bytes"
        );
    }

    #[test]
    fn decoder_rejects_bad_order_version_size_utf8_and_sparse_rows() {
        let mut decoder = Decoder::default();
        assert!(
            decoder
                .push(&packet(
                    MessageType::SourceAdded,
                    1,
                    1,
                    &source_added(1, 1, 1)
                ))
                .is_err()
        );

        let mut bad_version = packet(MessageType::Hello, 1, 1, &hello(Provider::Conhost));
        bad_version[4..6].copy_from_slice(&99u16.to_le_bytes());
        assert!(Decoder::default().push(&bad_version).is_err());

        let mut oversized = packet(MessageType::Hello, 1, 1, &[]);
        oversized[8..12].copy_from_slice(&((MAX_PAYLOAD + 1) as u32).to_le_bytes());
        assert!(Decoder::default().push(&oversized).is_err());

        let mut bad_utf8 = hello(Provider::Conhost);
        // adapter family starts after provider, arch, pid, caps, length.
        bad_utf8[12] = 0xff;
        assert!(
            Decoder::default()
                .push(&packet(MessageType::Hello, 1, 1, &bad_utf8))
                .is_err()
        );

        let hello_packet = packet(MessageType::Hello, 1, 1, &hello(Provider::Conhost));
        let mut decoder = Decoder::default();
        decoder.push(&hello_packet).unwrap();
        assert!(decoder.push(&hello_packet).is_err());

        let mut sparse = frame(1, 1, 1, "x");
        // First cell column: fixed frame prefix (28), colors (2), style table
        // count/style (12), link count (2), row count (2).
        let col_offset = 28 + 2 + 12 + 2 + 2;
        sparse[col_offset..col_offset + 2].copy_from_slice(&1u16.to_le_bytes());
        let mut decoder = Decoder::default();
        decoder
            .push(&packet(MessageType::Hello, 2, 1, &hello(Provider::Conhost)))
            .unwrap();
        assert!(
            decoder
                .push(&packet(MessageType::Frame, 2, 2, &sparse))
                .is_err()
        );
    }
}
