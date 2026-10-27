#!/usr/bin/env python3
"""
Generator: converts the C/C++ data tables of packMP3 into Rust source.

Reads:
  ../../source/huffmp3tbl.h
  ../../source/pmp3tbl.h
  ../../source/pmp3bitlen.h

Writes:
  ../src/hufftables.rs
  ../src/tables.rs
"""
import re
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
SRC = os.path.normpath(os.path.join(HERE, "..", "..", "source"))
OUT = os.path.normpath(os.path.join(HERE, "..", "src"))


def strip_comments(text):
    text = re.sub(r"/\*.*?\*/", "", text, flags=re.S)
    text = re.sub(r"//[^\n]*", "", text)
    return text


def parse_c_array(body):
    """Parse the text between an outer '{' and its matching '}' into a
    nested Python structure. Scalars become int when possible, else str."""
    items = []
    i = 0
    n = len(body)
    while i < n:
        c = body[i]
        if c in " \t\r\n,;":
            i += 1
            continue
        if c == "{":
            depth = 1
            j = i + 1
            while j < n and depth > 0:
                if body[j] == "{":
                    depth += 1
                elif body[j] == "}":
                    depth -= 1
                j += 1
            items.append(parse_c_array(body[i + 1 : j - 1]))
            i = j
        else:
            j = i
            while j < n and body[j] not in ",{}":
                j += 1
            tok = body[i:j].strip()
            items.append(tok)
            i = j
    return items


def to_int(s):
    s = s.strip()
    try:
        return int(s, 0)
    except ValueError:
        return None


def to_int_expr(s):
    """Evaluate simple arithmetic like '15 + 8191' or '0 +    0'."""
    s = s.strip()
    if s.upper() == "NULL":
        return None
    parts = re.split(r"[+\-]", s)
    try:
        total = 0
        for p in parts:
            total += int(p.strip(), 0)
        return total
    except ValueError:
        return None


def parse_arrays(text):
    """Return dict name -> (type_str, decl_size, nested_list) for every
    'static <type> <name>[<size>] = { ... };' block."""
    text = strip_comments(text)
    out = {}
    idx = 0
    while True:
        m = re.search(
            r"static\s+(?P<type>[A-Za-z0-9_\s\*]+?)\s+(?P<name>\w+)\s*(?:\[[^\]]*\])*\s*=\s*\{",
            text[idx:],
        )
        if not m:
            break
        name = m.group("name")
        typ = re.sub(r"\s+", " ", m.group("type")).strip()
        size = ""
        start = idx + m.end() - 1  # points at '{'
        # find matching close brace
        depth = 1
        k = start + 1
        while k < len(text) and depth > 0:
            if text[k] == "{":
                depth += 1
            elif text[k] == "}":
                depth -= 1
            k += 1
        body = text[start + 1 : k - 1]
        out[name] = (typ, size, parse_c_array(body))
        idx = k
    return out


# ---------------------------------------------------------------------------
# helpers to format Rust
# ---------------------------------------------------------------------------


def fmt_intlist(vals, per_line=16):
    lines = []
    for i in range(0, len(vals), per_line):
        chunk = vals[i : i + per_line]
        lines.append("    " + ", ".join(str(v) for v in chunk) + ",")
    return "\n".join(lines)


def fmt_flat_array(name, rust_type, vals, per_line=16):
    return f"pub static {name}: &[{rust_type}] = &[\n{fmt_intlist(vals, per_line)}\n];\n"


def fmt_nested(name, rust_type, data, dims, per_line=16):
    """data is nested lists matching dims; emit nested &[] consts."""
    if len(dims) == 1:
        return fmt_flat_array(name, rust_type, data, per_line)
    inner = []
    for sub in data:
        inner.append(fmt_nested("_x", rust_type, sub, dims[1:], per_line))
    # combine into one array literal
    parts = [f"pub static {name}: &[{_nest_type(rust_type, len(dims)-1)}] = &["]
    # we emit nested array-of-array references using inline &[...] literals
    # simplest: emit full inline literals
    return _emit_nested_inline(name, rust_type, data, dims, per_line)


def _nest_type(base, depth):
    return base  # placeholder


def _emit_nested_inline(name, rust_type, data, dims, per_line):
    def emit(data, dims):
        if len(dims) == 1:
            return "[" + ", ".join(str(v) for v in data) + "]"
        inner = [emit(sub, dims[1:]) for sub in data]
        return "[" + ", ".join(inner) + "]"

    lit = emit(data, dims)
    # build type: e.g. [[i32;4];4]
    t = rust_type
    for d in reversed(dims):
        t = f"[{t}; {d}]"
    return f"pub static {name}: &{t} = &{lit};\n"


# ---------------------------------------------------------------------------
# huffmp3tbl.h
# ---------------------------------------------------------------------------


def gen_hufftables(arrays):
    out = []
    out.append("// AUTO-GENERATED from source/huffmp3tbl.h by tools/gen_tables.py")
    out.append("#![allow(dead_code)]")
    out.append("")
    out.append("#[derive(Clone, Copy, Debug)]")
    out.append("pub struct HuffmanCode {")
    out.append("    pub len: u8,")
    out.append("    pub code: u32,")
    out.append("}")
    out.append("")
    out.append("#[derive(Clone, Copy, Debug)]")
    out.append("pub struct HuffmanConv {")
    out.append("    pub len: u8,")
    out.append("    pub v0: i32,")
    out.append("    pub v1: i32,")
    out.append("    pub ext: i32,")
    out.append("}")
    out.append("")
    out.append("#[derive(Clone, Copy, Debug)]")
    out.append("pub struct HuffmanConvSet {")
    out.append("    pub h: &'static [HuffmanConv],")
    out.append("    pub hwidth: u32,")
    out.append("}")
    out.append("")
    out.append("#[derive(Clone, Copy, Debug)]")
    out.append("pub struct HuffmanEncTable {")
    out.append("    pub h: Option<&'static [&'static [HuffmanCode]]>,")
    out.append("    pub linbits: u32,")
    out.append("    pub max: u32,")
    out.append("}")
    out.append("")
    out.append("#[derive(Clone, Copy, Debug)]")
    out.append("pub struct HuffmanDecTable {")
    out.append("    pub h: Option<&'static [HuffmanConvSet]>,")
    out.append("    pub linbits: u32,")
    out.append("    pub max: u32,")
    out.append("}")
    out.append("")

    def hc_lit(e):
        return f"HuffmanCode {{ len: {e[0]}, code: 0x{e[1]:X} }}"

    def hcv_lit(e):
        return f"HuffmanConv {{ len: {e[0]}, v0: {e[1]}, v1: {e[2]}, ext: {e[3]} }}"

    # --- small value tables A / B (encode) ---
    for nm in ["htabA_enc", "htabB_enc"]:
        typ, size, data = arrays[nm]
        vals = [hc_lit([to_int(x) for x in e]) for e in data]
        body = "\n".join("    " + v + "," for v in vals)
        out.append(f"pub static {nm.upper()}: &[HuffmanCode] = &[")
        out.append(body)
        out.append("];")
        out.append("")

    # --- small value tables A / B (decode) ---
    for nm in ["htabA_dec", "htabB_dec"]:
        # htabA_dec = { htabA_dec_raw, 6 }
        typ, size, data = arrays[nm]
        refname = data[0]
        hwidth = to_int(data[1])
        rawtyp, rawsize, rawdata = arrays[refname]
        vals = [hcv_lit([to_int(x) for x in e]) for e in rawdata]
        body = "\n".join("    " + v + "," for v in vals)
        out.append(f"pub const {refname.upper()}: &[HuffmanConv] = &[")
        out.append(body)
        out.append("];")
        out.append("")
        out.append(f"pub static {nm.upper()}: HuffmanConvSet = HuffmanConvSet {{")
        out.append(f"    h: {refname.upper()},")
        out.append(f"    hwidth: {hwidth},")
        out.append("};")
        out.append("")

    # --- big value tables (encode) ---
    enc_names = sorted(
        [
            n
            for n in arrays
            if re.fullmatch(r"htab\d+_enc", n) and "_raw" not in n
        ],
        key=lambda n: int(re.match(r"htab(\d+)_enc", n).group(1)),
    )
    for nm in enc_names:
        num = re.match(r"htab(\d+)_enc", nm).group(1)
        raw_nm = f"htab{num}_enc_raw"
        typ, size, data = arrays[nm]
        rawtyp, rawsize, rawdata = arrays[raw_nm]
        raw = [hc_lit([to_int(x) for x in e]) for e in rawdata]
        # pointer offsets
        offsets = []
        for e in data:
            m = re.match(rf"{raw_nm}\s*\+\s*(\d+)", e)
            offsets.append(int(m.group(1)))
        stride = offsets[1] if len(offsets) > 1 else len(raw)
        # emit 2D rows as inline literals (const range indexing is not
        # available in rustc 1.99 statics, so each row is its own literal)
        rows = []
        for off in offsets:
            row = ",\n".join(
                "        " + v for v in raw[off : off + stride]
            )
            rows.append(f"    &[\n{row}\n    ]")
        out.append(f"pub static HTAB{num}_ENC: &[&[HuffmanCode]] = &[")
        out.append(",\n".join(rows))
        out.append("];")
        out.append("")

    # --- big value tables (decode) ---
    dec_names = sorted(
        [
            n
            for n in arrays
            if re.fullmatch(r"htab\d+_dec", n) and "_start" not in n and "_ext" not in n
        ],
        key=lambda n: int(re.match(r"htab(\d+)_dec", n).group(1)),
    )
    for nm in dec_names:
        num = re.match(r"htab(\d+)_dec", nm).group(1)
        typ, size, data = arrays[nm]
        # data = [ [refname, hwidth], ... ]
        sets = []
        for e in data:
            refname = e[0]
            hwidth = to_int(e[1])
            rawtyp, rawsize, rawdata = arrays[refname]
            vals = [hcv_lit([to_int(x) for x in v]) for v in rawdata]
            body = "\n".join("    " + v + "," for v in vals)
            sets.append((refname, hwidth, body))
        # emit all raw tables then the conv set array
        for refname, hwidth, body in sets:
            out.append(f"pub const {refname.upper()}: &[HuffmanConv] = &[")
            out.append(body)
            out.append("];")
            out.append("")
        out.append(f"pub static HTAB{num}_DEC: &[HuffmanConvSet] = &[")
        for refname, hwidth, body in sets:
            out.append(f"    HuffmanConvSet {{ h: {refname.upper()}, hwidth: {hwidth} }},")
        out.append("];")
        out.append("")

    # --- bv_enc_table / bv_dec_table ---
    def gen_bv(name, hfield):
        typ, size, data = arrays[name]
        rows = []
        for e in data:
            refname = e[0]
            linbits = to_int(e[1])
            mx = to_int_expr(e[2])
            if refname.upper() == "NULL":
                hval = "None"
            else:
                hval = f"Some(HTAB{refname[4:].upper()})"
            rows.append(f"    {hfield}{{ h: {hval}, linbits: {linbits}, max: {mx} }},")
        out.append(f"pub static {name.upper()}: &[{hfield}] = &[")
        out.extend(rows)
        out.append("];")
        out.append("")

    gen_bv("bv_enc_table", "HuffmanEncTable")
    gen_bv("bv_dec_table", "HuffmanDecTable")

    return "\n".join(out) + "\n"


# ---------------------------------------------------------------------------
# pmp3tbl.h + pmp3bitlen.h
# ---------------------------------------------------------------------------


def gen_tables(pmp3_arrays, bitlen_arrays):
    out = []
    out.append("// AUTO-GENERATED from source/pmp3tbl.h and source/pmp3bitlen.h")
    out.append("#![allow(dead_code)]")
    out.append("")
    out.append("// ---- constants (from pmp3tbl.h) ----")
    out.append("pub const PADDING_MAX_RUN: i32 = 255;")
    out.append("pub const SWITCHING_MAX_RUN: i32 = 31;")
    out.append("pub const GG_CONTEXT_SIZE: i32 = 16;")
    out.append("pub const AUX_DATA_STEP: i32 = 255;")
    out.append("pub const STUFFING_STEP: i32 = 15;")
    out.append("pub const MAX_BAD_FIRST: i32 = 255;")
    out.append("pub const GARBAGE_TOLERANCE: i32 = 64 * 1024;")
    out.append("pub const FIRST_FRAME_AREA: i32 = 1024 * 1024;")
    out.append("")
    out.append("// MPEG version / layer / channel / block constants")
    out.append("pub const MP3_V2_5: i32 = 0;")
    out.append("pub const MP3_V2_0: i32 = 2;")
    out.append("pub const MP3_V1_0: i32 = 3;")
    out.append("pub const LAYER_III: i32 = 1;")
    out.append("pub const LAYER_II: i32 = 2;")
    out.append("pub const LAYER_I: i32 = 3;")
    out.append("pub const MP3_STEREO: i32 = 0;")
    out.append("pub const MP3_JOINT_STEREO: i32 = 1;")
    out.append("pub const MP3_DUAL_CHANNEL: i32 = 2;")
    out.append("pub const MP3_MONO: i32 = 3;")
    out.append("pub const LONG_BLOCK: i32 = 0;")
    out.append("pub const START_BLOCK: i32 = 1;")
    out.append("pub const SHORT_BLOCK: i32 = 2;")
    out.append("pub const STOP_BLOCK: i32 = 3;")
    out.append("")
    out.append("// file type constants")
    out.append("pub const F_MP3: i32 = 1;")
    out.append("pub const F_PMP: i32 = 2;")
    out.append("pub const F_UNK: i32 = 3;")
    out.append("")
    out.append("pub const TYPE_FILE: i32 = 0;")
    out.append("pub const TYPE_MEMORY: i32 = 1;")
    out.append("pub const TYPE_STREAM: i32 = 2;")
    out.append("pub const MODE_READ: i32 = 0;")
    out.append("pub const MODE_WRITE: i32 = 1;")
    out.append("")

    def flat(name, rust_type, conv):
        typ, size, data = arrays_lookup[name]
        vals = [conv(v) for v in data]
        out.append(f"pub static {name.upper()}: &[{rust_type}] = &[")
        out.append(fmt_intlist(vals, 16))
        out.append("];")
        out.append("")

    # ---- pmp3bitlen.h ----
    out.append("// ---- bit length tables (from pmp3bitlen.h) ----")
    bitlen_names = ["pbitmax_16", "pbitlen_p8192", "pbitlen_n8224_8223"]
    for nm in bitlen_names:
        typ, size, data = bitlen_arrays[nm]
        vals = [to_int(v) for v in data]
        rt = "u8" if nm != "pbitmax_16" else "i32"
        out.append(f"pub static {nm.upper()}: &[{rt}] = &[")
        out.append(fmt_intlist(vals, 32))
        out.append("];")
        out.append("")
    out.append("pub fn bitlen8192p(v: i32) -> u8 {")
    out.append("    PBITLEN_P8192[v as usize]")
    out.append("}")
    out.append("")
    out.append("pub fn bitmax16(bl: i32) -> i32 {")
    out.append("    PBITMAX_16[bl as usize]")
    out.append("}")
    out.append("")
    out.append("pub fn bitlen8224n(v: i32) -> i32 {")
    out.append("    PBITLEN_N8224_8223[(8224 + v) as usize] as i32")
    out.append("}")
    out.append("")

    arrays_lookup = dict(pmp3_arrays)
    arrays_lookup.update(bitlen_arrays)

    # ---- pmp3tbl.h tables ----
    out.append("// ---- samplerate table ----")
    name = "samplerate_table"
    typ, size, data = arrays_lookup[name]
    out.append(_emit_nested_inline(name.upper(), "i32", data, [4, 4], 4))
    out.append("")
    out.append("pub fn mp3_samplerate_table(sample: i32) -> i32 {")
    out.append("    SAMPLERATE_TABLE[MP3_V1_0 as usize][sample as usize]")
    out.append("}")
    out.append("")

    out.append("// ---- bitrate table ----")
    name = "bitrate_table"
    typ, size, data = arrays_lookup[name]
    out.append(_emit_nested_inline(name.upper(), "i32", data, [4, 4, 16], 8))
    out.append("")
    out.append("pub fn mp3_bitrate_table(bit: i32) -> i32 {")
    out.append("    BITRATE_TABLE[MP3_V1_0 as usize][LAYER_III as usize][bit as usize]")
    out.append("}")
    out.append("")

    out.append("// ---- frame size table ----")
    name = "frame_size_table"
    typ, size, data = arrays_lookup[name]
    out.append(_emit_nested_inline(name.upper(), "i32", data, [4, 4, 4, 16], 8))
    out.append("")
    out.append("pub fn mp3_frame_size_table(sample: i32) -> &'static [i32; 16] {")
    out.append("    &FRAME_SIZE_TABLE[MP3_V1_0 as usize][LAYER_III as usize][sample as usize]")
    out.append("}")
    out.append("")

    out.append("// ---- slength table ----")
    name = "slength_table"
    typ, size, data = arrays_lookup[name]
    out.append(_emit_nested_inline(name.upper(), "i32", data, [16, 2], 8))
    out.append("")

    out.append("// ---- scalefactor bounds ----")
    for nm, rt in [
        ("scf_bounds", "i32"),
        ("scf_width", "i32"),
        ("scf_bounds_short", "i32"),
        ("scf_width_short", "i32"),
        ("scf_lh_bounds", "i32"),
        ("scf_lh_width", "i32"),
        ("scf_lh_bounds_short", "i32"),
        ("scf_lh_width_short", "i32"),
        ("preemphasis_table", "i32"),
    ]:
        typ, size, data = arrays_lookup[nm]
        vals = [to_int(v) for v in data]
        out.append(f"pub static {nm.upper()}: &[{rt}] = &[")
        out.append(fmt_intlist(vals, 16))
        out.append("];")
        out.append("")

    out.append("// ---- bandwidth tables ----")
    for nm, dims in [
        ("bandwidth_table", [4, 4, 22]),
        ("bandwidth_table_short", [4, 4, 13]),
        ("bandwidth_bounds", [4, 4, 23]),
        ("bandwidth_bounds_short", [4, 4, 14]),
    ]:
        typ, size, data = arrays_lookup[nm]
        out.append(_emit_nested_inline(nm.upper(), "i32", data, dims, 16))
        out.append("")
    out.append("pub fn mp3_bandwidth_bounds(sample: i32) -> &'static [i32] {")
    out.append("    &BANDWIDTH_BOUNDS[MP3_V1_0 as usize][sample as usize]")
    out.append("}")
    out.append("")
    out.append("pub fn mp3_bandwidth_bounds_short(sample: i32) -> &'static [i32] {")
    out.append("    &BANDWIDTH_BOUNDS_SHORT[MP3_V1_0 as usize][sample as usize]")
    out.append("}")
    out.append("")

    out.append("// ---- crc table ----")
    name = "crc_table"
    typ, size, data = arrays_lookup[name]
    vals = [to_int(v) for v in data]
    out.append(f"pub static CRC_TABLE: &[u16] = &[")
    out.append(fmt_intlist(vals, 8))
    out.append("];")
    out.append("")

    out.append("// ---- position to band conversion ----")
    name = "mp3_bandwidth_conv"
    typ, size, data = arrays_lookup[name]
    out.append(_emit_nested_inline(name.upper(), "i32", data, [3, 577], 16))
    out.append("")

    out.append("// ---- bitrate prediction ----")
    name = "mp3_bitrate_pred"
    typ, size, data = arrays_lookup[name]
    out.append(_emit_nested_inline(name.upper(), "i32", data, [3, 2049], 32))
    out.append("")

    out.append("// ---- filetype description ----")
    name = "filetype_description"
    typ, size, data = arrays_lookup[name]
    vals = [str(v).strip('"') for v in data]
    out.append("pub static FILETYPE_DESCRIPTION: &[&str] = &[")
    for v in vals:
        out.append(f'    "{v}",')
    out.append("];")
    out.append("")

    return "\n".join(out) + "\n"


def main():
    huff_text = open(os.path.join(SRC, "huffmp3tbl.h")).read()
    pmp3_text = open(os.path.join(SRC, "pmp3tbl.h")).read()
    bitlen_text = open(os.path.join(SRC, "pmp3bitlen.h")).read()

    huff_arrays = parse_arrays(huff_text)
    pmp3_arrays = parse_arrays(pmp3_text)
    bitlen_arrays = parse_arrays(bitlen_text)

    with open(os.path.join(OUT, "hufftables.rs"), "w") as f:
        f.write(gen_hufftables(huff_arrays))
    with open(os.path.join(OUT, "tables.rs"), "w") as f:
        f.write(gen_tables(pmp3_arrays, bitlen_arrays))

    print("generated hufftables.rs and tables.rs")


if __name__ == "__main__":
    main()
