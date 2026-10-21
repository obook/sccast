//! pin.rs
//! The 6-character code a panel shows holds its address: decode it into an IP and a command
//! port, as the vendor's Windows client does (PinCodeService.dll, read with Ghidra; port table
//! from the Android sender). Unverified on a real panel. Codes that need the vendor's cloud
//! lookup are not supported.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::net::{Ipv4Addr, SocketAddrV4};

/// Command port, chosen by 2 bits of the code.
pub const PORTS: [u16; 4] = [7385, 29736, 2067, 39458];

/// Panel address from its code, or why the code cannot give one.
pub fn decode(typed: &str) -> Result<SocketAddrV4, String> {
    let typed = typed.trim();
    // W, X, Y and Z stand for characters a person could misread.
    let code: String = typed
        .to_ascii_uppercase()
        .chars()
        .map(|character| match character {
            'W' => '0',
            'X' => '1',
            'Y' => 'I',
            'Z' => 'O',
            other => other,
        })
        .collect();
    if code.len() != 6 {
        return Err(format!("the panel code has 6 characters, not {}", code.len()));
    }
    let bad = || format!("{typed} is not a panel code");
    let byte = |value: u32, shift: u32| (value >> shift) as u8;
    // Six digits: a 192.168.x.x address.
    if code.chars().all(|character| character.is_ascii_digit()) {
        let value: u32 = code.parse().map_err(|_| bad())?;
        let ip = Ipv4Addr::new(192, 168, byte(value, 0), byte(value, 8));
        return Ok(SocketAddrV4::new(ip, PORTS[(value >> 16) as usize & 3]));
    }
    let value = u32::from_str_radix(&code, 32).map_err(|_| bad())?;
    match (value >> 28) & 3 {
        // 10.x.x.x, each byte masked with a 2-bit key.
        3 => {
            let key = byte(value, 26) & 3;
            let mask = key ^ (key << 4);
            let ip = Ipv4Addr::new(10, mask ^ byte(value, 16), mask ^ byte(value, 8), mask ^ byte(value, 0));
            Ok(SocketAddrV4::new(ip, PORTS[(value >> 24) as usize & 3]))
        }
        // 172.16.0.0/12, masked with a 4-bit key; 2 bits of the key are repeated as a check.
        2 => {
            let (key_byte, second_byte) = (byte(value, 24), byte(value, 16));
            if key_byte & 3 != second_byte >> 6 {
                return Err(bad());
            }
            let key = key_byte & 0x0f;
            let mask = key ^ (key << 4);
            let ip = Ipv4Addr::new(172, (second_byte & 0x0f) | 0x10, mask ^ byte(value, 8), mask ^ byte(value, 0));
            Ok(SocketAddrV4::new(ip, PORTS[(value >> 20) as usize & 3]))
        }
        _ => Err(format!("the code {typed} needs the vendor's cloud service: give the panel's address with --host")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Base-32 code of `v` (30 bits), as the panel would show it.
    fn code(v: u32) -> String {
        (0..6).rev().map(|i| char::from_digit((v >> (5 * i)) & 31, 32).unwrap().to_ascii_uppercase()).collect()
    }

    #[test]
    fn decodes_a_home_network_code() {
        // 192.168.1.42, port index 0: v = 42 * 256 + 1.
        assert_eq!(decode("010753"), Ok("192.168.1.42:7385".parse().unwrap()));
        // 206497 = 0x0326A1: port index 3, bytes 0xA1 and 0x26.
        assert_eq!(decode("206497"), Ok("192.168.161.38:39458".parse().unwrap()));
    }

    #[test]
    fn decodes_10_and_172_codes() {
        let (k, idx) = (2u32, 1u32);
        let m = k ^ (k << 4);
        let v = 3 << 28 | k << 26 | idx << 24 | (20 ^ m) << 16 | (30 ^ m) << 8 | (40 ^ m);
        assert_eq!(decode(&code(v)), Ok("10.20.30.40:29736".parse().unwrap()));

        let (k, idx, n) = (0x9u32, 3u32, 5u32);
        let m = (k ^ (k << 4)) & 0xff;
        let b = (k & 3) << 6 | idx << 4 | n;
        let v = 2 << 28 | k << 24 | b << 16 | (200 ^ m) << 8 | (7 ^ m);
        assert_eq!(decode(&code(v)), Ok("172.21.200.7:39458".parse().unwrap()));
        // The repeated key bits do not match.
        assert!(decode(&code(v ^ 1 << 22)).is_err());
    }

    #[test]
    fn maps_letters_and_rejects_others() {
        // W and X stand for 0 and 1, lowercase is accepted.
        assert_eq!(decode("w1w753"), decode("010753"));
        assert!(decode("12345").is_err());
        assert!(decode("0000A0").unwrap_err().contains("cloud"));
    }
}
