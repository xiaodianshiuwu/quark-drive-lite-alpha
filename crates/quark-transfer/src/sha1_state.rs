//! SHA-1 prefix state for the observed OSS `X-Oss-Hash-Ctx` wire protocol.
//!
//! Independently implements the public SHA-1 compression algorithm. This is a
//! port of this project's independently authored evaluation protocol, not of an
//! AGPL/GPL uploader. Only block-aligned prefixes are exported; a final digest
//! must never be substituted for the unpadded compression state.
use crate::manager::{err, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Serialize;

#[derive(Clone)]
pub(crate) struct Sha1State {
    words: [u32; 5],
    bytes: u64,
    tail: Vec<u8>,
}

impl Sha1State {
    pub(crate) fn new() -> Self {
        Self {
            words: [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476, 0xc3d2e1f0],
            bytes: 0,
            tail: Vec::with_capacity(64),
        }
    }

    pub(crate) fn update(&mut self, mut input: &[u8]) -> Result<()> {
        self.bytes = self
            .bytes
            .checked_add(input.len() as u64)
            .filter(|n| *n <= u64::MAX / 8)
            .ok_or_else(|| err("INVALID_ARG", "上传文件超过 SHA-1 长度上限"))?;
        if !self.tail.is_empty() {
            let take = (64 - self.tail.len()).min(input.len());
            self.tail.extend_from_slice(&input[..take]);
            input = &input[take..];
            if self.tail.len() == 64 {
                let block: [u8; 64] = self.tail.as_slice().try_into().unwrap();
                self.compress(&block);
                self.tail.clear();
            }
        }
        let mut blocks = input.chunks_exact(64);
        for block in &mut blocks {
            self.compress(block.try_into().unwrap());
        }
        self.tail.extend_from_slice(blocks.remainder());
        Ok(())
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut schedule = [0u32; 80];
        for (word, bytes) in schedule.iter_mut().zip(block.chunks_exact(4)) {
            *word = u32::from_be_bytes(bytes.try_into().unwrap());
        }
        for i in 16..80 {
            schedule[i] = (schedule[i - 3] ^ schedule[i - 8] ^ schedule[i - 14] ^ schedule[i - 16])
                .rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = self.words;
        for (i, word) in schedule.into_iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5a827999),
                20..=39 => (b ^ c ^ d, 0x6ed9eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1bbcdc),
                _ => (b ^ c ^ d, 0xca62c1d6),
            };
            let next = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = next;
        }
        for (state, word) in self.words.iter_mut().zip([a, b, c, d, e]) {
            *state = state.wrapping_add(word);
        }
    }

    pub(crate) fn export_base64(&self) -> Result<String> {
        if !self.tail.is_empty() {
            return Err(err("CONTENT_MISMATCH", "上传哈希前缀未按 64 字节对齐"));
        }
        #[derive(Serialize)]
        #[allow(non_snake_case)]
        struct Context {
            hash_type: &'static str,
            h0: String,
            h1: String,
            h2: String,
            h3: String,
            h4: String,
            Nl: String,
            Nh: String,
            data: &'static str,
            num: &'static str,
        }
        let bits = self
            .bytes
            .checked_mul(8)
            .ok_or_else(|| err("INVALID_ARG", "上传文件过大"))?;
        let context = Context {
            hash_type: "sha1",
            h0: self.words[0].to_string(),
            h1: self.words[1].to_string(),
            h2: self.words[2].to_string(),
            h3: self.words[3].to_string(),
            h4: self.words[4].to_string(),
            Nl: (bits as u32).to_string(),
            Nh: ((bits >> 32) as u32).to_string(),
            data: "",
            num: "0",
        };
        Ok(STANDARD.encode(
            serde_json::to_vec(&context).map_err(|_| err("LOCAL_IO", "上传哈希状态编码失败"))?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha1::{Digest, Sha1};

    fn digest(mut state: Sha1State) -> Vec<u8> {
        let bits = state.bytes * 8;
        let padding = if state.tail.len() < 56 {
            56 - state.tail.len()
        } else {
            120 - state.tail.len()
        };
        let mut tail = vec![0; padding + 8];
        tail[0] = 0x80;
        tail[padding..].copy_from_slice(&bits.to_be_bytes());
        state.update(&tail).unwrap();
        state.words.iter().flat_map(|n| n.to_be_bytes()).collect()
    }

    #[test]
    fn real_compression_matches_standard_digest_across_buffer_boundaries() {
        for size in [0, 1, 55, 56, 63, 64, 65, 127, 128, 4097, 65536 + 7] {
            let bytes: Vec<_> = (0..size).map(|i| ((i * 71 + i / 17) % 256) as u8).collect();
            for chunk in [1, 13, 64, 1024] {
                let mut state = Sha1State::new();
                for data in bytes.chunks(chunk) {
                    state.update(data).unwrap();
                }
                assert_eq!(
                    digest(state),
                    Sha1::digest(&bytes).to_vec(),
                    "size={size} chunk={chunk}"
                );
            }
        }
    }

    #[test]
    fn exports_real_unpadded_prefix_and_bit_counter_high_word() {
        let mut state = Sha1State::new();
        state.update(&[b'a'; 64]).unwrap();
        let context: serde_json::Value =
            serde_json::from_slice(&STANDARD.decode(state.export_base64().unwrap()).unwrap())
                .unwrap();
        assert_eq!(context["h0"], "3662244075"); // Independently recorded evaluation fixture, before padding.
        assert_eq!(context["Nl"], "512");
        assert_eq!(context["Nh"], "0");
        state.bytes = 512 * 1024 * 1024 + 64;
        let context: serde_json::Value =
            serde_json::from_slice(&STANDARD.decode(state.export_base64().unwrap()).unwrap())
                .unwrap();
        assert_eq!(context["Nl"], "512");
        assert_eq!(context["Nh"], "1");
        state.update(&[1]).unwrap();
        assert!(state.export_base64().is_err());
    }
}
