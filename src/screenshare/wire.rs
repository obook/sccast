//! wire.rs
//! Byte layout of the panel protocol: command frames (varint length, then a small protobuf
//! message with a JSON payload) and the headers of the video and audio frames.
//! See docs/screenshare.md.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::io::{self, Read, Write};

/// Larger command frames are taken as a broken stream.
const MAX_MESSAGE: u64 = 1 << 20;

/// One command frame. `payload` is JSON.
#[derive(Debug, Default, PartialEq)]
pub struct ScreenshareMessage {
    pub version: i32,
    pub command_type: i32,
    pub command_id: i32,
    pub sequence: i32,
    pub timestamp: i64,
    pub payload: String,
}

fn bad(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why.to_string())
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Varint from `input`, one byte at a time: works on a socket and on a slice.
fn read_varint(input: &mut impl Read) -> io::Result<u64> {
    let mut v = 0;
    for shift in (0..64).step_by(7) {
        let mut byte = [0];
        input.read_exact(&mut byte)?;
        v |= u64::from(byte[0] & 0x7f) << shift;
        if byte[0] < 0x80 {
            return Ok(v);
        }
    }
    Err(bad("varint too long"))
}

fn skip(input: &mut &[u8], n: u64) -> io::Result<()> {
    let n = usize::try_from(n).ok().filter(|n| *n <= input.len()).ok_or_else(|| bad("field past the end"))?;
    *input = &input[n..];
    Ok(())
}

impl ScreenshareMessage {
    /// Protobuf body, without the length. Zero fields are left out, as proto3 does.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let numbers = [
            (1, i64::from(self.version)),
            (2, self.command_type.into()),
            (3, self.command_id.into()),
            (4, self.sequence.into()),
            (5, self.timestamp),
        ];
        for (field, v) in numbers.into_iter().filter(|(_, v)| *v != 0) {
            put_varint(&mut out, field << 3);
            // A negative int32 takes 10 bytes, sign-extended, as in protobuf.
            put_varint(&mut out, v as u64);
        }
        if !self.payload.is_empty() {
            put_varint(&mut out, 6 << 3 | 2);
            put_varint(&mut out, self.payload.len() as u64);
            out.extend_from_slice(self.payload.as_bytes());
        }
        out
    }

    /// Message from a protobuf body. Unknown fields are skipped.
    pub fn decode(mut input: &[u8]) -> io::Result<Self> {
        let mut m = Self::default();
        while !input.is_empty() {
            let key = read_varint(&mut input)?;
            match (key >> 3, key & 7) {
                (6, 2) => {
                    let len = read_varint(&mut input)?;
                    let rest = input;
                    skip(&mut input, len)?;
                    let text = &rest[..rest.len() - input.len()];
                    m.payload = String::from_utf8(text.to_vec()).map_err(|_| bad("payload is not UTF-8"))?;
                }
                (field, 0) => {
                    let v = read_varint(&mut input)?;
                    match field {
                        1 => m.version = v as i32,
                        2 => m.command_type = v as i32,
                        3 => m.command_id = v as i32,
                        4 => m.sequence = v as i32,
                        5 => m.timestamp = v as i64,
                        _ => {}
                    }
                }
                (_, 1) => skip(&mut input, 8)?,
                (_, 2) => {
                    let len = read_varint(&mut input)?;
                    skip(&mut input, len)?;
                }
                (_, 5) => skip(&mut input, 4)?,
                _ => return Err(bad("unknown protobuf wire type")),
            }
        }
        Ok(m)
    }
}

/// Write `message` with its varint length, in one write.
pub fn write_message(output: &mut impl Write, message: &ScreenshareMessage) -> io::Result<()> {
    let body = message.encode();
    let mut frame = Vec::with_capacity(body.len() + 5);
    put_varint(&mut frame, body.len() as u64);
    frame.extend(body);
    output.write_all(&frame)
}

/// Read one length-prefixed message.
pub fn read_message(input: &mut impl Read) -> io::Result<ScreenshareMessage> {
    let len = read_varint(input)?;
    if len > MAX_MESSAGE {
        return Err(bad("command frame too large"));
    }
    let mut body = vec![0; len as usize];
    input.read_exact(&mut body)?;
    ScreenshareMessage::decode(&body)
}

/// Video frame (`index` Some) or audio frame (`index` None), header and data, big endian.
pub fn media_frame(index: Option<i32>, kind: i32, data: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(data.len() + 12);
    frame.extend((data.len() as i32).to_be_bytes());
    if let Some(index) = index {
        frame.extend(index.to_be_bytes());
    }
    frame.extend(kind.to_be_bytes());
    frame.extend_from_slice(data);
    frame
}

/// Split an Annex B access unit as the Android sender sends it: each SPS (7) and PPS (8)
/// alone, the other NAL units together. Start codes stay in the parts.
pub fn split_config(unit: &[u8]) -> Vec<&[u8]> {
    // Start of each NAL unit, at its start code (3 or 4 bytes).
    let mut starts: Vec<usize> = (0..unit.len().saturating_sub(2))
        .filter(|&i| unit[i..i + 3] == [0, 0, 1])
        .map(|i| if i > 0 && unit[i - 1] == 0 { i - 1 } else { i })
        .collect();
    if starts.first() != Some(&0) {
        starts.insert(0, 0);
    }
    let mut parts: Vec<(usize, usize, bool)> = Vec::new();
    for (k, &begin) in starts.iter().enumerate() {
        let end = starts.get(k + 1).copied().unwrap_or(unit.len());
        let header = unit[begin..end].iter().position(|&b| b == 1).and_then(|p| unit.get(begin + p + 1));
        let config = matches!(header.map(|h| h & 0x1f), Some(7 | 8));
        match parts.last_mut() {
            Some((_, last_end, false)) if !config => *last_end = end,
            _ => parts.push((begin, end, config)),
        }
    }
    parts.into_iter().map(|(begin, end, _)| &unit[begin..end]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_a_cast_request_frame() {
        let m = ScreenshareMessage { version: 1, command_type: 6, command_id: 2, payload: "{}".into(), ..Default::default() };
        let mut frame = Vec::new();
        write_message(&mut frame, &m).unwrap();
        assert_eq!(frame, [10, 0x08, 1, 0x10, 6, 0x18, 2, 0x32, 2, b'{', b'}']);
        assert_eq!(read_message(&mut frame.as_slice()).unwrap(), m);
    }

    #[test]
    fn round_trips_large_and_negative_values() {
        let m = ScreenshareMessage {
            version: 76,
            command_type: 1,
            command_id: 101,
            sequence: -1,
            timestamp: 1_792_000_000_000,
            payload: "{\"resultType\":1}".into(),
        };
        assert_eq!(ScreenshareMessage::decode(&m.encode()).unwrap(), m);
    }

    #[test]
    fn skips_unknown_fields_and_rejects_truncation() {
        // Field 7 (varint 5), field 8 (string "ab"), then commandId 3.
        let body = [0x38, 5, 0x42, 2, b'a', b'b', 0x18, 3];
        assert_eq!(ScreenshareMessage::decode(&body).unwrap().command_id, 3);
        assert!(ScreenshareMessage::decode(&[0x32, 5, b'a']).is_err());
    }

    #[test]
    fn builds_media_headers() {
        assert_eq!(media_frame(Some(5), 0, &[9, 9, 9]), [0, 0, 0, 3, 0, 0, 0, 5, 0, 0, 0, 0, 9, 9, 9]);
        assert_eq!(media_frame(None, 258, &[7]), [0, 0, 0, 1, 0, 0, 1, 2, 7]);
    }

    #[test]
    fn splits_sps_and_pps_from_a_key_frame() {
        let sps = [0, 0, 0, 1, 0x67, 1, 2];
        let pps = [0, 0, 0, 1, 0x68, 3];
        let rest = [0, 0, 1, 0x09, 0xf0, 0, 0, 0, 1, 0x65, 4, 5];
        let unit = [&sps[..], &pps, &rest].concat();
        assert_eq!(split_config(&unit), [&sps[..], &pps, &rest]);
        let delta = [0, 0, 0, 1, 0x41, 6];
        assert_eq!(split_config(&delta), [&delta[..]]);
    }
}
