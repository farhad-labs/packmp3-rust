// Round-trip tests for the arithmetic coder and statistical models.

use packmp3::aricoder::{
    decode_ari_b, decode_ari_s, encode_ari_b, encode_ari_s, Aricoder, ModelB, ModelS,
};
use packmp3::bitops::{IoSource, IoStream, MODE_READ, MODE_WRITE, TYPE_MEMORY};

fn encode_s(data: &[i32], max_s: i32, max_c: i32, max_o: i32, c_lim: i32) -> Vec<u8> {
    let mut stream = IoStream::new(&IoSource::Memory(&[]), TYPE_MEMORY, 0, MODE_WRITE);
    let mut ac = Aricoder::new(&mut stream, MODE_WRITE);
    let mut model = ModelS::new(max_s, max_c, max_o, c_lim);
    for &c in data {
        encode_ari_s(&mut ac, &mut model, c);
    }
    ac.flush();
    stream.getptr()
}

fn decode_s(data: &[u8], count: usize, max_s: i32, max_c: i32, max_o: i32, c_lim: i32) -> Vec<i32> {
    let mut stream = IoStream::new(&IoSource::Memory(data), TYPE_MEMORY, 0, MODE_READ);
    let mut ac = Aricoder::new(&mut stream, MODE_READ);
    let mut model = ModelS::new(max_s, max_c, max_o, c_lim);
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        out.push(decode_ari_s(&mut ac, &mut model));
    }
    out
}

#[test]
fn model_s_roundtrip_basic() {
    let data: Vec<i32> = (0..256).cycle().take(2000).collect();
    let encoded = encode_s(&data, 256, 2, 2, 511);
    let decoded = decode_s(&encoded, data.len(), 256, 2, 2, 511);
    assert_eq!(decoded, data);
}

#[test]
fn model_s_roundtrip_skewed() {
    // skewed distribution to stress escape handling (many escapes initially)
    let data: Vec<i32> = (0..300).map(|i| (i * 37) % 200 + 20).collect();
    let encoded = encode_s(&data, 256, 4, 3, 1024);
    let decoded = decode_s(&encoded, data.len(), 256, 4, 3, 1024);
    assert_eq!(decoded, data);
}

#[test]
fn model_s_roundtrip_small_alphabet() {
    let data: Vec<i32> = (0..500).map(|i| (i * 7) % 8).collect();
    let encoded = encode_s(&data, 8, 2, 1, 511);
    let decoded = decode_s(&encoded, data.len(), 8, 2, 1, 511);
    assert_eq!(decoded, data);
}

#[test]
fn model_s_roundtrip_constant() {
    // constant stream: only one symbol ever appears
    let data: Vec<i32> = vec![42; 500];
    let encoded = encode_s(&data, 64, 2, 2, 511);
    let decoded = decode_s(&encoded, data.len(), 64, 2, 2, 511);
    assert_eq!(decoded, data);
}

fn encode_b(data: &[i32], max_c: i32, max_o: i32, c_lim: i32) -> Vec<u8> {
    let mut stream = IoStream::new(&IoSource::Memory(&[]), TYPE_MEMORY, 0, MODE_WRITE);
    let mut ac = Aricoder::new(&mut stream, MODE_WRITE);
    let mut model = ModelB::new(max_c, max_o, c_lim);
    for &c in data {
        encode_ari_b(&mut ac, &mut model, c);
    }
    ac.flush();
    stream.getptr()
}

fn decode_b(data: &[u8], count: usize, max_c: i32, max_o: i32, c_lim: i32) -> Vec<i32> {
    let mut stream = IoStream::new(&IoSource::Memory(data), TYPE_MEMORY, 0, MODE_READ);
    let mut ac = Aricoder::new(&mut stream, MODE_READ);
    let mut model = ModelB::new(max_c, max_o, c_lim);
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        out.push(decode_ari_b(&mut ac, &mut model));
    }
    out
}

#[test]
fn model_b_roundtrip() {
    let data: Vec<i32> = (0..3000).map(|i| ((i * 13) % 7 == 0) as i32).collect();
    let encoded = encode_b(&data, 2, 2, 511);
    let decoded = decode_b(&encoded, data.len(), 2, 2, 511);
    assert_eq!(decoded, data);
}

#[test]
fn model_b_roundtrip_order_minus_one() {
    // max_o = -1 uses only the null table
    let data: Vec<i32> = (0..200).map(|i| i % 2).collect();
    let encoded = encode_b(&data, 4, -1, 511);
    let decoded = decode_b(&encoded, data.len(), 4, -1, 511);
    assert_eq!(decoded, data);
}

#[test]
fn encode_decode_are_deterministic() {
    let data: Vec<i32> = (0..1000).map(|i| (i * 31) % 64).collect();
    let a = encode_s(&data, 64, 3, 2, 511);
    let b = encode_s(&data, 64, 3, 2, 511);
    assert_eq!(a, b);
}
