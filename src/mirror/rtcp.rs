//! rtcp.rs
//! Read the reports the mirroring receiver sends back over UDP (RTCP): packets lost,
//! key frame requests, share of packets lost. send.rs acts on them.
//! From openscreen/cast/streaming/impl/compound_rtcp_parser.cc.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

/// What the receiver reports in RTCP.
#[derive(Debug, PartialEq)]
pub enum Feedback {
    KeyFrame { ssrc: u32 },
    Lost { ssrc: u32, frame: u8, packet: u16, bits: u8 },
    /// Share of packets lost since the last report, out of 256.
    Loss { ssrc: u32, fraction: u8 },
}

fn word(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// Parse a compound RTCP packet from the receiver. Unknown parts are skipped.
pub fn parse_rtcp(buf: &[u8]) -> Vec<Feedback> {
    let mut found = Vec::new();
    let mut at = 0;
    while at + 4 <= buf.len() {
        let (subtype, kind) = (buf[at] & 0x1f, buf[at + 1]);
        let words = usize::from(u16::from_be_bytes([buf[at + 2], buf[at + 3]]));
        let end = (at + 4 + 4 * words).min(buf.len());
        let body = &buf[at + 4..end];
        at = end;
        match (kind, subtype) {
            // Receiver Report: its blocks follow the reporter's SSRC.
            (201, count) => {
                for block in body.get(4..).unwrap_or_default().chunks_exact(24).take(usize::from(count)) {
                    found.extend(word(block, 0).map(|ssrc| Feedback::Loss { ssrc, fraction: block[4] }));
                }
            }
            (206, 1) => found.extend(word(body, 4).map(|ssrc| Feedback::KeyFrame { ssrc })),
            (206, 15) if body.get(8..12) == Some(b"CAST") && body.len() >= 16 => {
                let Some(ssrc) = word(body, 4) else { continue };
                for loss in body[16..].chunks_exact(4).take(usize::from(body[13])) {
                    let packet = u16::from_be_bytes([loss[1], loss[2]]);
                    found.push(Feedback::Lost { ssrc, frame: loss[0], packet, bits: loss[3] });
                }
            }
            _ => {}
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    const SSRC: u32 = 0x5cca_5701;

    #[test]
    fn parses_receiver_feedback() {
        let mut buf = vec![0x81, 201, 0, 7];
        buf.extend(2u32.to_be_bytes());
        buf.extend(SSRC.to_be_bytes());
        buf.extend([12, 0, 0, 1]);
        buf.extend([0; 16]);
        buf.extend([0x81, 206, 0, 2]);
        buf.extend(2u32.to_be_bytes());
        buf.extend(SSRC.to_be_bytes());
        buf.extend([0x8f, 206, 0, 5]);
        buf.extend(2u32.to_be_bytes());
        buf.extend(SSRC.to_be_bytes());
        buf.extend(b"CAST");
        buf.extend([4, 1, 0, 100]);
        buf.extend([5, 0, 3, 0b10]);
        assert_eq!(
            parse_rtcp(&buf),
            vec![
                Feedback::Loss { ssrc: SSRC, fraction: 12 },
                Feedback::KeyFrame { ssrc: SSRC },
                Feedback::Lost { ssrc: SSRC, frame: 5, packet: 3, bits: 0b10 },
            ]
        );
        assert!(parse_rtcp(&[0x80, 201]).is_empty());
    }
}
