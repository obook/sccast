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
/// Playout delay extension on the first packet of a frame: type 1, size 2, then the delay in ms.
const DELAY_EXTENSION: usize = 4;
/// Frames kept to resend lost packets until acknowledged: the receiver gives up beyond 120
/// (kMaxUnackedFrames).
const CACHE_FRAMES: usize = 120;
const NTP_UNIX_OFFSET: u64 = 2_208_988_800;
/// The receiver repeats its loss report until the packet arrives: wait one round trip before
/// resending it again, and at least this long. On the Sharp (3 ms round trip), waiting the
/// round trip alone gave 9 to 38 resends a second, against about 5 with 50 ms.
const RESEND_SPACING: Duration = Duration::from_millis(50);
/// Sender Reports remembered to match the receiver's: 1.6 s at one every 100 ms.
const REPORT_HISTORY: usize = 16;

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
    /// Sender Reports sent: middle 32 bits of their NTP time, and when.
    reports: VecDeque<(u32, Instant)>,
    /// Round trip to the receiver, from its last Receiver Report.
    pub rtt: Option<Duration>,
    /// Playout delay in ms; None when the receiver did not accept the extension.
    pub playout_delay: Option<u16>,
    /// The next frame carries the new delay. Only that one, as openscreen does.
    delay_changed: bool,
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
            reports: VecDeque::with_capacity(REPORT_HISTORY),
            rtt: None,
            playout_delay: None,
            delay_changed: false,
        }
    }

    /// New playout delay, sent in the next frame. No-op without the extension or if unchanged.
    pub fn set_playout_delay(&mut self, ms: u16) {
        if self.playout_delay.is_some_and(|current| current != ms) {
            self.playout_delay = Some(ms);
            self.delay_changed = true;
        }
    }

    /// Full id of a frame id truncated to 8 bits: the latest sent frame with these low bits.
    fn frame_id(&self, frame: u8) -> u32 {
        let latest = self.next_frame.wrapping_sub(1);
        let id = (latest & !0xff) | u32::from(frame);
        if id > latest {
            id.wrapping_sub(256)
        } else {
            id
        }
    }

    /// The receiver has every frame up to `frame`: they will not be asked for again.
    /// False when the checkpoint is before the first frame (nothing received yet).
    pub fn acked(&mut self, frame: u8) -> bool {
        let id = self.frame_id(frame);
        // Wrapping order: `f` is at or before `id`.
        while self.sent.front().is_some_and(|f| id.wrapping_sub(f.id) < 1 << 31) {
            self.sent.pop_front();
        }
        id < self.next_frame
    }

    /// Round trip from a Receiver Report: time since our Sender Report, minus the receiver's hold.
    pub fn receiver_report(&mut self, last_sr: u32, since_sr: u32, now: Instant) {
        let Some(&(_, sent)) = self.reports.iter().rev().find(|(ntp, _)| last_sr != 0 && *ntp == last_sr) else {
            return;
        };
        let held = Duration::from_nanos(u64::from(since_sr) * 1_000_000_000 / 65_536);
        if let Some(measure) = now.duration_since(sent).checked_sub(held) {
            // Smoothed as TCP's SRTT (RFC 6298): one low sample on a busy Wi-Fi must not hasten resends.
            self.rtt = Some(self.rtt.map_or(measure, |rtt| (rtt * 7 + measure) / 8));
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
        // A lost first packet is resent from the cache, extension included.
        let announce = self.playout_delay.filter(|_| std::mem::take(&mut self.delay_changed));
        // Room for the extension in every packet of that frame keeps them all under MAX_PACKET.
        let chunk = MAX_PACKET - HEADER - announce.map_or(0, |_| DELAY_EXTENSION);
        // An empty frame still needs one packet, or the receiver waits for its id.
        let count = payload.len().div_ceil(chunk).max(1);
        // A delta frame of H.264 without B-frames refers to the previous one.
        let reference = if key_frame { id } else { id.wrapping_sub(1) };
        let timestamp = self.rtp_time(running);
        let mut packets = Vec::with_capacity(count);
        for (index, part) in payload.chunks(chunk).chain(payload.is_empty().then_some(&[][..])).enumerate() {
            let mut packet = Vec::with_capacity(HEADER + DELAY_EXTENSION + part.len());
            let marker = if index == count - 1 { 0x80 } else { 0 };
            packet.extend([0x80, marker | self.payload_type]);
            packet.extend(self.seq.to_be_bytes());
            packet.extend(timestamp.to_be_bytes());
            packet.extend(self.ssrc.to_be_bytes());
            // Only the first packet carries the delay, as openscreen does.
            let delay = announce.filter(|_| index == 0);
            // Key frame flag, "has reference frame id" flag, extension count, then truncated frame ids.
            packet.push(if key_frame { 0xc0 } else { 0x40 } | u8::from(delay.is_some()));
            packet.push(id as u8);
            packet.extend((index as u16).to_be_bytes());
            packet.extend(((count - 1) as u16).to_be_bytes());
            packet.push(reference as u8);
            if let Some(ms) = delay {
                packet.extend((1u16 << 10 | 2).to_be_bytes());
                packet.extend(ms.to_be_bytes());
            }
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
    /// Packets resent less than one round trip before `now` are left out.
    pub fn lost(&mut self, frame: u8, packet: u16, bits: u8, now: Instant) -> Vec<&[u8]> {
        let id = self.frame_id(frame);
        let spacing = self.rtt.map_or(RESEND_SPACING, |rtt| rtt.max(RESEND_SPACING));
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
            if last.is_some_and(|at| now.duration_since(at) < spacing) {
                continue;
            }
            *last = Some(now);
            out.push(data.as_slice());
        }
        out
    }

    /// RTCP Sender Report: maps the RTP clock to the wall clock, so the receiver can schedule frames.
    /// Remembered with `now`, the time it is sent, to measure the round trip.
    pub fn sender_report(&mut self, wall: Duration, running: Duration, now: Instant) -> Vec<u8> {
        let fraction = (u64::from(wall.subsec_nanos()) << 32) / 1_000_000_000;
        let ntp = ((wall.as_secs() + NTP_UNIX_OFFSET) << 32) | fraction;
        if self.reports.len() == REPORT_HISTORY {
            self.reports.pop_front();
        }
        self.reports.push_back(((ntp >> 16) as u32, now));
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

        // Checkpoint 0xff: before frame 0, nothing received yet.
        assert!(!stream.acked(0xff));
        // Frame 0 acknowledged: no longer resent. Frame 1 still is.
        assert!(stream.acked(0));
        assert!(stream.lost(0, 0xffff, 0, now + RESEND_SPACING).is_empty());
        assert_eq!(stream.lost(1, 0xffff, 0, now).len(), 1);
    }

    #[test]
    fn writes_the_playout_delay() {
        let mut stream = RtpStream::new(SSRC, PAYLOAD_TYPE, 90_000, KEYS);
        stream.playout_delay = Some(100);
        let frame = vec![7u8; 2000];
        // Unchanged since the OFFER: no extension.
        assert_eq!(stream.packetize(&frame, Duration::ZERO, false)[0][12], 0x40);
        stream.set_playout_delay(300);
        let packets = stream.packetize(&frame, Duration::ZERO, false).to_vec();
        assert_eq!(packets.len(), 2);
        // Delta frame, one extension, then type 1 size 2 and 300 ms after the reference id.
        assert_eq!(packets[0][12], 0x41);
        assert_eq!(packets[0][19..23], [0x04, 0x02, 0x01, 0x2c]);
        assert_eq!(packets[1][12], 0x40, "first packet only");
        assert!(packets.iter().all(|p| p.len() <= MAX_PACKET));
        assert_eq!(packets[0].len() - HEADER - DELAY_EXTENSION + packets[1].len() - HEADER, 2000);
        // The next frame does not repeat it, and an unchanged delay is not sent again.
        assert_eq!(stream.packetize(&frame, Duration::ZERO, false)[0][12], 0x40);
        stream.set_playout_delay(300);
        assert_eq!(stream.packetize(&frame, Duration::ZERO, false)[0][12], 0x40);
        // Without the extension, nothing is ever written.
        let mut plain = RtpStream::new(SSRC, PAYLOAD_TYPE, 90_000, KEYS);
        plain.set_playout_delay(300);
        assert_eq!(plain.packetize(&frame, Duration::ZERO, false)[0][12], 0x40);
    }

    #[test]
    fn measures_the_round_trip() {
        let mut stream = RtpStream::new(SSRC, PAYLOAD_TYPE, 90_000, KEYS);
        let sent = Instant::now();
        // 1.5 s after the Unix epoch: NTP seconds 2_208_988_801, fraction 0x8000_0000.
        stream.sender_report(Duration::from_millis(1500), Duration::ZERO, sent);
        let last_sr = ((2_208_988_801u64 << 32 | 0x8000_0000) >> 16) as u32;
        // Unknown report, or none received: no measure.
        stream.receiver_report(last_sr + 1, 0, sent + Duration::from_millis(100));
        stream.receiver_report(0, 0, sent + Duration::from_millis(100));
        assert_eq!(stream.rtt, None);
        // Answered 100 ms after sending, the receiver held it 20 ms (1311/65536 s): 80 ms round trip.
        stream.receiver_report(last_sr, 1311, sent + Duration::from_millis(100));
        let rtt = stream.rtt.expect("round trip measured");
        assert!(rtt.abs_diff(Duration::from_millis(80)) < Duration::from_millis(1), "{rtt:?}");
        // A second measure of 160 ms moves it by one eighth only: 90 ms.
        stream.sender_report(Duration::from_millis(1500), Duration::ZERO, sent);
        stream.receiver_report(last_sr, 0, sent + Duration::from_millis(160));
        let smoothed = stream.rtt.expect("round trip measured");
        assert!(smoothed.abs_diff(Duration::from_millis(90)) < Duration::from_millis(1), "{smoothed:?}");
        // A round trip above RESEND_SPACING sets the wait between two resends.
        stream.rtt = Some(Duration::from_millis(80));
        let packets = stream.packetize(&[1], Duration::ZERO, true).len();
        assert_eq!(packets, 1);
        let now = Instant::now();
        assert_eq!(stream.lost(0, 0, 0, now).len(), 1);
        assert!(stream.lost(0, 0, 0, now + RESEND_SPACING).is_empty());
        assert_eq!(stream.lost(0, 0, 0, now + Duration::from_millis(81)).len(), 1);
        // A short one does not go below RESEND_SPACING.
        stream.rtt = Some(Duration::from_millis(3));
        let later = now + Duration::from_millis(81);
        assert!(stream.lost(0, 0, 0, later + Duration::from_millis(10)).is_empty());
        assert_eq!(stream.lost(0, 0, 0, later + RESEND_SPACING).len(), 1);
    }
}
