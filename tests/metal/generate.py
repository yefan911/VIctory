import sys
import struct
import zlib
from pathlib import Path

root = Path(sys.argv[1]).resolve()
out = root / "input"
out.mkdir()

def u32(*words):
    return struct.pack("<" + "I" * len(words), *words)

def f32(value):
    return struct.unpack("<I", struct.pack("<f", value))[0]

def vop1(op, dst, src):
    return 0x7E000000 | dst << 17 | op << 9 | src

def vop2(op, dst, src0, src1):
    return op << 25 | dst << 17 | src1 << 9 | src0

def mov(dst, value):
    return [vop1(1, dst, 255), value]

def literal(op, dst, value, src):
    return [vop2(op, dst, 255, src), value]

def cvtu(dst, src):
    return vop1(7, dst, 256 + src)

def cvtf(dst, src):
    return vop1(6, dst, 256 + src)

def mul(dst, value, src):
    return literal(8, dst, f32(value), src)

def container(parts):
    offset = 32 + 4 * len(parts)
    body = bytearray()
    offsets = []

    for tag, data in parts:
        offsets.append(offset + len(body))
        body += tag + u32(len(data)) + data

    return (
        b"DXBC" + bytes(16)
        + struct.pack(
            "<HHII", 1, 0,
            offset + len(body), len(parts)
        )
        + u32(*offsets) + body
    )

def signature(name, kind, comp, mask):
    return (
        u32(1, 8)
        + u32(0, 40, 0, kind, comp, 0, mask, 0)
        + name.encode() + b"\0"
    )

def native(slot, words, stage, layout=b""):
    payload = bytearray(0x68)

    struct.pack_into("<Q", payload, 0, 6)

    for s in (8,16,24,32,40,48,72,96):
        struct.pack_into(
            "<Q", payload, s,
            0xDEADBEEFDEADBEEF
        )

    rec = len(payload)
    struct.pack_into("<Q", payload, slot, rec)

    code = u32(*words)
    header = bytearray(0x54)

    code_at = rec + len(header) + len(stage)

    struct.pack_into(
        "<QQ", header, 0, code_at, len(code)
    )

    struct.pack_into("<I", header, 0x14, 1 << 5)
    struct.pack_into("<I", header, 0x50, 28)

    if layout:
        struct.pack_into(
            "<Q", header, 0x28,
            code_at + len(code)
        )
        struct.pack_into(
            "<I", header, 0x38,
            len(layout) // 20
        )

    payload += header + stage + code + layout

    raw = (
        b"FSSM"
        + struct.pack("<IQ", 0x101, len(payload))
        + payload
    )

    compressed = zlib.compress(raw)

    xbch = (
        b"XBCH"
        + struct.pack(
            "<IQQ", 3, len(compressed), len(raw)
        )
        + compressed
    )

    return (
        bytes(0x18)
        + b"XDAT"
        + u32(1, 0x24, 0x0D, 0x30, len(xbch))
        + xbch
    )

END = 0xBF810000
EXPORT_COLOR = [0xF800180F, 0x03020100, END]

def pixel(name, words, position=True):
    stage = bytearray(0x14c + 8 * 404)

    for i in range(8):
        struct.pack_into(
            "<I", stage,
            0x14c + i * 404,
            0xffffffff
        )

    if position:
        struct.pack_into("<I", stage, 0x134, 0xf00)

    parts = [
        (b"DXIL", native(
            0x30, words + EXPORT_COLOR, stage
        )),
        (b"OSG1", signature(
            "SV_Target", 64, 3, 15
        )),
    ]

    if position:
        parts.append(
            (b"ISG1", signature(
                "SV_Position", 1, 3, 15
            ))
        )

    (out / f"{name}.dxbc").write_bytes(
        container(parts)
    )

pixel(
    "red",
    mov(0, f32(1))
    + mov(1, 0)
    + mov(2, 0)
    + mov(3, f32(1)),
    position=False
)

pixel(
    "gradient",
    mul(0, 1/32, 0)
    + mul(1, 1/32, 1)
    + mov(2, 0)
    + mov(3, f32(1))
)

pixel(
    "checker",
    [
        cvtu(0, 0),
        cvtu(1, 1),
        vop2(0x1d, 0, 256, 1),
        vop2(0x1b, 0, 129, 0),
        cvtf(0, 0),
        vop2(4, 1, 242, 0),
    ]
    + mov(2, 0)
    + mov(3, f32(1))
)

# Compare the pixel position against 32 and select 0.0 or 1.0.
pixel(
    "select",
    mov(3, f32(1))
    + [0x7C0200FF, f32(32)]
    + [
        vop2(1, 4, 128, 3),
        vop1(1, 0, 260),
        vop1(1, 1, 260),
        vop1(1, 2, 260),
    ]
)

for name, a, b in (
    ("perm_a", 0x807f12e3, 0x7f80a145),
    ("perm_b", 0x7f80ed1c, 0x807f5eba),
):
    words = [
        cvtu(2, 0),
        vop2(0x1a, 3, 136, 2),
        vop2(0x1c, 3, 258, 3),
        vop2(0x1a, 4, 144, 3),
        vop2(0x1c, 3, 259, 4),
    ]

    words += literal(
        0x1d, 3, 0xffaa5500, 3
    )

    words += mov(4, a) + mov(5, b)

    words += [
        0xD7440006,
        260 | (261 << 9) | (259 << 18)
    ]

    for channel in range(4):
        words += [
            vop2(
                0x16, 7,
                128 + 8 * channel, 6
            )
        ]

        words += literal(0x1b, 7, 255, 7)
        words += [cvtf(7, 7)]
        words += mul(channel, 1/255, 7)

    pixel(name, words)

vs_stage = bytearray(0x18c)

vs_words = [
    vop2(0x1b, 1, 129, 0),
    vop2(0x1b, 2, 130, 0),
    cvtf(1, 1),
    cvtf(2, 2),
]

vs_words += mul(1, 4, 1)
vs_words += mul(2, 2, 2)

vs_words += [
    vop2(3, 0, 243, 1),
    vop2(3, 1, 243, 2),
]

vs_words += mov(2, 0) + mov(3, f32(1))

vs_words += [0xF80008CF, 0x03020100, END]

(out / "vertex.dxbc").write_bytes(
    container([
        (b"DXIL", native(
            0x28, vs_words, vs_stage
        )),
        (b"ISG1", signature(
            "SV_VertexID", 6, 1, 1
        )),
        (b"OSG1", signature(
            "SV_Position", 1, 3, 15
        )),
    ])
)

cs_stage = bytearray(0x34)

struct.pack_into("<I", cs_stage, 0x24, 2)
struct.pack_into("<III", cs_stage, 0x28, 64, 1, 1)

cs_words = [
    0xBE810380,
    0xF4080100,
    0xFA000000,
    vop2(0x1a, 1, 130, 0),
]

cs_words += literal(0x1d, 2, 0x13579bdf, 0)

cs_words += [
    0xE0701000,
    0x80010201,
    END
]

layout = u32(5, 0, 1, 0) + bytes(4)

(out / "compute.dxbc").write_bytes(
    container([
        (b"DXIL", native(
            0x60, cs_words, cs_stage, layout
        ))
    ])
)

for file in sorted(out.glob("*.dxbc")):
    print(file.name, file.stat().st_size, "bytes")
