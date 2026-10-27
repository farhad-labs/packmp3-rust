// bitops.rs - bitwise and bytewise reading/writing of arrays, and the
// universal input/output stream abstraction.
//
// Faithful port of source/bitops.{h,cpp}.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// bit macros (from bitops.h)
// ---------------------------------------------------------------------------

#[inline]
fn rbits(c: u32, n: i32) -> u32 {
    // c & (0xFF >> (8 - n))
    c & (0xFFu32 >> (8 - n))
}

#[inline]
#[allow(dead_code)] // used by mp3.rs/pmp.rs port
fn lbits(c: u32, n: i32) -> u32 {
    c >> (8 - n)
}

#[inline]
fn mbits(c: u32, l: i32, r: i32) -> u32 {
    rbits(c, l) >> r
}

#[inline]
fn rbits32(c: u32, n: i32) -> u32 {
    c & (0xFFFF_FFFFu32 >> (32 - n))
}

#[inline]
fn mbits32(c: u32, l: i32, r: i32) -> u32 {
    rbits32(c, l) >> r
}

#[inline]
fn bitn(c: u32, n: i32) -> u32 {
    (c >> n) & 0x1
}

// stream type / mode constants
pub const TYPE_FILE: i32 = 0;
pub const TYPE_MEMORY: i32 = 1;
pub const TYPE_STREAM: i32 = 2;
pub const MODE_READ: i32 = 0;
pub const MODE_WRITE: i32 = 1;

// used by mp3.rs/pmp.rs port
#[allow(dead_code)]
const BTST_BUFF: usize = 1024 * 1024;
const ADDS: usize = 65536;

// ---------------------------------------------------------------------------
// abitreader - reads arrays bitwise
// ---------------------------------------------------------------------------

pub struct AbitReader<'a> {
    data: &'a [u8],
    lbyte: i32,
    cbyte: i32,
    cbit: i32,
    pub eof: bool,
    peof: i32,
}

impl<'a> AbitReader<'a> {
    pub fn new(array: &'a [u8]) -> Self {
        AbitReader {
            data: array,
            lbyte: array.len() as i32,
            cbyte: 0,
            cbit: 8,
            peof: 0,
            eof: false,
        }
    }

    pub fn read(&mut self, mut nbits: i32) -> u32 {
        let mut retval: u32 = 0;
        // safety check for eof
        if self.eof {
            self.peof += nbits;
            return 0;
        }
        while nbits >= self.cbit {
            nbits -= self.cbit;
            retval |= rbits(self.data[self.cbyte as usize] as u32, self.cbit) << nbits;
            self.cbit = 8;
            self.cbyte += 1;
            if self.cbyte >= self.lbyte {
                self.peof = nbits;
                self.eof = true;
                return retval;
            }
        }
        if nbits > 0 {
            retval |= mbits(
                self.data[self.cbyte as usize] as u32,
                self.cbit,
                self.cbit - nbits,
            );
            self.cbit -= nbits;
        }
        retval
    }

    pub fn read_bit(&mut self) -> u8 {
        // safety check for eof
        if self.eof {
            self.peof += 1;
            return 0;
        }
        self.cbit -= 1;
        let bit = bitn(self.data[self.cbyte as usize] as u32, self.cbit) as u8;
        if self.cbit == 0 {
            self.cbyte += 1;
            if self.cbyte == self.lbyte {
                self.eof = true;
            }
            self.cbit = 8;
        }
        bit
    }

    /// skip padding from current byte
    pub fn unpad(&mut self, mut fillbit: u8) -> u8 {
        if self.cbit == 8 || self.eof {
            fillbit
        } else {
            fillbit = self.read(1) as u8;
            while self.cbit != 8 {
                self.read(1);
            }
            fillbit
        }
    }

    pub fn getpos(&self) -> i32 {
        self.cbyte
    }

    pub fn getbitp(&self) -> i32 {
        self.cbit
    }

    pub fn setpos(&mut self, pbyte: i32, pbit: i32) {
        if pbyte < self.lbyte {
            self.eof = false;
            self.cbyte = pbyte;
            self.cbit = pbit;
        } else {
            self.eof = true;
            self.cbyte = self.lbyte;
            self.cbit = 8;
            self.peof = ((pbyte - self.lbyte) * 8) + 8 - pbit;
        }
    }

    pub fn rewind_bits(&mut self, mut nbits: i32) {
        if self.eof {
            if nbits > self.peof {
                nbits -= self.peof;
            } else {
                self.peof -= nbits;
                return;
            }
            self.eof = false;
        }
        self.cbit += nbits;
        while self.cbit > 8 {
            self.cbyte -= 1;
            self.cbit -= 8;
        }
        if self.cbyte < 0 {
            self.cbyte = 0;
            self.cbit = 8;
        }
    }
}

// ---------------------------------------------------------------------------
// abitwriter - writes arrays bitwise
// ---------------------------------------------------------------------------

pub struct AbitWriter {
    fillbit: u8,
    data: Vec<u8>,
    dsize: usize,
    adds: usize,
    cbyte: usize,
    cbit: i32,
    pub error: bool,
}

impl AbitWriter {
    pub fn new(size: i32) -> Self {
        let dsize = if size > 0 { size as usize } else { ADDS };
        AbitWriter {
            fillbit: 1,
            data: vec![0u8; dsize],
            dsize,
            adds: ADDS,
            cbyte: 0,
            cbit: 8,
            error: false,
        }
    }

    fn grow(&mut self) {
        self.dsize += self.adds;
        self.data.resize(self.dsize, 0);
    }

    pub fn write(&mut self, val: u32, mut nbits: i32) {
        if self.error {
            return;
        }
        // test if pointer beyond flush threshold
        if self.cbyte > (self.dsize - 5) {
            self.grow();
        }
        while nbits >= self.cbit {
            let x = self.data[self.cbyte] as u32;
            self.data[self.cbyte] = (x | mbits32(val, nbits, nbits - self.cbit)) as u8;
            nbits -= self.cbit;
            self.cbyte += 1;
            self.cbit = 8;
        }
        if nbits > 0 {
            let x = self.data[self.cbyte] as u32;
            let v = rbits32(val, nbits) << (self.cbit - nbits);
            self.data[self.cbyte] = (x | v) as u8;
            self.cbit -= nbits;
        }
    }

    pub fn write_bit(&mut self, bit: u8) {
        if self.error {
            return;
        }
        if bit != 0 {
            self.cbit -= 1;
            self.data[self.cbyte] |= 0x1 << self.cbit;
        } else {
            self.cbit -= 1;
        }
        if self.cbit == 0 {
            self.cbyte += 1;
            if self.cbyte > (self.dsize - 5) {
                self.grow();
            }
            self.cbit = 8;
        }
    }

    /// pads data using fillbit
    pub fn pad(&mut self, fillbit: u8) {
        while self.cbit < 8 {
            self.write(fillbit as u32, 1);
        }
    }

    /// returns data array (padded). Takes ownership of the buffer.
    pub fn getptr(&mut self) -> Vec<u8> {
        self.pad(self.fillbit);
        self.data.truncate(self.cbyte);
        std::mem::take(&mut self.data)
    }

    pub fn getpos(&self) -> usize {
        self.cbyte
    }

    pub fn getbitp(&self) -> i32 {
        self.cbit
    }
}

// ---------------------------------------------------------------------------
// abytereader - reads arrays bytewise
// ---------------------------------------------------------------------------

pub struct AbyteReader<'a> {
    data: &'a [u8],
    lbyte: usize,
    cbyte: usize,
    pub eof: bool,
}

impl<'a> AbyteReader<'a> {
    pub fn new(array: &'a [u8]) -> Self {
        let lbyte = array.len();
        AbyteReader {
            data: array,
            lbyte,
            cbyte: 0,
            eof: array.is_empty(),
        }
    }

    /// reads 1 byte; returns true if a byte was read
    pub fn read(&mut self, byte: &mut u8) -> bool {
        if self.cbyte >= self.lbyte {
            self.cbyte = self.lbyte;
            self.eof = true;
            return false;
        }
        *byte = self.data[self.cbyte];
        self.cbyte += 1;
        true
    }

    /// reads n bytes; returns number of bytes actually read
    pub fn read_n(&mut self, byte: &mut [u8], n: usize) -> usize {
        let nl = self.lbyte - self.cbyte;
        if nl < n {
            byte[..nl].copy_from_slice(&self.data[self.cbyte..self.cbyte + nl]);
            self.cbyte = self.lbyte;
            self.eof = true;
            nl
        } else {
            byte[..n].copy_from_slice(&self.data[self.cbyte..self.cbyte + n]);
            self.cbyte += n;
            n
        }
    }

    pub fn seek(&mut self, pos: usize) {
        if pos >= self.lbyte {
            self.cbyte = self.lbyte;
            self.eof = true;
        } else {
            self.cbyte = pos;
            self.eof = false;
        }
    }

    pub fn getsize(&self) -> usize {
        self.lbyte
    }

    pub fn getpos(&self) -> usize {
        self.cbyte
    }
}

// ---------------------------------------------------------------------------
// abytewriter - writes arrays bytewise
// ---------------------------------------------------------------------------

pub struct AbyteWriter {
    data: Vec<u8>,
    dsize: usize,
    adds: usize,
    cbyte: usize,
    pub error: bool,
}

impl AbyteWriter {
    pub fn new(size: i32) -> Self {
        let dsize = if size > 0 { size as usize } else { ADDS };
        AbyteWriter {
            data: vec![0u8; dsize],
            dsize,
            adds: ADDS,
            cbyte: 0,
            error: false,
        }
    }

    pub fn write(&mut self, byte: u8) {
        if self.error {
            return;
        }
        if self.cbyte >= (self.dsize - 2) {
            self.dsize += self.adds;
            self.data.resize(self.dsize, 0);
        }
        self.data[self.cbyte] = byte;
        self.cbyte += 1;
    }

    pub fn write_n(&mut self, byte: &[u8]) {
        if self.error {
            return;
        }
        let n = byte.len();
        while (self.cbyte + n) >= (self.dsize - 2) {
            self.dsize += self.adds;
            self.data.resize(self.dsize, 0);
        }
        self.data[self.cbyte..self.cbyte + n].copy_from_slice(byte);
        self.cbyte += n;
    }

    /// returns data array; takes ownership of the buffer
    pub fn getptr(&mut self) -> Vec<u8> {
        if self.error {
            return Vec::new();
        }
        self.data.truncate(self.cbyte);
        std::mem::take(&mut self.data)
    }

    pub fn peekptr(&self) -> &[u8] {
        &self.data[..self.cbyte]
    }

    pub fn getpos(&self) -> usize {
        self.cbyte
    }

    /// reset without realloc
    pub fn reset(&mut self) {
        self.cbyte = 0;
    }
}

// ---------------------------------------------------------------------------
// iostream - input and output from file, memory or stdin/stdout
// ---------------------------------------------------------------------------

/// Source descriptor for an IoStream.
pub enum IoSource<'a> {
    File(&'a str),
    Memory(&'a [u8]),
    Stdio,
}

pub struct IoStream {
    srct: i32,
    mode: i32,
    /// data buffer: input data (read mode) or accumulated output (write mode)
    buf: Vec<u8>,
    /// read position
    pos: usize,
    /// set when a read reaches the end (mirrors feof / mrdr->eof)
    eof: bool,
    /// output file, kept open like fopen "wb" so write errors are detectable
    file: Option<File>,
    path: Option<PathBuf>,
    error: bool,
}

impl IoStream {
    pub fn new(src: &IoSource, srctype: i32, srcsize: i32, iomode: i32) -> Self {
        let mut s = IoStream {
            srct: srctype,
            mode: iomode,
            buf: Vec::new(),
            pos: 0,
            eof: false,
            file: None,
            path: None,
            error: false,
        };
        match srctype {
            TYPE_FILE => {
                let path = match src {
                    IoSource::File(f) => f.to_string(),
                    _ => {
                        s.error = true;
                        return s;
                    }
                };
                if iomode == MODE_READ {
                    match File::open(&path) {
                        Ok(mut f) => {
                            let mut buf = Vec::new();
                            match f.read_to_end(&mut buf) {
                                Ok(_) => s.buf = buf,
                                Err(_) => s.error = true,
                            }
                        }
                        Err(_) => s.error = true,
                    }
                } else {
                    // open for writing now, like fopen(path, "wb") -> truncates
                    match File::create(&path) {
                        Ok(f) => {
                            s.file = Some(f);
                            s.path = Some(PathBuf::from(path));
                        }
                        Err(_) => s.error = true,
                    }
                }
            }
            TYPE_MEMORY => {
                let data = match src {
                    IoSource::Memory(d) => d,
                    _ => {
                        s.error = true;
                        return s;
                    }
                };
                let size = if srcsize > 0 { srcsize as usize } else { data.len() };
                s.buf = data[..size].to_vec();
            }
            TYPE_STREAM => {
                let _ = srcsize;
                if iomode == MODE_READ {
                    // read whole stream into memory buffer
                    let mut buf = Vec::new();
                    match io::stdin().lock().read_to_end(&mut buf) {
                        Ok(_) => s.buf = buf,
                        Err(_) => s.error = true,
                    }
                }
                // for writing: buffer in memory, flush to stdout at drop
            }
            _ => s.error = true,
        }
        s
    }

    /// switches mode from reading to writing and vice versa
    pub fn switch_mode(&mut self) {
        if self.chkerr() {
            return;
        }
        if self.mode == MODE_READ {
            // switching from reading to writing: information might be lost
            self.buf.clear();
            self.pos = 0;
            self.eof = false;
            self.mode = MODE_WRITE;
        } else {
            // switching from writing to reading
            // for file output, flush the buffered data to disk first so the
            // on-disk file matches (mirrors C++ fflush/fclose/fopen "rb").
            if self.srct == TYPE_FILE {
                self.flush_file();
            }
            self.pos = 0;
            self.eof = false;
            self.mode = MODE_READ;
        }
    }

    /// generic read: fills `to[..dtsize]`, returns number of items read
    pub fn read(&mut self, to: &mut [u8], tpsize: i32, dtsize: i32) -> i32 {
        let n = tpsize * dtsize;
        if n <= 0 {
            return 0;
        }
        let avail = (self.buf.len() - self.pos).min(n as usize);
        to[..avail].copy_from_slice(&self.buf[self.pos..self.pos + avail]);
        self.pos += avail;
        if self.pos >= self.buf.len() {
            self.eof = true;
        }
        (avail as i32) / tpsize
    }

    /// generic write
    pub fn write(&mut self, from: &[u8], tpsize: i32, dtsize: i32) -> i32 {
        let n = tpsize * dtsize;
        if self.error {
            return 0;
        }
        self.buf.extend_from_slice(&from[..n as usize]);
        n
    }

    pub fn flush(&mut self) -> i32 {
        self.flush_file();
        self.getpos() as i32
    }

    fn flush_file(&mut self) {
        if self.srct == TYPE_FILE && self.mode == MODE_WRITE {
            if let Some(f) = self.file.as_mut() {
                if f.write_all(&self.buf).is_err() {
                    self.error = true;
                }
                if f.flush().is_err() {
                    self.error = true;
                }
            }
        }
    }

    /// rewind to beginning of stream
    pub fn rewind(&mut self) -> i32 {
        self.pos = 0;
        self.eof = false;
        self.getpos() as i32
    }

    pub fn getpos(&self) -> usize {
        if self.mode == MODE_READ {
            self.pos
        } else {
            self.buf.len()
        }
    }

    pub fn getsize(&self) -> usize {
        if self.mode == MODE_READ {
            self.buf.len()
        } else {
            self.getpos()
        }
    }

    /// data pointer (for mem io only)
    pub fn getptr(&mut self) -> Vec<u8> {
        if self.srct == TYPE_MEMORY {
            if self.mode == MODE_READ {
                self.buf.clone()
            } else {
                std::mem::take(&mut self.buf)
            }
        } else {
            Vec::new()
        }
    }

    pub fn chkerr(&self) -> bool {
        if (self.mode != 0) && (self.mode != 1) {
            return true;
        }
        if (self.srct != 0) && (self.srct != 1) && (self.srct != 2) {
            return true;
        }
        if self.srct == TYPE_FILE {
            if self.mode == MODE_READ {
                if self.error {
                    return true;
                }
            } else {
                if self.file.is_none() {
                    return true;
                }
            }
        } else if self.error {
            return true;
        }
        false
    }

    /// check for eof (read only)
    pub fn chkeof(&self) -> bool {
        if self.mode == MODE_READ {
            self.eof
        } else {
            false
        }
    }
}

impl Drop for IoStream {
    fn drop(&mut self) {
        if self.srct == TYPE_STREAM && self.mode == MODE_WRITE {
            let _ = io::stdout().lock().write_all(&self.buf);
        } else if self.srct == TYPE_FILE && self.mode == MODE_WRITE {
            self.flush_file();
        }
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abit_roundtrip() {
        let mut w = AbitWriter::new(0);
        // write known patterns MSB-first
        w.write(0xFF, 8);
        w.write(0x00, 8);
        w.write(0b1010_1010, 8);
        w.write(0x55, 8);
        w.write(0b1, 1);
        w.write(0b0, 1);
        w.write(0xFFFF_FFFF, 32);
        let data = w.getptr();
        // byte 5: bit 1 then bit 0, then pad(6 bits, fillbit=1) => 0b10_111111 = 0xBF
        assert_eq!(
            data,
            vec![0xFF, 0x00, 0xAA, 0x55, 0xBF, 0xFF, 0xFF, 0xFF, 0xFF]
        );

        let mut r = AbitReader::new(&data);
        assert_eq!(r.read(8), 0xFF);
        assert_eq!(r.read(8), 0x00);
        assert_eq!(r.read(8), 0xAA);
        assert_eq!(r.read(8), 0x55);
        assert_eq!(r.read(1), 1);
        assert_eq!(r.read(1), 0);
        assert_eq!(r.read(32), 0xFFFF_FFFF);
        assert!(!r.eof);
    }

    #[test]
    fn abit_write_bit() {
        let mut w = AbitWriter::new(0);
        for i in 0..8u32 {
            w.write_bit(((i & 1) != 0) as u8);
        }
        for i in 0..8u32 {
            w.write_bit(((i & 1) != 0) as u8);
        }
        let data = w.getptr();
        // 0,1,0,1,0,1,0,1 MSB-first => 0x55
        assert_eq!(data, vec![0x55, 0x55]);
        let mut r = AbitReader::new(&data);
        for i in 0..8u32 {
            assert_eq!(r.read_bit(), ((i & 1) != 0) as u8);
        }
        for i in 0..8u32 {
            assert_eq!(r.read_bit(), ((i & 1) != 0) as u8);
        }
    }

    #[test]
    fn abit_partial_bytes() {
        // 3 bits + 5 bits = one byte
        let mut w = AbitWriter::new(0);
        w.write(0b101, 3);
        w.write(0b01010, 5);
        let data = w.getptr();
        assert_eq!(data, vec![0b101_01010]);
        let mut r = AbitReader::new(&data);
        assert_eq!(r.read(3), 0b101);
        assert_eq!(r.read(5), 0b01010);
        assert_eq!(r.getbitp(), 8);
    }

    #[test]
    fn abit_rewind_and_setpos() {
        let data = [0xAB, 0xCD, 0xEF, 0x01];
        let mut r = AbitReader::new(&data);
        assert_eq!(r.read(4), 0xA);
        assert_eq!(r.read(4), 0xB);
        assert_eq!(r.getpos(), 1);
        // C++ rewind_bits(4) from a byte boundary moves back to cbyte 0, cbit 4,
        // where the next read consumes the 4 low bits (0xB)
        r.rewind_bits(4);
        assert_eq!(r.getbitp(), 4);
        assert_eq!(r.read(4), 0xB);
        assert_eq!(r.getpos(), 1);
        r.setpos(3, 8);
        assert_eq!(r.read(8), 0x01);
        assert!(r.eof); // reading the last byte sets eof
    }

    #[test]
    fn abit_eof_read() {
        let data = [0x80];
        let mut r = AbitReader::new(&data);
        assert_eq!(r.read(1), 1);
        assert_eq!(r.getbitp(), 7);
        // reading beyond end returns 0 and sets eof; peof tracks excess bits
        assert_eq!(r.read(16), 0);
        assert!(r.eof);
        // 7 bits remained after the first read; 16-7 = 9 excess
        assert_eq!(r.peof, 9);
    }

    #[test]
    fn abit_unpad() {
        let data = [0b1010_0000];
        let mut r = AbitReader::new(&data);
        assert_eq!(r.read(1), 1);
        assert_eq!(r.unpad(0), 0); // fillbit from padding
        assert_eq!(r.getbitp(), 8);
        assert_eq!(r.read(1), 0); // next byte boundary (zeroed by reader)
    }

    #[test]
    fn a_byte_roundtrip() {
        let mut w = AbyteWriter::new(0);
        w.write(0x11);
        w.write_n(&[0x22, 0x33, 0x44]);
        let data = w.getptr();
        assert_eq!(data, vec![0x11, 0x22, 0x33, 0x44]);

        let mut r = AbyteReader::new(&data);
        let mut b = 0u8;
        assert!(r.read(&mut b));
        assert_eq!(b, 0x11);
        let mut buf = [0u8; 4];
        assert_eq!(r.read_n(&mut buf, 4), 3);
        assert_eq!(buf[..3], [0x22, 0x33, 0x44]);
        assert!(r.eof);
    }

    #[test]
    fn a_byte_seek() {
        let data = [1u8, 2, 3, 4, 5];
        let mut r = AbyteReader::new(&data);
        r.seek(2);
        assert_eq!(r.getpos(), 2);
        let mut b = 0u8;
        r.read(&mut b);
        assert_eq!(b, 3);
        r.seek(100);
        assert!(r.eof);
    }

    #[test]
    fn iostream_memory_roundtrip() {
        let input = [0u8, 1, 2, 3, 4, 5, 6, 7];
        let mut s = IoStream::new(&IoSource::Memory(&input), TYPE_MEMORY, 0, MODE_READ);
        assert!(!s.chkerr());
        assert_eq!(s.getsize(), 8);
        let mut buf = [0u8; 4];
        assert_eq!(s.read(&mut buf, 1, 4), 4);
        assert_eq!(buf, [0, 1, 2, 3]);
        assert_eq!(s.read(&mut buf, 1, 4), 4);
        assert_eq!(s.read(&mut buf, 1, 4), 0);
        assert!(s.chkeof());
        s.rewind();
        assert!(!s.chkeof());
        assert_eq!(s.getpos(), 0);
    }

    #[test]
    fn iostream_write_then_read() {
        // memory writer, then switch to read and read back
        let mut s = IoStream::new(&IoSource::Memory(&[]), TYPE_MEMORY, 0, MODE_WRITE);
        assert_eq!(s.write(&[0xDE, 0xAD], 1, 2), 2);
        assert_eq!(s.getsize(), 2);
        s.switch_mode();
        assert!(!s.chkerr());
        let mut buf = [0u8; 2];
        assert_eq!(s.read(&mut buf, 1, 2), 2);
        assert_eq!(buf, [0xDE, 0xAD]);
    }

    #[test]
    fn iostream_memory_slice_size() {
        let input = [9u8, 8, 7, 6];
        let mut s = IoStream::new(&IoSource::Memory(&input), TYPE_MEMORY, 2, MODE_READ);
        // srcsize=2 caps the memory source
        assert_eq!(s.getsize(), 2);
        let mut buf = [0u8; 4];
        assert_eq!(s.read(&mut buf, 1, 4), 2);
        assert!(s.chkeof());
    }
}
