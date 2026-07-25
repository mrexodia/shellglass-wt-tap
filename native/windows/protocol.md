# Native capture protocol v1

This is the byte-level contract implemented by `src/native_protocol.rs` and the
independent C++ mock adapter. All integers and IEEE-754 floats are little-endian.
There is no native alignment or padding.

## Envelope

Every message is:

| Field | Type |
|---|---:|
| magic (`SGNT`) | 4 bytes |
| protocol (`1`) | `u16` |
| message type | `u16` |
| payload length | `u32` |
| process nonce | `u64` |
| connection-local monotonically increasing sequence | `u64` |
| payload | `payload length` bytes |

Maximum payload is 16 MiB. Adapter connections start with exactly one `HELLO`.
The nonce cannot change on a connection and sequences must strictly increase.
Strings are strict UTF-8 encoded as `u16 byte_length, bytes`. Colors are one of:

```text
00                         default
01 index:u8                palette index
02 red:u8 green:u8 blue:u8 resolved RGB
```

A boolean is exactly `00` or `01`.

## Adapter → broker

Message type IDs are `HELLO=1`, `SOURCE_ADDED=2`, `SOURCE_UPDATED=3`,
`SOURCE_REMOVED=4`, `FRAME=5`, `IMAGE_BLOB=6`, `DIAGNOSTIC=7`.

### HELLO

```text
provider:u8                 1=Windows Terminal, 2=conhost
architecture:u8             1=x64, 2=ARM64
pid:u32
capabilities:u32
adapter_family:string
module_hash_count:u8
module_sha256[hash_count][32]
```

At most 16 module hashes and 256 bytes of adapter-family text are accepted.

### SOURCE_ADDED

```text
source_id:u64 generation:u64 owner_hwnd:u64
rows:u16 cols:u16 flags:u32 title:string
```

Flag bit 0 is focused and bit 1 visible. Dimensions are nonzero and capped at
500 rows × 1000 columns.

### SOURCE_UPDATED

```text
source_id:u64 generation:u64 mask:u8
if mask&01: owner_hwnd:u64
if mask&02: rows:u16 cols:u16
if mask&04: focused:bool
if mask&08: visible:bool
if mask&10: title:string
```

Unknown mask bits are invalid. `SOURCE_REMOVED` is simply
`source_id:u64 generation:u64`.

### FRAME

```text
source_id:u64 generation:u64 frame_sequence:u64
rows:u16 cols:u16 default_fg:color default_bg:color
style_count:u16
style[style_count]
link_count:u16
link[link_count]
row[rows]
cursor_visible:u8
if visible: cursor_row:u16 cursor_col:u16
cursor_style:u8
title:string
image_count:u16
image[image_count]
```

At least one and at most 4096 styles are allowed. A style is:

```text
fg:color bg:color flags:u16 underline_style:u8 ul_color:color link_id:u32
```

Style flag bits are bold `01`, dim `02`, italic `04`, strike `08`, concealed
`10`, blink `20`, inverse `40`; all others are invalid. Underline is 0–5.
`link_id=ffffffff` means none. Otherwise it must exist in the frame's link table.
A link is `id:u32 uri:string`; IDs cannot repeat and there are at most 4096.

Each row is:

```text
cell_count:u16
cell[cell_count] = column:u16 occupied_columns:u8 style_index:u16 text:string
```

Cells must be ordered, non-overlapping, start at column zero, and cover exactly
the announced width. `occupied_columns` is 1 or 2; a wide continuation is not
serialized. Empty text canonicalizes to one space. At most 500,000 stored cells
are accepted per frame. Cursor coordinates must lie inside the viewport and its
style is 0–6.

An image placement is:

```text
row:i16 col:u16 cols:f32 rows:f32 content_key[64 ASCII hex]
```

A NaN extent means absent/natural sizing; otherwise extents must be finite,
positive, and at most 10,000 cells. There are at most 1024 placements. Every
referenced blob must have arrived first on the same source generation.

### IMAGE_BLOB

```text
source_id:u64 generation:u64 content_key[64 ASCII hex]
mime:string byte_length:u32 bytes[byte_length]
```

MIME is one of PNG, JPEG, GIF, or WebP. Bytes are capped at 16 MiB and the broker
recomputes the shellglass content key over MIME + bytes; client-chosen mismatches
are rejected.

### DIAGNOSTIC

```text
has_source:u8 [source_id:u64] code:u16 text:string
```

Text is capped at 4096 bytes and neutered before broker-side logging.

## Broker → adapter

Type IDs are `SUBSCRIBE=0x101`, `UNSUBSCRIBE=0x102`,
`REQUEST_FULL=0x103`, `PING=0x104`, `SHUTDOWN=0x105`.

```text
SUBSCRIBE    source_id:u64 generation:u64 desired_max_fps:u16
UNSUBSCRIBE  source_id:u64 generation:u64
REQUEST_FULL source_id:u64 generation:u64
PING         empty
SHUTDOWN     empty
```

A stale generation never changes engine state. Subscription and request-full must
bypass ordinary pacing once and cause a complete viewport frame. Unsubscribe makes
the engine dormant; it must not retain large in-progress frame buffers.
