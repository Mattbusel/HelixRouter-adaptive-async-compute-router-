//! Request/response compression with multiple algorithms.
//!
//! Provides run-length encoding, dictionary compression, and a simplified LZ77
//! implementation, plus a [`CompressionEngine`] that selects algorithms
//! automatically based on data entropy.

use std::collections::HashMap;

// ── Algorithm ────────────────────────────────────────────────────────────────

/// Compression algorithm selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompressionAlgorithm {
    /// No compression — data is stored verbatim.
    None,
    /// Run-length encoding: (count, byte) pairs.
    RunLength,
    /// Dictionary-based compression with a learned byte-sequence vocabulary.
    Dictionary,
    /// Simplified LZ77 sliding-window compression.
    Lz77,
}

impl CompressionAlgorithm {
    /// Relative compression effort level (0 = none, 3 = highest).
    pub fn compression_level(&self) -> u8 {
        match self {
            CompressionAlgorithm::None => 0,
            CompressionAlgorithm::RunLength => 1,
            CompressionAlgorithm::Dictionary => 2,
            CompressionAlgorithm::Lz77 => 3,
        }
    }
}

// ── CompressedData ────────────────────────────────────────────────────────────

/// Output of a compression operation.
#[derive(Debug, Clone)]
pub struct CompressedData {
    /// Algorithm used to produce this compressed payload.
    pub algorithm: CompressionAlgorithm,
    /// Size of the original (uncompressed) data in bytes.
    pub original_size: usize,
    /// The compressed bytes.
    pub compressed: Vec<u8>,
    /// Dictionary used by [`CompressionAlgorithm::Dictionary`], if any.
    pub dict: Option<Vec<Vec<u8>>>,
}

// ── Run-Length Encoding ───────────────────────────────────────────────────────

/// Encode `data` with run-length encoding.
///
/// Output format: repeated `(count: u8, byte: u8)` pairs.  A run longer than
/// 255 bytes is split into multiple pairs.
pub fn run_length_encode(data: &[u8]) -> Vec<u8> {
    if data.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(data.len() * 2);
    let mut i = 0;
    while i < data.len() {
        let byte = data[i];
        let mut count: usize = 1;
        while i + count < data.len() && data[i + count] == byte && count < 255 {
            count += 1;
        }
        out.push(count as u8);
        out.push(byte);
        i += count;
    }
    out
}

/// Decode run-length-encoded `data`.
pub fn run_length_decode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < data.len() {
        let count = data[i] as usize;
        let byte = data[i + 1];
        for _ in 0..count {
            out.push(byte);
        }
        i += 2;
    }
    out
}

// ── Dictionary Compression ────────────────────────────────────────────────────

/// Build a dictionary of the most frequent byte sequences of lengths 2–4 and
/// compress `data` by replacing occurrences with 1-byte dictionary indices.
///
/// Returns `(compressed_bytes, dictionary)`.  Bytes that don't match any
/// dictionary entry are emitted as a literal escape: `[0xFF, byte]`.  A
/// dictionary reference is emitted as `[0xFE, index]`.  The dictionary is
/// capped at 253 entries (indices 0x00–0xFC) so 0xFE and 0xFF are reserved.
pub fn dictionary_compress(data: &[u8], dict_size: usize) -> (Vec<u8>, Vec<Vec<u8>>) {
    // Count ngram frequencies.
    let mut freq: HashMap<Vec<u8>, usize> = HashMap::new();
    for len in 2usize..=4 {
        for start in 0..data.len().saturating_sub(len - 1) {
            let ngram = data[start..start + len].to_vec();
            *freq.entry(ngram).or_insert(0) += 1;
        }
    }

    // Build dictionary: top-N by frequency, only entries that appear >1 time.
    let cap = dict_size.min(253);
    let mut entries: Vec<(Vec<u8>, usize)> = freq.into_iter().filter(|(_, c)| *c > 1).collect();
    entries.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.len().cmp(&a.0.len())));
    let dict: Vec<Vec<u8>> = entries.into_iter().take(cap).map(|(k, _)| k).collect();

    // Build lookup map: bytes -> index.
    let mut lookup: HashMap<Vec<u8>, u8> = HashMap::new();
    for (i, seq) in dict.iter().enumerate() {
        lookup.insert(seq.clone(), i as u8);
    }

    // Compress greedily: try longest match first.
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let mut matched = false;
        for len in (2usize..=4).rev() {
            if i + len > data.len() {
                continue;
            }
            let slice = &data[i..i + len];
            if let Some(&idx) = lookup.get(slice) {
                out.push(0xFE_u8);
                out.push(idx);
                i += len;
                matched = true;
                break;
            }
        }
        if !matched {
            let b = data[i];
            if b == 0xFE || b == 0xFF {
                out.push(0xFF);
            }
            out.push(b);
            i += 1;
        }
    }

    (out, dict)
}

/// Decompress data compressed with [`dictionary_compress`].
pub fn dictionary_decompress(data: &[u8], dict: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        match data[i] {
            0xFE if i + 1 < data.len() => {
                let idx = data[i + 1] as usize;
                if idx < dict.len() {
                    out.extend_from_slice(&dict[idx]);
                }
                i += 2;
            }
            0xFF if i + 1 < data.len() => {
                out.push(data[i + 1]);
                i += 2;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

// ── LZ77 Compression ─────────────────────────────────────────────────────────

/// Simplified LZ77 compression.
///
/// Each token is 5 bytes: `[offset_hi, offset_lo, length, next_char, FLAG]`
/// where FLAG = 0x01 for back-references and 0x00 for literal tokens.
/// A literal token uses offset=0, length=0, next_char=byte.
pub fn lz77_compress(data: &[u8], window: usize, lookahead: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pos = 0;

    // Helper: emit a token (5 bytes).
    let emit = |out: &mut Vec<u8>, offset: u16, length: u8, next: u8, is_ref: bool| {
        out.push((offset >> 8) as u8);
        out.push((offset & 0xFF) as u8);
        out.push(length);
        out.push(next);
        out.push(if is_ref { 0x01 } else { 0x00 });
    };

    while pos < data.len() {
        let win_start = pos.saturating_sub(window);
        let la_end = (pos + lookahead).min(data.len());

        let mut best_offset: u16 = 0;
        let mut best_len: usize = 0;

        // Search the window for the longest match.
        for start in win_start..pos {
            let mut len = 0;
            while pos + len < la_end && data[start + len] == data[pos + len] {
                len += 1;
                if start + len >= pos {
                    break;
                }
            }
            if len > best_len {
                best_len = len;
                best_offset = (pos - start) as u16;
            }
        }

        if best_len >= 2 {
            let next = if pos + best_len < data.len() {
                data[pos + best_len]
            } else {
                0
            };
            emit(&mut out, best_offset, best_len as u8, next, true);
            pos += best_len + if pos + best_len < data.len() { 1 } else { 0 };
        } else {
            emit(&mut out, 0, 0, data[pos], false);
            pos += 1;
        }
    }

    out
}

/// Decompress data produced by [`lz77_compress`].
pub fn lz77_decompress(data: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut i = 0;

    while i + 4 < data.len() {
        let offset = ((data[i] as u16) << 8) | data[i + 1] as u16;
        let length = data[i + 2];
        let next_char = data[i + 3];
        let is_ref = data[i + 4] == 0x01;
        i += 5;

        if is_ref && offset > 0 && length > 0 {
            let start = out.len().saturating_sub(offset as usize);
            for j in 0..length as usize {
                let byte = out[start + j];
                out.push(byte);
            }
            if i <= data.len() {
                out.push(next_char);
            }
        } else {
            // Literal token.
            out.push(next_char);
        }
    }

    out
}

// ── CompressionEngine ─────────────────────────────────────────────────────────

/// High-level compression engine.
pub struct CompressionEngine;

impl CompressionEngine {
    /// Compress `data` using the specified algorithm.
    pub fn compress(data: &[u8], algo: CompressionAlgorithm) -> CompressedData {
        match algo {
            CompressionAlgorithm::None => CompressedData {
                algorithm: CompressionAlgorithm::None,
                original_size: data.len(),
                compressed: data.to_vec(),
                dict: None,
            },
            CompressionAlgorithm::RunLength => {
                let compressed = run_length_encode(data);
                CompressedData {
                    algorithm: CompressionAlgorithm::RunLength,
                    original_size: data.len(),
                    compressed,
                    dict: None,
                }
            }
            CompressionAlgorithm::Dictionary => {
                let (compressed, dict) = dictionary_compress(data, 128);
                CompressedData {
                    algorithm: CompressionAlgorithm::Dictionary,
                    original_size: data.len(),
                    compressed,
                    dict: Some(dict),
                }
            }
            CompressionAlgorithm::Lz77 => {
                let compressed = lz77_compress(data, 255, 16);
                CompressedData {
                    algorithm: CompressionAlgorithm::Lz77,
                    original_size: data.len(),
                    compressed,
                    dict: None,
                }
            }
        }
    }

    /// Decompress a [`CompressedData`] payload.
    pub fn decompress(compressed: &CompressedData) -> Vec<u8> {
        match &compressed.algorithm {
            CompressionAlgorithm::None => compressed.compressed.clone(),
            CompressionAlgorithm::RunLength => run_length_decode(&compressed.compressed),
            CompressionAlgorithm::Dictionary => {
                let dict = compressed.dict.as_deref().unwrap_or(&[]);
                dictionary_decompress(&compressed.compressed, dict)
            }
            CompressionAlgorithm::Lz77 => lz77_decompress(&compressed.compressed),
        }
    }

    /// Estimate Shannon entropy of `data` (0.0 = highly compressible, 8.0 = random).
    pub fn entropy(data: &[u8]) -> f64 {
        if data.is_empty() {
            return 0.0;
        }
        let mut counts = [0u64; 256];
        for &b in data {
            counts[b as usize] += 1;
        }
        let len = data.len() as f64;
        counts
            .iter()
            .filter(|&&c| c > 0)
            .map(|&c| {
                let p = c as f64 / len;
                -p * p.log2()
            })
            .sum()
    }

    /// Automatically select the best algorithm based on data entropy.
    pub fn auto_select(data: &[u8]) -> CompressionAlgorithm {
        if data.len() < 8 {
            return CompressionAlgorithm::None;
        }
        let e = Self::entropy(data);
        if e < 1.0 {
            // Very low entropy: lots of repeated bytes → RLE is ideal.
            CompressionAlgorithm::RunLength
        } else if e < 4.0 {
            // Moderate entropy: repeated patterns → dictionary works well.
            CompressionAlgorithm::Dictionary
        } else if e < 6.5 {
            // Higher entropy: sliding-window back-references.
            CompressionAlgorithm::Lz77
        } else {
            // Near-random: compression won't help.
            CompressionAlgorithm::None
        }
    }

    /// Compute the compression ratio.
    ///
    /// Returns values > 1.0 when compression succeeded, < 1.0 when it expanded.
    pub fn compression_ratio(original: usize, compressed: usize) -> f64 {
        if compressed == 0 {
            return f64::INFINITY;
        }
        original as f64 / compressed as f64
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    // ── RLE ──

    #[test]
    fn rle_empty() {
        assert!(run_length_encode(&[]).is_empty());
        assert!(run_length_decode(&[]).is_empty());
    }

    #[test]
    fn rle_roundtrip_simple() {
        let original: Vec<u8> = vec![0xAA, 0xAA, 0xAA, 0xBB, 0xBB, 0xCC];
        let encoded = run_length_encode(&original);
        let decoded = run_length_decode(&encoded);
        assert_eq!(decoded, original);
        // Should be shorter: 3 pairs instead of 6 bytes.
        assert!(encoded.len() < original.len());
    }

    #[test]
    fn rle_roundtrip_all_different() {
        let original: Vec<u8> = (0u8..=255).collect();
        let encoded = run_length_encode(&original);
        let decoded = run_length_decode(&encoded);
        assert_eq!(decoded, original);
    }

    #[test]
    fn rle_long_run() {
        let original = vec![0x42u8; 300];
        let encoded = run_length_encode(&original);
        let decoded = run_length_decode(&encoded);
        assert_eq!(decoded, original);
    }

    // ── Dictionary ──

    #[test]
    fn dict_roundtrip_repeated_pattern() {
        let pattern = b"hello world hello world hello world";
        let (compressed, dict) = dictionary_compress(pattern, 64);
        let decompressed = dictionary_decompress(&compressed, &dict);
        assert_eq!(decompressed, pattern);
    }

    #[test]
    fn dict_roundtrip_no_repetition() {
        let data: Vec<u8> = (0u8..=20).collect();
        let (compressed, dict) = dictionary_compress(&data, 64);
        let decompressed = dictionary_decompress(&compressed, &dict);
        assert_eq!(decompressed, data);
    }

    #[test]
    fn dict_empty() {
        let (c, d) = dictionary_compress(&[], 64);
        let dec = dictionary_decompress(&c, &d);
        assert!(dec.is_empty());
    }

    // ── LZ77 ──

    #[test]
    fn lz77_roundtrip_repetitive() {
        let data = b"abcabcabcabcabc";
        let compressed = lz77_compress(data, 64, 8);
        let decompressed = lz77_decompress(&compressed);
        assert_eq!(decompressed, data.as_ref());
    }

    #[test]
    fn lz77_roundtrip_literal() {
        let data = b"abcdef";
        let compressed = lz77_compress(data, 64, 8);
        let decompressed = lz77_decompress(&compressed);
        assert_eq!(decompressed, data.as_ref());
    }

    // ── CompressionEngine ──

    #[test]
    fn engine_none_roundtrip() {
        let data = b"hello";
        let cd = CompressionEngine::compress(data, CompressionAlgorithm::None);
        assert_eq!(CompressionEngine::decompress(&cd), data);
    }

    #[test]
    fn engine_rle_roundtrip() {
        let data = vec![0xAAu8; 100];
        let cd = CompressionEngine::compress(&data, CompressionAlgorithm::RunLength);
        let out = CompressionEngine::decompress(&cd);
        assert_eq!(out, data);
        assert!(cd.compressed.len() < data.len());
    }

    #[test]
    fn engine_dict_roundtrip() {
        let data = b"the quick brown fox jumps over the lazy dog the quick";
        let cd = CompressionEngine::compress(data, CompressionAlgorithm::Dictionary);
        let out = CompressionEngine::decompress(&cd);
        assert_eq!(out, data.as_ref());
    }

    #[test]
    fn engine_lz77_roundtrip() {
        let data = b"aaaaabbbbbcccccaaaaabbbbbccccc";
        let cd = CompressionEngine::compress(data, CompressionAlgorithm::Lz77);
        let out = CompressionEngine::decompress(&cd);
        assert_eq!(out, data.as_ref());
    }

    #[test]
    fn auto_select_low_entropy() {
        let data = vec![0x00u8; 1000];
        assert_eq!(
            CompressionEngine::auto_select(&data),
            CompressionAlgorithm::RunLength
        );
    }

    #[test]
    fn auto_select_high_entropy() {
        // Near-random data: all unique byte values repeated.
        let data: Vec<u8> = (0u8..=255).cycle().take(512).collect();
        // High entropy should pick None or Lz77; it should not be RunLength.
        let algo = CompressionEngine::auto_select(&data);
        assert_ne!(algo, CompressionAlgorithm::RunLength);
    }

    #[test]
    fn compression_ratio_calculation() {
        assert!((CompressionEngine::compression_ratio(100, 50) - 2.0).abs() < 1e-9);
        assert!((CompressionEngine::compression_ratio(50, 100) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn compression_level_ordering() {
        assert!(
            CompressionAlgorithm::None.compression_level()
                < CompressionAlgorithm::RunLength.compression_level()
        );
        assert!(
            CompressionAlgorithm::RunLength.compression_level()
                < CompressionAlgorithm::Dictionary.compression_level()
        );
        assert!(
            CompressionAlgorithm::Dictionary.compression_level()
                < CompressionAlgorithm::Lz77.compression_level()
        );
    }
}
