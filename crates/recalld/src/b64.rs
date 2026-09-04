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

/// The inverse, for the one thing that needs *that*: PCM arriving from the
/// Discord plugin inside a JSON line (0.12.1).
///
/// Strict, deliberately. Whitespace is skipped — a client that wrapped its
/// lines is still sending the same bytes — but any character outside the
/// alphabet, a misplaced pad, or a length that is not a multiple of four is
/// `None`. A lenient decoder would turn a corrupted frame into a quieter,
/// shorter frame full of plausible samples, and that is a worse failure than
/// a refused line: it would be transcribed.
pub fn decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut quad = [0u8; 4];
    let mut n = 0usize;
    let mut pad = 0usize;
    for c in text.bytes() {
        if c.is_ascii_whitespace() {
            continue;
        }
        // Padding may only ever be the last one or two characters of a quad,
        // and nothing may follow it.
        if c == b'=' {
            if n < 2 || pad >= 2 {
                return None;
            }
            pad += 1;
            quad[n] = 0;
        } else {
            if pad > 0 {
                return None;
            }
            quad[n] = match c {
                b'A'..=b'Z' => c - b'A',
                b'a'..=b'z' => c - b'a' + 26,
                b'0'..=b'9' => c - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                _ => return None,
            };
        }
        n += 1;
        if n == 4 {
            let v = (u32::from(quad[0]) << 18)
                | (u32::from(quad[1]) << 12)
                | (u32::from(quad[2]) << 6)
                | u32::from(quad[3]);
            out.push((v >> 16) as u8);
            if pad < 2 {
                out.push((v >> 8) as u8);
            }
            if pad < 1 {
                out.push(v as u8);
            }
            n = 0;
            if pad > 0 {
                // A pad ends the stream; anything after it is not this message.
                return text
                    .bytes()
                    .skip_while(|b| *b != b'=')
                    .all(|b| b == b'=' || b.is_ascii_whitespace())
                    .then_some(out);
            }
        }
    }
    (n == 0).then_some(out)
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
            assert_eq!(
                decode(want).as_deref(),
                Some(raw.as_bytes()),
                "decoding {want:?}"
            );
        }
    }

    #[test]
    fn decode_round_trips_the_whole_byte_range_and_a_pcm_frame() {
        let all: Vec<u8> = (0..=255u8).collect();
        assert_eq!(decode(&encode(&all)).as_deref(), Some(all.as_slice()));

        // What actually arrives: 500 ms of 16 kHz PCM16.
        let pcm: Vec<u8> = (0..16_000).map(|i| (i % 251) as u8).collect();
        let text = encode(&pcm);
        assert_eq!(text.len(), 21_336, "the wire size a batch is budgeted by");
        assert_eq!(decode(&text).as_deref(), Some(pcm.as_slice()));
        // A client that wrapped its lines still sent the same bytes.
        assert_eq!(decode("Zm9v\nYmFy").as_deref(), Some(&b"foobar"[..]));
    }

    #[test]
    fn a_corrupted_frame_is_refused_rather_than_shortened() {
        // Silently dropping bad characters would produce a shorter run of
        // entirely plausible samples, and that gets transcribed.
        for bad in [
            "Zm9vYmF",   // length not a multiple of four
            "Zm9v!mFy",  // outside the alphabet
            "Zm9=vYmFy", // pad in the middle
            "Z===",      // two pads is the most there can be
            "====",      // and never in the first two places
            "Zg==Zg==",  // a pad ends the message
        ] {
            assert_eq!(decode(bad), None, "{bad:?} must not decode");
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
