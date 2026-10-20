//! rtp.rs
//! Cast Streaming wire format on the sender side: encrypted RTP frames and Sender Reports.
//! RTP carries the frames over UDP; RTCP carries the reports both ways (see rtcp.rs).
//! From openscreen/cast/streaming/impl: rtp_packetizer.cc, frame_crypto.cc,
//! sender_report_builder.cc.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::collections::VecDeque;
use std::io::Read;
use std::time::{Duration, Instant};

use aes::Aes128;
use ctr::cipher::{KeyIvInit, StreamCipher};

const MAX_PACKET: usize = 1400;
/// 12 bytes of RTP header, 7 of Cast header.
const HEADER: usize = 19;
/// Frames kept to resend lost packets: the receiver gives up beyond 120 (kMaxUnackedFrames).
const CACHE_FRAMES: usize = 120;
const NTP_UNIX_OFFSET: u64 = 2_208_988_800;
/// The receiver repeats its loss report until the packet arrives: do not resend it again so soon.
/// ponytail: fixed, openscreen waits one measured round trip.
const RESEND_SPACING: Duration = Duration::from_millis(50);

/// AES-128 key and IV mask of one stream, sent in the OFFER.
#[derive(Clone, Copy)]
pub struct Keys {
    pub key: [u8; 16],
    pub iv_mask: [u8; 16],
}

impl Keys {
    pub fn random() -> anyhow::Result<Self> {
        let mut bytes = [0u8; 32];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        let (key, iv_mask) = bytes.split_at(16);
        Ok(Self { key: key.try_into()?, iv_mask: iv_mask.try_into()? })
    }
}

/// AES-CTR over a frame. The counter starts at the IV mask XOR the frame id in bytes 8..12.
fn crypt(keys: &Keys, frame_id: u32, data: &mut [u8]) {
    let mut nonce = [0u8; 16];
    nonce[8..12].copy_from_slice(&frame_id.to_be_bytes());
    for (byte, mask) in nonce.iter_mut().zip(keys.iv_mask) {
        *byte ^= mask;
    }
    ctr::Ctr128BE::<Aes128>::new(&keys.key.into(), &nonce.into()).apply_keystream(data);
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A frame kept for resending.
struct SentFrame {
    id: u32,
    packets: Vec<Vec<u8>>,
    /// When each packet was last resent.
    resent: Vec<Option<Instant>>,
}

/// One RTP stream (video or audio): numbering, encryption, and the packets kept for resending.
pub struct RtpStream {
    ssrc: u32,
    payload_type: u8,
    clock_rate: u64,
    keys: Keys,
    seq: u16,
    next_frame: u32,
    sent: VecDeque<SentFrame>,
    packets: u32,
    /// Payload bytes for the Sender Report: wraps at 4 GiB, as RTCP wants.
    octets: u32,
    /// Payload bytes sent so far, for the bitrate shown in the menu.
    pub total_octets: u64,
}

impl RtpStream {
    pub fn new(ssrc: u32, payload_type: u8, clock_rate: u64, keys: Keys) -> Self {
        // ponytail: fixed start values; RFC 3550 wants random ones, the receiver does not care.
        Self {
            ssrc,
            payload_type,
            clock_rate,
            keys,
            seq: 0,
            next_frame: 0,
            sent: VecDeque::new(),
            packets: 0,
            octets: 0,
            total_octets: 0,
        }
    }

    fn rtp_time(&self, running: Duration) -> u32 {
        (running.as_nanos() * u128::from(self.clock_rate) / 1_000_000_000) as u32
    }

    /// Encrypt a frame captured at `running` and split it into packets, kept for resending.
    pub fn packetize(&mut self, data: &[u8], running: Duration, key_frame: bool) -> &[Vec<u8>] {
        let id = self.next_frame;
        self.next_frame = id.wrapping_add(1);
        let mut payload = data.to_vec();
        crypt(&self.keys, id, &mut payload);
        let chunk = MAX_PACKET - HEADER;
        // An empty frame still needs one packet, or the receiver waits for its id.
        let count = payload.len().div_ceil(chunk).max(1);
        // A delta frame of H.264 without B-frames refers to the previous one.
        let reference = if key_frame { id } else { id.wrapping_sub(1) };
        let timestamp = self.rtp_time(running);
        let mut packets = Vec::with_capacity(count);
        for (index, part) in payload.chunks(chunk).chain(payload.is_empty().then_some(&[][..])).enumerate() {
            let mut packet = Vec::with_capacity(HEADER + part.len());
            let marker = if index == count - 1 { 0x80 } else { 0 };
            packet.extend([0x80, marker | self.payload_type]);
            packet.extend(self.seq.to_be_bytes());
            packet.extend(timestamp.to_be_bytes());
            packet.extend(self.ssrc.to_be_bytes());
            // Key frame flag, "has reference frame id" flag, then truncated frame ids.
            packet.push(if key_frame { 0xc0 } else { 0x40 });
            packet.push(id as u8);
            packet.extend((index as u16).to_be_bytes());
            packet.extend(((count - 1) as u16).to_be_bytes());
            packet.push(reference as u8);
            packet.extend(part);
            self.seq = self.seq.wrapping_add(1);
            self.packets = self.packets.wrapping_add(1);
            self.octets = self.octets.wrapping_add(part.len() as u32);
            self.total_octets += part.len() as u64;
            packets.push(packet);
        }
        if self.sent.len() == CACHE_FRAMES {
            self.sent.pop_front();
        }
        let resent = vec![None; packets.len()];
        self.sent.push_back(SentFrame { id, packets, resent });
        &self.sent.back().expect("just pushed").packets
    }

    /// Packets the receiver reported lost: `packet`, or all of the frame for 0xffff, plus the
    /// next 8 packets flagged in `bits`. `frame` is the frame id truncated to 8 bits.
    /// Packets resent less than RESEND_SPACING before `now` are left out.
    pub fn lost(&mut self, frame: u8, packet: u16, bits: u8, now: Instant) -> Vec<&[u8]> {
        let latest = self.next_frame.wrapping_sub(1);
        let mut id = (latest & !0xff) | u32::from(frame);
        if id > latest {
            id = id.wrapping_sub(256);
        }
        let Some(SentFrame { packets, resent, .. }) = self.sent.iter_mut().find(|f| f.id == id) else {
            return Vec::new();
        };
        let wanted: Vec<usize> = if packet == 0xffff {
            (0..packets.len()).collect()
        } else {
            let first = usize::from(packet);
            let flagged = (0..8).filter(|bit| bits >> bit & 1 == 1).map(|bit| first + 1 + bit);
            std::iter::once(first).chain(flagged).collect()
        };
        let mut out = Vec::new();
        for index in wanted {
            let (Some(data), Some(last)) = (packets.get(index), resent.get_mut(index)) else { continue };
            if last.is_some_and(|at| now.duration_since(at) < RESEND_SPACING) {
                continue;
            }
            *last = Some(now);
            out.push(data.as_slice());
        }
        out
    }

    /// RTCP Sender Report: maps the RTP clock to the wall clock, so the receiver can schedule frames.
    pub fn sender_report(&self, wall: Duration, running: Duration) -> Vec<u8> {
        let fraction = (u64::from(wall.subsec_nanos()) << 32) / 1_000_000_000;
        let ntp = ((wall.as_secs() + NTP_UNIX_OFFSET) << 32) | fraction;
        let mut report = vec![0x80, 200, 0, 6];
        report.extend(self.ssrc.to_be_bytes());
        report.extend(ntp.to_be_bytes());
        report.extend(self.rtp_time(running).to_be_bytes());
        report.extend(self.packets.to_be_bytes());
        report.extend(self.octets.to_be_bytes());
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SSRC: u32 = 0x5cca_5701;
    const PAYLOAD_TYPE: u8 = 101;
    const KEYS: Keys = Keys {
        key: [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
        iv_mask: [0xf0, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa, 0xfb, 0xfc, 0xfd, 0xfe, 0xff],
    };

    #[test]
    fn encrypts_like_the_prototype() {
        // Expected bytes from proto/stream_proto.py (Python cryptography), whose frames the Sharp TV decoded.
        let mut data = b"Cast Streaming test payload, 40 bytes...".to_vec();
        crypt(&KEYS, 0x0102_0307, &mut data);
        assert_eq!(hex(&data), "8219ecebefdbbe9c7c63cce6247d2093800ae9fa86ea2a75f6798bc377a6b91a1347f8a6846536e1");
        crypt(&KEYS, 0x0102_0307, &mut data);
        assert_eq!(data, b"Cast Streaming test payload, 40 bytes...");
    }

    #[test]
    fn packetizes_and_resends() {
        let mut stream = RtpStream::new(SSRC, PAYLOAD_TYPE, 90_000, KEYS);
        let frame = vec![7u8; 3000];
        let packets = stream.packetize(&frame, Duration::from_secs(1), true).to_vec();
        assert_eq!(packets.len(), 3);
        let first = &packets[0];
        assert_eq!(first[..2], [0x80, PAYLOAD_TYPE]);
        assert_eq!(first[4..8], 90_000u32.to_be_bytes());
        assert_eq!(first[8..12], SSRC.to_be_bytes());
        // Key frame, frame 0, packet 0 of 0..=2, refers to itself.
        assert_eq!(first[12..19], [0xc0, 0, 0, 0, 0, 2, 0]);
        assert_eq!(packets[2][1], 0x80 | PAYLOAD_TYPE, "marker on the last packet");
        assert_eq!(packets.iter().map(|p| p.len() - HEADER).sum::<usize>(), 3000);
        assert_eq!(packets[1][2..4], 1u16.to_be_bytes());

        let delta = stream.packetize(&[1, 2, 3], Duration::from_secs(2), false).to_vec();
        assert_eq!(delta[0][12..19], [0x40, 1, 0, 0, 0, 0, 0]);
        let now = Instant::now();
        assert_eq!(stream.lost(0, 1, 0b1, now), vec![&packets[1][..], &packets[2][..]]);
        // Packets 1 and 2 were just resent: only packet 0 goes again.
        assert_eq!(stream.lost(0, 0xffff, 0, now).len(), 1);
        assert_eq!(stream.lost(0, 0xffff, 0, now + RESEND_SPACING).len(), 3);
        assert!(stream.lost(9, 0, 0, now).is_empty());
        assert_eq!(stream.total_octets, 3003);
    }
}
