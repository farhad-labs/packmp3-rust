// aricoder.rs - arithmetic coding engine and statistical models.
// Faithful port of source/aricoder.{h,cpp}.
//
// The C++ code uses raw `unsigned int` (32-bit wrapping) arithmetic and
// hand-managed linked context tables. We replicate the wrapping semantics with
// `u32` (see wrapping() helper) and replace the pointer graph with an arena of
// index-addressed tables, which is acyclic when traversed through `links`.

use crate::bitops::{IoStream, MODE_READ, MODE_WRITE};

// defines for coder
pub const CODER_USE_BITS: u32 = 31; // must never be above 31
pub const CODER_LIMIT100: u32 = 1 << CODER_USE_BITS;
#[allow(clippy::identity_op)] // mirrors the C++ macro (…/4)*1
pub const CODER_LIMIT025: u32 = (CODER_LIMIT100 / 4) * 1;
pub const CODER_LIMIT050: u32 = (CODER_LIMIT100 / 4) * 2;
pub const CODER_LIMIT075: u32 = (CODER_LIMIT100 / 4) * 3;
pub const CODER_MAXSCALE: u32 = CODER_LIMIT025 - 1;
pub const ESCAPE_SYMBOL: u32 = CODER_LIMIT025;

/// 32-bit wrapping multiply (mirrors C++ `unsigned int` arithmetic)
#[inline]
fn wmul(a: u32, b: u32) -> u32 {
    a.wrapping_mul(b)
}

/// 32-bit wrapping add
#[inline]
fn wadd(a: u32, b: u32) -> u32 {
    a.wrapping_add(b)
}

/// 32-bit wrapping sub
#[inline]
fn wsub(a: u32, b: u32) -> u32 {
    a.wrapping_sub(b)
}

// symbol struct, used in arithmetic coding
#[derive(Clone, Copy, Default)]
pub struct Symbol {
    pub low_count: u32,
    pub high_count: u32,
    pub scale: u32,
}

// ---------------------------------------------------------------------------
// aricoder
// ---------------------------------------------------------------------------

pub struct Aricoder<'a> {
    stream: &'a mut IoStream,
    mode: i32,
    bbyte: u8,
    cbit: u8,

    ccode: u32,
    clow: u32,
    chigh: u32,
    cstep: u32,
    nrbits: u32,
}

impl<'a> Aricoder<'a> {
    pub fn new(stream: &'a mut IoStream, iomode: i32) -> Self {
        let mut ac = Aricoder {
            stream,
            mode: iomode,
            bbyte: 0,
            cbit: 0,
            ccode: 0,
            clow: 0,
            chigh: CODER_LIMIT100 - 1,
            cstep: 0,
            nrbits: 0,
        };
        if ac.mode == MODE_READ {
            // code buffer has to be filled before starting decoding
            for _ in 0..CODER_USE_BITS {
                ac.ccode = (ac.ccode << 1) | ac.read_bit() as u32;
            }
        }
        ac
    }

    /// encoding: flush remaining bits
    pub fn flush(&mut self) {
        if self.mode != MODE_WRITE {
            return;
        }
        // due to clow < CODER_LIMIT050, and chigh >= CODER_LIMIT050
        // there are only two possible cases
        if self.clow < CODER_LIMIT025 {
            // case a.)
            self.write_bit(0);
            // write remaining bits
            self.write_bit(1);
            while self.nrbits > 0 {
                self.nrbits -= 1;
                self.write_bit(1);
            }
        } else {
            // case b.), clow >= CODER_LIMIT025
            self.write_bit(1);
        } // done, zeroes are auto-read by the decoder

        // pad code with zeroes
        while self.cbit > 0 {
            self.write_bit(0);
        }
    }

    pub fn encode(&mut self, s: &Symbol) {
        // update steps, low count, high count
        self.cstep = (wsub(self.chigh, self.clow) + 1) / s.scale;
        self.chigh = wsub(wadd(self.clow, wmul(self.cstep, s.high_count)), 1);
        self.clow = wadd(self.clow, wmul(self.cstep, s.low_count));

        // e3 scaling is performed for speed and to avoid underflows
        // if both, low and high are either in the lower half or in the higher half
        // one bit can be safely shifted out
        while (self.clow >= CODER_LIMIT050) || (self.chigh < CODER_LIMIT050) {
            if self.chigh < CODER_LIMIT050 {
                // this means both, high and low are below, and 0 can be safely shifted out
                // write 0 bit
                self.write_bit(0);
                // shift out remaining e3 bits
                while self.nrbits > 0 {
                    self.nrbits -= 1;
                    self.write_bit(1);
                }
            } else {
                // if the first wasn't the case, it's clow >= CODER_LIMIT050
                // write 1 bit
                self.write_bit(1);
                self.clow &= CODER_LIMIT050 - 1;
                self.chigh &= CODER_LIMIT050 - 1;
                // shift out remaining e3 bits
                while self.nrbits > 0 {
                    self.nrbits -= 1;
                    self.write_bit(0);
                }
            }
            self.clow <<= 1;
            self.chigh <<= 1;
            self.chigh = wadd(self.chigh, 1);
        }

        // e3 scaling, to make sure that there's enough space between low and high
        while (self.clow >= CODER_LIMIT025) && (self.chigh < CODER_LIMIT075) {
            self.nrbits += 1;
            self.clow &= CODER_LIMIT025 - 1;
            self.chigh ^= CODER_LIMIT025 + CODER_LIMIT050;
            self.clow <<= 1;
            self.chigh <<= 1;
            self.chigh = wadd(self.chigh, 1);
        }
    }

    /// arithmetic decoder get count function
    pub fn decode_count(&mut self, s: &Symbol) -> u32 {
        // update cstep, which is needed to remove the symbol from the stream later
        self.cstep = (wsub(self.chigh, self.clow) + 1) / s.scale;
        // return counts, needed to decode the symbol from the statistical model
        wsub(self.ccode, self.clow) / self.cstep
    }

    /// arithmetic decoder function
    pub fn decode(&mut self, s: &Symbol) {
        // already have steps updated from decode_count
        // update low count and high count
        self.chigh = wsub(wadd(self.clow, wmul(self.cstep, s.high_count)), 1);
        self.clow = wadd(self.clow, wmul(self.cstep, s.low_count));

        // e3 scaling is performed for speed and to avoid underflows
        while (self.clow >= CODER_LIMIT050) || (self.chigh < CODER_LIMIT050) {
            if self.clow >= CODER_LIMIT050 {
                self.clow &= CODER_LIMIT050 - 1;
                self.chigh &= CODER_LIMIT050 - 1;
                self.ccode &= CODER_LIMIT050 - 1;
            } // if the first wasn't the case, it's chigh < CODER_LIMIT050
            self.clow <<= 1;
            self.chigh <<= 1;
            self.chigh = wadd(self.chigh, 1);
            self.ccode <<= 1;
            self.ccode |= self.read_bit() as u32;
            self.nrbits = 0;
        }

        // e3 scaling, to make sure that there's enough space between low and high
        while (self.clow >= CODER_LIMIT025) && (self.chigh < CODER_LIMIT075) {
            self.nrbits += 1;
            self.clow &= CODER_LIMIT025 - 1;
            self.chigh ^= CODER_LIMIT025 + CODER_LIMIT050;
            self.ccode = wsub(self.ccode, CODER_LIMIT025);
            self.clow <<= 1;
            self.chigh <<= 1;
            self.chigh = wadd(self.chigh, 1);
            self.ccode <<= 1;
            self.ccode |= self.read_bit() as u32;
        }
    }

    fn write_bit(&mut self, bit: u8) {
        // add bit at last position
        self.bbyte = (self.bbyte << 1) | bit;
        // increment bit position
        self.cbit += 1;

        // write bit if done
        if self.cbit == 8 {
            let byte = [self.bbyte];
            self.stream.write(&byte, 1, 1);
            self.cbit = 0;
        }
    }

    fn read_bit(&mut self) -> u8 {
        // read in new byte if needed
        if self.cbit == 0 {
            let mut byte = [0u8];
            if self.stream.read(&mut byte, 1, 1) == 0 {
                byte[0] = 0; // if no more data is left in the stream
            }
            self.bbyte = byte[0];
            self.cbit = 8;
        }
        // decrement current bit position
        self.cbit -= 1;
        // return bit at cbit position
        (self.bbyte >> self.cbit) & 0x1
    }
}

// ---------------------------------------------------------------------------
// model_s - universal statistical model
// ---------------------------------------------------------------------------

/// A context table stored in the arena (index-addressed).
struct TableS {
    /// counts for each symbol contained in the table (None until totalized)
    counts: Option<Vec<u16>>,
    /// links to higher order contexts (None = no links allocated)
    links: Option<Vec<Option<usize>>>,
    /// link to lower order context
    lesser: Option<usize>,
    /// speedup info
    max_count: u16,
    max_symbol: u16,
}

pub struct ModelS {
    totals: Vec<u32>,
    scoreboard: Vec<i8>,
    sb0_count: i32,
    contexts: Vec<usize>,
    arena: Vec<TableS>,
    pub error: bool,

    max_symbol: i32,
    max_context: i32,
    current_order: i32,
    max_order: i32,
    max_count: i32,
}

impl ModelS {
    /// `max_s` maximum symbol, `max_c` maximum context, `max_o` maximum order,
    /// `c_lim` maximum count.
    pub fn new(max_s: i32, max_c: i32, max_o: i32, c_lim: i32) -> Self {
        let mut m = ModelS {
            totals: vec![0u32; (max_s + 2) as usize],
            scoreboard: vec![0i8; max_s as usize],
            sb0_count: max_s,
            contexts: vec![0usize; (max_o + 2) as usize],
            arena: Vec::new(),
            error: false,
            max_symbol: max_s,
            max_context: max_c,
            current_order: max_o,
            max_order: max_o,
            max_count: c_lim,
        };

        // set up null table
        let null_table = m.alloc(TableS {
            counts: Some(vec![1u16; max_s as usize]),
            links: None,
            lesser: None,
            max_count: 1,
            max_symbol: max_s as u16,
        });

        // set up start table
        let start_table = m.alloc(TableS {
            counts: None,
            links: Some(vec![None; max_c as usize]),
            lesser: Some(null_table),
            max_count: 0,
            max_symbol: 0,
        });

        // build links for start table & null table
        m.arena[start_table].lesser = Some(null_table);
        m.arena[null_table].links = Some(vec![Some(start_table); max_c as usize]);

        // integrate tables into contexts
        m.contexts[0] = null_table; // contexts[-1]
        if max_o >= 0 {
            m.contexts[1] = start_table; // contexts[0]
        }

        // build initial 'normal' tables
        for i in 1..=max_o {
            let lesser = m.contexts[((i - 1) + 1) as usize];
            let links = if i < max_o {
                Some(vec![None; max_c as usize])
            } else {
                None
            };
            let idx = m.alloc(TableS {
                counts: None,
                links,
                lesser: Some(lesser),
                max_count: 0,
                max_symbol: 0,
            });
            // contexts[i-1]->links[0] = contexts[i]
            m.arena[lesser].links.as_mut().unwrap()[0] = Some(idx);
            m.contexts[(i + 1) as usize] = idx;
        }

        m
    }

    fn alloc(&mut self, t: TableS) -> usize {
        self.arena.push(t);
        self.arena.len() - 1
    }

    fn ctx(&self, order: i32) -> usize {
        self.contexts[(order + 1) as usize]
    }
}

impl ModelS {
    /// updates statistics for a specific symbol / resets to highest order.
    /// use -1 to just reset without updating statistics.
    pub fn update_model(&mut self, symbol: i32) {
        // only contexts that were actually used to encode the symbol get their
        // counts updated
        if symbol >= 0 {
            let lo = if self.current_order < 0 { 0 } else { self.current_order };
            for local_order in lo..=self.max_order {
                let c = self.ctx(local_order);
                // defensive: C++ would crash on NULL counts here; allocate zeroed
                if self.arena[c].counts.is_none() {
                    self.arena[c].counts = Some(vec![0u16; self.max_symbol as usize]);
                }
                let need_rescale = {
                    let counts = self.arena[c].counts.as_mut().unwrap();
                    counts[symbol as usize] += 1;
                    let sc = counts[symbol as usize];
                    // store side information for totalize_table
                    if sc > self.arena[c].max_count {
                        self.arena[c].max_count = sc;
                    }
                    if symbol >= self.arena[c].max_symbol as i32 {
                        self.arena[c].max_symbol = (symbol + 1) as u16;
                    }
                    sc >= self.max_count as u16
                };
                // if counts for that symbol have gone above the maximum count
                // the table has to be resized (scale factor 2)
                if need_rescale {
                    self.rescale_table(c, 1);
                }
            }
        }

        // reset scoreboard and current order
        self.current_order = self.max_order;
        self.scoreboard.iter_mut().for_each(|s| *s = 0);
        self.sb0_count = self.max_symbol;
    }

    /// shift in one context (max no of contexts is max_c)
    pub fn shift_context(&mut self, c: i32) {
        // shifting is not possible if max_order is below 1 or context index is negative
        if self.max_order < 1 || c < 0 {
            return;
        }
        // shift each orders' context
        for i in (1..=self.max_order).rev() {
            // this is the new current order context
            let parent = self.ctx(i - 1);
            let child = self.arena[parent].links.as_ref().unwrap()[c as usize];

            let idx = if let Some(child) = child {
                child
            } else {
                // reserve memory for next table_s
                let lesser = self.arena[self.ctx(i - 2)].links.as_ref().unwrap()[c as usize];
                let links = if i == self.max_order {
                    None
                } else {
                    Some(vec![None; self.max_context as usize])
                };
                let new = self.alloc(TableS {
                    counts: None,
                    links,
                    lesser,
                    max_count: 0,
                    max_symbol: 0,
                });
                // add lesser link for higher context (see above)
                if i < self.max_order {
                    let hi = self.ctx(i + 1);
                    self.arena[hi].lesser = Some(new);
                }
                // put context to its right place
                self.arena[parent].links.as_mut().unwrap()[c as usize] = Some(new);
                new
            };
            // switch context
            self.contexts[(i + 1) as usize] = idx;
        }
    }

    /// flushes the whole model by dividing through a specific scale factor
    pub fn flush_model(&mut self, scale_factor: i32) {
        self.recursive_flush(self.ctx(0), scale_factor);
    }

    /// exclude specific symbols using this function
    pub fn exclude_symbols(&mut self, rule: u8, c: i32) {
        // exclusions are back to normal after update_model is used
        // modify scoreboard according to rule and value
        match rule {
            b'a' => {
                // above rule: every symbol above c is excluded
                for s in (c + 1)..self.max_symbol {
                    if self.scoreboard[s as usize] == 0 {
                        self.scoreboard[s as usize] = 1;
                        self.sb0_count -= 1;
                    }
                }
            }
            b'b' => {
                // below rule: every symbol below c is excluded
                let mut s = c - 1;
                while s >= 0 {
                    if self.scoreboard[s as usize] == 0 {
                        self.scoreboard[s as usize] = 1;
                        self.sb0_count -= 1;
                    }
                    s -= 1;
                }
            }
            b'e' if self.scoreboard[c as usize] == 0 => {
                // equal rule: only c is excluded
                self.scoreboard[c as usize] = 1;
                self.sb0_count -= 1;
            }
            _ => {
                // unknown rule / already excluded: do nothing
            }
        }
    }
}

impl ModelS {
    /// converts an int to a symbol, needed only when encoding.
    /// returns 0 if symbol found, 1 if escape was emitted
    pub fn convert_int_to_symbol(&mut self, c: i32, s: &mut Symbol) -> i32 {
        // totalize table for the current context
        let context = self.ctx(self.current_order);
        self.totalize_table(context);

        // finding the scale is easy
        s.scale = self.totals[0];

        // check if that symbol exists in the current table. send escape otherwise
        if c >= 0 {
            let counts = self.arena[context].counts.as_ref().unwrap();
            if counts[c as usize] > 0 {
                // return high and low count for the current symbol
                s.low_count = self.totals[(c + 2) as usize];
                s.high_count = self.totals[(c + 1) as usize];
                return 0;
            }
        }

        // return high and low count for the escape symbol
        s.low_count = self.totals[1];
        s.high_count = self.totals[0];
        self.current_order -= 1;
        1
    }

    /// returns the current context scale, needed only when decoding
    pub fn get_symbol_scale(&mut self, s: &mut Symbol) {
        let context = self.ctx(self.current_order);
        self.totalize_table(context);
        s.scale = self.totals[0];
    }

    /// converts a count to an int, called after get_symbol_scale.
    /// returns ESCAPE_SYMBOL when the escape symbol was hit
    pub fn convert_symbol_to_int(&mut self, count: u32, s: &mut Symbol) -> u32 {
        // go through the totals table, search the symbol that matches the count
        let mut c: i32 = 1;
        while count < self.totals[c as usize] {
            c += 1;
        }
        // set up the current symbol
        s.low_count = self.totals[c as usize];
        s.high_count = self.totals[(c - 1) as usize];
        // send escape if escape symbol encountered
        if c == 1 {
            self.current_order -= 1;
            return ESCAPE_SYMBOL;
        }
        // return symbol value
        (c - 2) as u32
    }

    /// totals are calculated by accumulating counts in the current table_s
    fn totalize_table(&mut self, context: usize) {
        // update exclusion is used, so this has to be done each time
        // escape probability calculation also takes place here
        if let Some(counts) = self.arena[context].counts.as_ref().cloned() {
            // locally store current fill/symbol count
            let local_symb = self.sb0_count;

            // set the last symbol of the totals table zero
            let mut i = self.arena[context].max_symbol as i32 - 1;
            let totals = &mut self.totals;
            totals[(i + 2) as usize] = 0;
            // (re)set current total
            let mut curr_total: u32 = 0;

            // go reverse through the whole counts table and accumulate counts
            // leave space at the beginning of the table for the escape symbol
            while i >= 0 {
                // only count probability if the current symbol is not 'scoreboard - excluded'
                if self.scoreboard[i as usize] == 0 {
                    let curr_count = counts[i as usize] as u32;
                    if curr_count > 0 {
                        // add counts for the current symbol
                        curr_total += curr_count;
                        // exclude symbol from scoreboard
                        self.scoreboard[i as usize] = 1;
                        self.sb0_count -= 1;
                    }
                }
                totals[(i + 1) as usize] = curr_total;
                i -= 1;
            }
            // here the escape calculation needs to take place
            let esc_prob: u32 = if local_symb == self.sb0_count {
                1
            } else if self.sb0_count == 0 {
                0
            } else {
                let p = (self.sb0_count * (local_symb - self.sb0_count))
                    / (local_symb * self.arena[context].max_count as i32)
                    + 1;
                p as u32
            };
            // include escape probability in totals table
            totals[0] = totals[1] + esc_prob;
        } else {
            // setup counts for current table
            self.arena[context].counts = Some(vec![0u16; self.max_symbol as usize]);
            // set totals table -> only escape probability included
            self.totals[0] = 1;
            self.totals[1] = 0;
        }
    }

    /// resizes one table by bitshifting each count using a specific value
    fn rescale_table(&mut self, context: usize, scale_factor: i32) {
        let t = &mut self.arena[context];
        let lst_symbol = t.max_symbol as usize;
        if lst_symbol == 0 {
            return;
        }
        let counts = t.counts.as_mut().unwrap();
        // now scale the table by bitshifting each count
        for c in counts.iter_mut().take(lst_symbol) {
            if *c > 0 {
                *c >>= scale_factor;
            }
        }
        // also rescale table's max count
        t.max_count >>= scale_factor;

        // seek for new last symbol
        let mut last = -1i32;
        for i in (0..lst_symbol).rev() {
            if counts[i] > 0 {
                last = i as i32;
                break;
            }
        }
        t.max_symbol = (last + 1) as u16;
    }

    /// a recursive function to go through each context and rescale the counts
    fn recursive_flush(&mut self, context: usize, scale_factor: i32) {
        // go through each link != NULL
        if let Some(links) = self.arena[context].links.clone() {
            let children: Vec<usize> = links.iter().flatten().copied().collect();
            for child in children {
                self.recursive_flush(child, scale_factor);
            }
        }
        // rescale specific table
        self.rescale_table(context, scale_factor);
    }
}

// ---------------------------------------------------------------------------
// model_b - binary statistical model
// ---------------------------------------------------------------------------

/// A context table for the binary model (stored in the arena).
struct Table {
    /// counts for each symbol (0 and 1)
    counts: Option<[u16; 2]>,
    /// links to higher order contexts
    links: Option<Vec<Option<usize>>>,
    /// link to lower order context
    lesser: Option<usize>,
    /// accumulated counts
    scale: u32,
}

pub struct ModelB {
    contexts: Vec<usize>,
    arena: Vec<Table>,
    pub error: bool,

    max_context: i32,
    max_order: i32,
    max_count: i32,
}

impl ModelB {
    pub fn new(max_c: i32, max_o: i32, c_lim: i32) -> Self {
        let mut m = ModelB {
            contexts: vec![0usize; (max_o + 2) as usize],
            arena: Vec::new(),
            error: false,
            max_context: max_c,
            max_order: max_o,
            max_count: c_lim,
        };

        // set up null table
        let null_table = m.alloc(Table {
            counts: Some([1, 1]),
            links: None,
            lesser: None,
            scale: 2,
        });

        // set up start table
        let start_table = m.alloc(Table {
            counts: None,
            links: Some(vec![None; max_c as usize]),
            lesser: Some(null_table),
            scale: 0,
        });

        // build links for start table & null table
        m.arena[start_table].lesser = Some(null_table);
        m.arena[null_table].links = Some(vec![Some(start_table); max_c as usize]);

        // integrate tables into contexts
        m.contexts[0] = null_table; // contexts[-1]
        if max_o >= 0 {
            m.contexts[1] = start_table; // contexts[0]
        }

        // build initial 'normal' tables
        for i in 1..=max_o {
            let lesser = m.contexts[((i - 1) + 1) as usize];
            let links = if i < max_o {
                Some(vec![None; max_c as usize])
            } else {
                None
            };
            let idx = m.alloc(Table {
                counts: None,
                links,
                lesser: Some(lesser),
                scale: 0,
            });
            m.arena[lesser].links.as_mut().unwrap()[0] = Some(idx);
            m.contexts[(i + 1) as usize] = idx;
        }

        m
    }

    fn alloc(&mut self, t: Table) -> usize {
        self.arena.push(t);
        self.arena.len() - 1
    }

    fn ctx(&self, order: i32) -> usize {
        self.contexts[(order + 1) as usize]
    }

    /// updates statistics for a specific symbol / resets to highest order.
    /// use -1 to just reset without updating statistics.
    pub fn update_model(&mut self, symbol: i32) {
        let context = self.ctx(self.max_order);
        // only contexts that were actually used to encode the symbol get their
        // counts updated
        if symbol >= 0 && self.max_order >= 0 {
            let cur = {
                let t = &mut self.arena[context];
                t.counts.as_mut().unwrap()[symbol as usize] += 1;
                t.scale += 1;
                t.counts.as_ref().unwrap()[symbol as usize]
            };
            // if counts for that symbol have gone above the maximum count
            // the table has to be resized (scale factor 2)
            if cur >= self.max_count as u16 {
                self.rescale_table(context, 1);
            }
        }
    }

    /// shift in one context (max no of contexts is max_c)
    pub fn shift_context(&mut self, c: i32) {
        // shifting is not possible if max_order is below 1 or context index is negative
        if self.max_order < 1 || c < 0 {
            return;
        }
        // shift each orders' context
        for i in (1..=self.max_order).rev() {
            let parent = self.ctx(i - 1);
            let child = self.arena[parent].links.as_ref().unwrap()[c as usize];

            let idx = if let Some(child) = child {
                child
            } else {
                // reserve memory for next table
                let lesser = self.arena[self.ctx(i - 2)].links.as_ref().unwrap()[c as usize];
                let links = if i == self.max_order {
                    None
                } else {
                    Some(vec![None; self.max_context as usize])
                };
                let new = self.alloc(Table {
                    counts: None,
                    links,
                    lesser,
                    scale: 0,
                });
                if i < self.max_order {
                    let hi = self.ctx(i + 1);
                    self.arena[hi].lesser = Some(new);
                }
                // put context to its right place
                self.arena[parent].links.as_mut().unwrap()[c as usize] = Some(new);
                new
            };
            // switch context
            self.contexts[(i + 1) as usize] = idx;
        }
    }

    /// flushes the whole model by dividing through a specific scale factor
    pub fn flush_model(&mut self, scale_factor: i32) {
        self.recursive_flush(self.ctx(0), scale_factor);
    }

    /// converts an int to a symbol, needed only when encoding
    pub fn convert_int_to_symbol(&mut self, c: i32, s: &mut Symbol) -> i32 {
        let context = self.ctx(self.max_order);
        // check if counts are available
        self.check_counts(context);
        // finding the scale is easy
        s.scale = self.arena[context].scale;
        // return high and low count for current symbol
        if c == 0 {
            // if 0 is to be encoded
            s.low_count = 0;
            s.high_count = self.arena[context].counts.as_ref().unwrap()[0] as u32;
        } else {
            // if 1 is to be encoded
            s.low_count = self.arena[context].counts.as_ref().unwrap()[0] as u32;
            s.high_count = self.arena[context].scale;
        }
        1
    }

    /// returns the current context scale, needed only when decoding
    pub fn get_symbol_scale(&mut self, s: &mut Symbol) {
        let context = self.ctx(self.max_order);
        // check if counts are available
        self.check_counts(context);
        // getting the scale is easy
        s.scale = self.arena[context].scale;
    }

    /// converts a count to an int, called after get_symbol_scale
    pub fn convert_symbol_to_int(&mut self, count: u32, s: &mut Symbol) -> i32 {
        let context = self.ctx(self.max_order);
        let counts0 = self.arena[context].counts.as_ref().unwrap()[0] as u32;
        // set up the current symbol
        if count < counts0 {
            s.low_count = 0;
            s.high_count = counts0;
            0
        } else {
            s.low_count = counts0;
            s.high_count = s.scale;
            1
        }
    }

    /// checks if counts exist, and, if they exist and are below max
    fn check_counts(&mut self, context: usize) {
        if self.arena[context].counts.is_none() {
            // setup counts for current table
            self.arena[context].counts = Some([1, 1]);
            // set scale
            self.arena[context].scale = 2;
        }
    }

    /// resizes one table by bitshifting each count using a specific value
    fn rescale_table(&mut self, context: usize, scale_factor: i32) {
        let counts = match self.arena[context].counts.as_mut() {
            Some(c) => c,
            None => return, // return now if counts not set
        };
        // now scale the table by bitshifting each count,
        // be careful not to set any count zero
        counts[0] >>= scale_factor;
        counts[1] >>= scale_factor;
        if counts[0] == 0 {
            counts[0] = 1;
        }
        if counts[1] == 0 {
            counts[1] = 1;
        }
        self.arena[context].scale = (counts[0] as u32) + (counts[1] as u32);
    }

    /// a recursive function to go through each context and rescale the counts
    fn recursive_flush(&mut self, context: usize, scale_factor: i32) {
        // go through each link != NULL
        if let Some(links) = self.arena[context].links.clone() {
            let children: Vec<usize> = links.iter().flatten().copied().collect();
            for child in children {
                self.recursive_flush(child, scale_factor);
            }
        }
        // rescale specific table
        self.rescale_table(context, scale_factor);
    }
}

// ---------------------------------------------------------------------------
// generic model_s encoder/decoder
// ---------------------------------------------------------------------------

/// generic model_s encoder function
pub fn encode_ari_s(encoder: &mut Aricoder, model: &mut ModelS, c: i32) {
    loop {
        let mut s = Symbol::default();
        let esc = model.convert_int_to_symbol(c, &mut s);
        encoder.encode(&s);
        if esc == 0 {
            break;
        }
    }
    model.update_model(c);
}

/// generic model_s decoder function
pub fn decode_ari_s(decoder: &mut Aricoder, model: &mut ModelS) -> i32 {
    let mut s = Symbol::default();
    loop {
        model.get_symbol_scale(&mut s);
        let count = decoder.decode_count(&s);
        let c = model.convert_symbol_to_int(count, &mut s);
        decoder.decode(&s);
        if c != ESCAPE_SYMBOL {
            model.update_model(c as i32);
            return c as i32;
        }
    }
}

/// generic model_b encoder function
pub fn encode_ari_b(encoder: &mut Aricoder, model: &mut ModelB, c: i32) {
    let mut s = Symbol::default();
    model.convert_int_to_symbol(c, &mut s);
    encoder.encode(&s);
    model.update_model(c);
}

/// generic model_b decoder function
pub fn decode_ari_b(decoder: &mut Aricoder, model: &mut ModelB) -> i32 {
    let mut s = Symbol::default();
    model.get_symbol_scale(&mut s);
    let count = decoder.decode_count(&s);
    let c = model.convert_symbol_to_int(count, &mut s);
    decoder.decode(&s);
    model.update_model(c);
    c
}
