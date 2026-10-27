// huffmp3.rs - huffman reading/writing of MP3 spectral data.
// Faithful port of source/huffmp3.{h,cpp}.

use crate::bitops::{AbitReader, AbitWriter};
use crate::hufftables::{HuffmanCode, HuffmanConvSet};

// bit buffer size - careful changing this!
// should not be smaller than 8 or HWIDTH_MAX
const BIT_BUFFER_SIZE: i32 = 16;

/// small values quadruple conversion table (abs)
pub static SV_QCONV_ABS: [[u8; 4]; 16] = [
    [0, 0, 0, 0], // 0000 -> 0/0/0/0
    [0, 0, 0, 1], // 0001 -> 0/0/0/1
    [0, 0, 1, 0], // 0010 -> 0/0/1/0
    [0, 0, 1, 1], // 0011 -> 0/0/1/1
    [0, 1, 0, 0], // 0100 -> 0/1/0/0
    [0, 1, 0, 1], // 0101 -> 0/1/0/1
    [0, 1, 1, 0], // 0110 -> 0/1/1/0
    [0, 1, 1, 1], // 0111 -> 0/1/1/1
    [1, 0, 0, 0], // 1000 -> 1/0/0/0
    [1, 0, 0, 1], // 1001 -> 1/0/0/1
    [1, 0, 1, 0], // 1010 -> 1/0/1/0
    [1, 0, 1, 1], // 1011 -> 1/0/1/1
    [1, 1, 0, 0], // 1100 -> 1/1/0/0
    [1, 1, 0, 1], // 1101 -> 1/1/0/1
    [1, 1, 1, 0], // 1110 -> 1/1/1/0
    [1, 1, 1, 1], // 1111 -> 1/1/1/1
];

// ---------------------------------------------------------------------------
// huffman_reader - huffman decoding / data reading
// ---------------------------------------------------------------------------

pub struct HuffmanReader<'a> {
    bit_reader: AbitReader<'a>,
    bit_buffer: u32,
    count: i32,
}

impl<'a> HuffmanReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        // WARNING: no error checks in this class!
        let mut bit_reader = AbitReader::new(data);
        // fill the bit buffer for the first time
        let bit_buffer = bit_reader.read(BIT_BUFFER_SIZE);
        HuffmanReader {
            bit_reader,
            bit_buffer,
            count: 0,
        }
    }

    /// decode one big value pair
    pub fn decode_pair(&mut self, table: &[HuffmanConvSet], vals: &mut [u8; 2]) {
        let mut ext: i32 = 0;
        let mut conv;
        loop {
            let t = &table[ext as usize];
            conv = &t.h[(self.bit_buffer >> (BIT_BUFFER_SIZE - t.hwidth as i32)) as usize];
            self.advance_bitstream(conv.len as i32);
            if conv.ext == -1 {
                break;
            }
            ext = conv.ext;
        }
        vals[0] = conv.v0 as u8;
        vals[1] = conv.v1 as u8;
    }

    /// decode one small value quadruple
    pub fn decode_quadruple(&mut self, table: &HuffmanConvSet, vals: &mut [u8; 4]) {
        let conv = &table.h[(self.bit_buffer >> (BIT_BUFFER_SIZE - table.hwidth as i32)) as usize];
        self.advance_bitstream(conv.len as i32);
        vals.copy_from_slice(&SV_QCONV_ABS[conv.v0 as usize]);
    }

    /// bit reader function for n bits
    pub fn read_bits(&mut self, n: i32) -> u32 {
        // read bits
        let bits = if n <= BIT_BUFFER_SIZE {
            self.bit_buffer >> (BIT_BUFFER_SIZE - n)
        } else {
            (self.bit_buffer << (n - BIT_BUFFER_SIZE))
                | self.bit_reader.read(n - BIT_BUFFER_SIZE)
        };
        // refill the buffer
        self.advance_bitstream(n);
        bits
    }

    /// bit reader function for one bit
    pub fn read_bit(&mut self) -> u8 {
        // read one bit
        let bit = (self.bit_buffer >> (BIT_BUFFER_SIZE - 1)) as u8;
        // refill the buffer
        self.advance_bitstream_1();
        bit
    }

    /// reset the internal bit counter
    pub fn reset_counter(&mut self) {
        self.count = 0;
    }

    /// get the internal bit count
    pub fn get_count(&self) -> i32 {
        self.count
    }

    /// rewind n bits
    pub fn rewind_bits(&mut self, n: i32) {
        // rewind n bits
        self.bit_reader.rewind_bits(n + BIT_BUFFER_SIZE);
        // fill the bit buffer
        self.bit_buffer = self.bit_reader.read(BIT_BUFFER_SIZE);
        // set back the counter
        self.count = if self.count > n { self.count - n } else { 0 };
    }

    /// set position in stream
    pub fn setpos(&mut self, pbyte: i32, pbit: i32) {
        // reposition the bit reader
        self.bit_reader.setpos(pbyte, pbit);
        // fill the bit buffer
        self.bit_buffer = self.bit_reader.read(BIT_BUFFER_SIZE);
    }

    /// return current byte position
    pub fn getpos(&self) -> i32 {
        // ugly, but it works!
        self.bit_reader.getpos() - (BIT_BUFFER_SIZE + (8 - self.bit_reader.getbitp())) / 8
    }

    /// refill bit buffer utility function
    fn advance_bitstream(&mut self, n: i32) {
        // refill the buffer
        self.bit_buffer = if n >= BIT_BUFFER_SIZE {
            self.bit_reader.read(BIT_BUFFER_SIZE)
        } else {
            ((self.bit_buffer << n) | self.bit_reader.read(n)) & ((1 << BIT_BUFFER_SIZE) - 1)
        };
        // record # of bits read
        self.count += n;
    }

    /// refill bit buffer utility function (1 bit)
    fn advance_bitstream_1(&mut self) {
        // refill the buffer
        self.bit_buffer =
            ((self.bit_buffer << 1) | self.bit_reader.read_bit() as u32) & ((1 << BIT_BUFFER_SIZE) - 1);
        // increment counter
        self.count += 1;
    }
}

// ---------------------------------------------------------------------------
// huffman_writer - huffman encoding / data writer
// ---------------------------------------------------------------------------

pub struct HuffmanWriter {
    bit_writer: AbitWriter,
    count: i32,
}

impl HuffmanWriter {
    pub fn new(adds: i32) -> Self {
        // WARNING: no error checks in this class!
        // init bitwriter - recommended value: 5MB
        let adds = if adds == 0 { 5 * 1024 * 1024 } else { adds };
        HuffmanWriter {
            bit_writer: AbitWriter::new(adds),
            count: 0,
        }
    }

    /// encode one big value pair
    pub fn encode_pair(&mut self, hcodes: &[&[HuffmanCode]], vals: &[u8; 2]) {
        // find correct code, encode absvals and signs
        let hcode = &hcodes[vals[0] as usize][vals[1] as usize];
        self.write_bits(hcode.code, hcode.len as i32);
    }

    /// encode one small value quadruple
    pub fn encode_quadruple(&mut self, hcode: &[HuffmanCode], vals: &[u8; 4]) {
        // build bits unit
        let mut bits: usize = 0;
        for v in vals.iter() {
            bits <<= 1;
            if *v != 0 {
                bits |= 1;
            }
        }
        // encode vals
        let h = &hcode[bits];
        self.write_bits(h.code, h.len as i32);
    }

    /// bit writer function for n bits
    pub fn write_bits(&mut self, bits: u32, n: i32) {
        // write bits
        self.bit_writer.write(bits, n);
        // take count
        self.count += n;
    }

    /// bit writer function for 1 bit
    pub fn write_bit(&mut self, bit: u8) {
        // write bit
        self.bit_writer.write_bit(bit);
        // increment counter
        self.count += 1;
    }

    /// reset the internal bit counter
    pub fn reset_counter(&mut self) {
        self.count = 0;
    }

    /// get the internal bit count
    pub fn get_count(&self) -> i32 {
        self.count
    }

    /// return data pointer (when finished)
    pub fn getptr(&mut self) -> Vec<u8> {
        self.bit_writer.getptr()
    }

    /// return current byte position
    pub fn getpos(&self) -> usize {
        self.bit_writer.getpos()
    }
}
