// Round-trip tests for the Huffman reader/writer using the real MP3 tables.

use packmp3::huffmp3::{HuffmanReader, HuffmanWriter};
use packmp3::hufftables::{
    HTABA_DEC, HTABA_ENC, HTABB_DEC, HTABB_ENC, HTAB1_DEC, HTAB1_ENC, HTAB16_DEC, HTAB16_ENC,
    HTAB24_DEC, HTAB24_ENC,
};

fn encode_pairs(enc: &[&[packmp3::hufftables::HuffmanCode]], pairs: &[[u8; 2]]) -> Vec<u8> {
    let mut w = HuffmanWriter::new(0);
    for p in pairs {
        w.encode_pair(enc, p);
    }
    w.getptr()
}

fn decode_pairs(dec: &[packmp3::hufftables::HuffmanConvSet], bytes: &[u8], n: usize) -> Vec<[u8; 2]> {
    let mut r = HuffmanReader::new(bytes);
    let mut out = Vec::with_capacity(n);
    let mut v = [0u8; 2];
    for _ in 0..n {
        r.decode_pair(dec, &mut v);
        out.push(v);
    }
    out
}

#[test]
fn htab1_pair_roundtrip() {
    let pairs = vec![[0u8, 0], [0, 1], [1, 0], [1, 1]];
    let bytes = encode_pairs(HTAB1_ENC, &pairs);
    let decoded = decode_pairs(HTAB1_DEC, &bytes, pairs.len());
    assert_eq!(decoded, pairs);
}

#[test]
fn htab16_pair_roundtrip() {
    // all combos over the 16x16 absolute-value grid
    let mut pairs = Vec::new();
    for a in 0..16u8 {
        for b in 0..16u8 {
            pairs.push([a, b]);
        }
    }
    let bytes = encode_pairs(HTAB16_ENC, &pairs);
    let decoded = decode_pairs(HTAB16_DEC, &bytes, pairs.len());
    assert_eq!(decoded, pairs);
}

#[test]
fn htab24_pair_roundtrip() {
    let mut pairs = Vec::new();
    for a in 0..16u8 {
        for b in 0..16u8 {
            pairs.push([a, b]);
        }
    }
    let bytes = encode_pairs(HTAB24_ENC, &pairs);
    let decoded = decode_pairs(HTAB24_DEC, &bytes, pairs.len());
    assert_eq!(decoded, pairs);
}

fn encode_quads(enc: &[packmp3::hufftables::HuffmanCode], quads: &[[u8; 4]]) -> Vec<u8> {
    let mut w = HuffmanWriter::new(0);
    for q in quads {
        w.encode_quadruple(enc, q);
    }
    w.getptr()
}

fn decode_quads(dec: &packmp3::hufftables::HuffmanConvSet, bytes: &[u8], n: usize) -> Vec<[u8; 4]> {
    let mut r = HuffmanReader::new(bytes);
    let mut out = Vec::with_capacity(n);
    let mut v = [0u8; 4];
    for _ in 0..n {
        r.decode_quadruple(dec, &mut v);
        out.push(v);
    }
    out
}

#[test]
fn htaba_quad_roundtrip_all() {
    // every 4-bit pattern; SV_QCONV_ABS maps pattern -> [b3,b2,b1,b0]
    let mut quads = Vec::new();
    for p in 0..16u8 {
        quads.push([
            (p >> 3) & 1,
            (p >> 2) & 1,
            (p >> 1) & 1,
            p & 1,
        ]);
    }
    let bytes = encode_quads(HTABA_ENC, &quads);
    let decoded = decode_quads(&HTABA_DEC, &bytes, quads.len());
    assert_eq!(decoded, quads);
}

#[test]
fn htabb_quad_roundtrip() {
    let quads = vec![[0u8, 0, 0, 0], [1, 0, 0, 0], [0, 1, 0, 1], [1, 1, 1, 1]];
    let bytes = encode_quads(HTABB_ENC, &quads);
    let decoded = decode_quads(&HTABB_DEC, &bytes, quads.len());
    assert_eq!(decoded, quads);
}

#[test]
fn reader_bits_and_counter() {
    // write a known bit pattern, read it back via read_bits/read_bit
    let mut w = HuffmanWriter::new(0);
    w.write_bits(0x5A, 8);
    w.write_bit(1);
    w.write_bit(0);
    let bytes = w.getptr();

    let mut r = HuffmanReader::new(&bytes);
    r.reset_counter();
    assert_eq!(r.read_bits(4), 0x5);
    assert_eq!(r.get_count(), 4);
    assert_eq!(r.read_bits(4), 0xA);
    assert_eq!(r.read_bit(), 1);
    assert_eq!(r.read_bit(), 0);
    assert_eq!(r.get_count(), 10);
    r.rewind_bits(2);
    assert_eq!(r.get_count(), 8);
    assert_eq!(r.read_bit(), 1);
    assert_eq!(r.read_bit(), 0);
}

#[test]
fn getpos_tracks_consumed_bytes() {
    let mut w = HuffmanWriter::new(0);
    w.write_bits(0x1234, 16);
    w.write_bits(0x5678, 16);
    let bytes = w.getptr();
    assert_eq!(bytes.len(), 4);

    let mut r = HuffmanReader::new(&bytes);
    assert_eq!(r.getpos(), 0); // 16-bit prefetch consumed, position at unread data start
    let _ = r.read_bits(16);
    assert_eq!(r.getpos(), 2);
    let _ = r.read_bits(16);
    assert_eq!(r.getpos(), 2); // at end of data
}
