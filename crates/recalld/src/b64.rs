//! Standard base64 (RFC 4648, padded), for the one thing that needs it:
//! putting a segment's WAV inside a JSON frame so a client can play it.
//!
//! Hand-rolled rather than pulled in as a dependency — it is twenty lines, the
//! alphabet is frozen by the RFC, and a capture daemon should not grow a crate
//! for it.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// How many bytes `encode` will produce for `n` input bytes. Exact, so a caller
/// can check a size limit before it spends the memory.
pub const fn encoded_len(n: usize) -> usize {
    n.div_ceil(3) * 4
}

pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(encoded_len(bytes.len()));
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        // Padding is not cosmetic: every JavaScript `atob` refuses a length
        // that is not a multiple of four.
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rfc_4648_vectors_round_trip() {
        for (raw, want) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode(raw.as_bytes()), want, "encoding {raw:?}");
            assert_eq!(encoded_len(raw.len()), want.len());
        }
    }

    #[test]
    fn the_whole_byte_range_uses_the_whole_alphabet() {
        let all: Vec<u8> = (0..=255u8).collect();
        let encoded = encode(&all);
        assert_eq!(encoded.len(), encoded_len(all.len()));
        assert!(encoded.ends_with('='), "256 is not a multiple of 3");
        for c in encoded.chars() {
            assert!(
                c == '=' || ALPHABET.contains(&(c as u8)),
                "{c:?} is not base64"
            );
        }
        // A RIFF header is what the GUI actually decodes; make sure the leading
        // bytes are the ones every base64 decoder produces for it.
        assert_eq!(encode(b"RIFF"), "UklGRg==");
    }
}
