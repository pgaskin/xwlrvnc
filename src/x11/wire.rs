//! X11 wire-format framing: reading requests and writing replies/errors.
//!
//! We only support little-endian clients (the setup byte order is checked in
//! [`super::conn`]); on x86_64 Xlib always uses LSB-first, which matches
//! x11rb-protocol's native-endian (de)serialization.

use std::io::{self, Read};

use x11rb_protocol::x11_utils::Serialize;

/// A raw X11 request read off the wire (header fields + body bytes). `body` is
/// everything after the 4-byte (or 8-byte, for BIG-REQUESTS) header, which is
/// exactly what [`x11rb_protocol::protocol::Request::parse`] expects.
pub struct RawRequest {
    pub major_opcode: u8,
    pub minor_opcode: u8,
    pub remaining_length: u32,
    pub body: Vec<u8>,
}

/// Reads one request. Returns `Ok(None)` on a clean EOF at a request boundary.
pub fn read_request(r: &mut impl Read) -> io::Result<Option<RawRequest>> {
    let mut hdr = [0u8; 4];
    if !read_exact_or_eof(r, &mut hdr)? {
        return Ok(None);
    }
    let major_opcode = hdr[0];
    let minor_opcode = hdr[1];
    let short_len = u16::from_le_bytes([hdr[2], hdr[3]]);
    let remaining_length = if short_len == 0 {
        // BIG-REQUESTS: the real length follows as a u32 in 4-byte units,
        // including the now-2-unit header.
        let mut ext = [0u8; 4];
        r.read_exact(&mut ext)?;
        u32::from_le_bytes(ext).saturating_sub(2)
    } else {
        u32::from(short_len) - 1
    };
    let mut body = vec![0u8; remaining_length as usize * 4];
    r.read_exact(&mut body)?;
    Ok(Some(RawRequest {
        major_opcode,
        minor_opcode,
        remaining_length,
        body,
    }))
}

/// Like `read_exact`, but distinguishes a clean EOF (no bytes read) from a
/// truncated read.
fn read_exact_or_eof(r: &mut impl Read, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..])? {
            0 if filled == 0 => return Ok(false),
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            n => filled += n,
        }
    }
    Ok(true)
}

/// Serializes an x11rb reply, pads to the 32-byte wire minimum, and patches in
/// the length field. The sequence number at `[2..4]` is left zero and stamped by
/// the writer just before sending (see [`crate::event::Client::send_reply`]).
///
/// x11rb's reply `serialize` only emits the meaningful bytes (e.g. 12 for
/// `QueryExtension`); the real wire format is always at least 32 bytes with
/// `length` counting the extra 4-byte units beyond that. Patching `[4..8]` from
/// the final length works for every reply because that field is always the reply
/// length.
pub fn build_reply(reply: &impl Serialize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(32);
    reply.serialize_into(&mut buf);
    // Replies are at least 32 bytes, and their trailing variable data must be
    // padded to a 4-byte boundary (x11rb's serialize doesn't always do this).
    if buf.len() < 32 {
        buf.resize(32, 0);
    }
    let pad = (4 - buf.len() % 4) % 4;
    buf.resize(buf.len() + pad, 0);
    let length = ((buf.len() - 32) / 4) as u32;
    buf[4..8].copy_from_slice(&length.to_le_bytes());
    buf
}
