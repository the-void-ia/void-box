#!/usr/bin/env python3
"""Write the seed corpora for the `virtio_net` and `virtio_blk` fuzz targets.

Both harnesses in `src/fuzz.rs` read their input through the same hand-rolled
cursor, and the committed seeds are bound to that read order byte for byte. The
layout, shared by both targets:

    u16 write_offset      where the guest memory image lands
    u16 image_len         bytes of image that follow
    image                 descriptor table, rings, buffers, request headers
    operations...         until the input runs out, at most 48

A `virtio_blk` operation is a register write:

    u8 register index     modulo the bring-up table below
    u32 value
    u8 read offset        an MMIO read of offset * 4 follows every operation

A `virtio_net` operation is a frame handed to the device or a register write:

    u8 slot               0 flush a frame, 1 buffer a frame, 2 + index a register
    then for a frame:     u16 len, len bytes (the harness folds len modulo 2049)
    then for a register:  u32 value
    u8 read offset

Run it from anywhere; it clears and rewrites `fuzz/corpus/virtio_net/` and
`fuzz/corpus/virtio_blk/` under the repository root. Rerun it after any change to
a harness's read order, and check the result with `cargo test --test fuzz_corpus`.
"""
import os
import shutil
import struct
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

# `QUEUE_BRING_UP_REGISTERS` in src/fuzz.rs, by index.
(STATUS, QUEUE_SEL, QUEUE_NUM, DESC_LOW, DESC_HIGH, DRIVER_LOW, DRIVER_HIGH,
 DEVICE_LOW, DEVICE_HIGH, QUEUE_READY, QUEUE_NOTIFY, DRIVER_FEATURES,
 DRIVER_FEATURES_SEL) = range(13)

# `virtio_net` operation slots: the frame slots, then the registers.
NET_FLUSH_FRAME, NET_QUEUE_FRAME, NET_FIRST_REGISTER = 0, 1, 2

# Descriptor flags.
NEXT, WRITE = 1, 2

# Guest memory layout every seed uses. The image starts at BASE and runs to the
# end of the highest piece a seed places.
BASE = 0x1000
DESC, AVAIL, USED = 0x1000, 0x1100, 0x1200
HDR, STATUS_BYTE, DATA = 0x1300, 0x1310, 0x1400
UNMAPPED = 0x7000_0000

RX_QUEUE, TX_QUEUE = 0, 1
BLK_T_IN, BLK_T_OUT, BLK_T_FLUSH = 0, 1, 4

# Every operation is followed by an MMIO read of this offset times four, which
# lands in the device's config space.
CONFIG_READ = 0x40

QUEUE_SIZE = 8


class Image:
    """Sparse guest memory a seed fills, rendered as one contiguous image."""

    def __init__(self):
        self.pieces = {}

    def put(self, addr, data):
        self.pieces[addr] = bytes(data)

    def desc(self, index, addr, length, flags, next_index):
        self.put(DESC + 16 * index, struct.pack('<QIHH', addr, length, flags, next_index))

    def avail(self, heads, idx=None):
        idx = len(heads) if idx is None else idx
        ring = b''.join(struct.pack('<H', head) for head in heads)
        self.put(AVAIL, struct.pack('<HH', 0, idx) + ring)

    def render(self):
        end = max(addr + len(data) for addr, data in self.pieces.items())
        buf = bytearray(end - BASE)
        for addr, data in self.pieces.items():
            buf[addr - BASE:addr - BASE + len(data)] = data
        return bytes(buf)


def register_op(slot, value, read=CONFIG_READ):
    return struct.pack('<BIB', slot, value, read)


def frame_op(slot, payload, read=CONFIG_READ):
    return struct.pack('<BH', slot, len(payload)) + payload + struct.pack('<B', read)


def seed(image, ops):
    img = image.render()
    return struct.pack('<HH', BASE, len(img)) + img + b''.join(ops)


def program(reg, queue, num=QUEUE_SIZE, desc=DESC, avail=AVAIL, used=USED, ready=True):
    """The register writes a driver issues to bring one queue up."""
    ops = [
        reg(QUEUE_SEL, queue),
        reg(QUEUE_NUM, num),
        reg(DESC_LOW, desc & 0xFFFFFFFF),
        reg(DESC_HIGH, desc >> 32),
        reg(DRIVER_LOW, avail & 0xFFFFFFFF),
        reg(DRIVER_HIGH, avail >> 32),
        reg(DEVICE_LOW, used & 0xFFFFFFFF),
        reg(DEVICE_HIGH, used >> 32),
    ]
    if ready:
        ops.append(reg(QUEUE_READY, 1))
    return ops


def pattern(n, start=0):
    return bytes((start + i) % 251 for i in range(n))


def tx_frame(n):
    """A virtio-net header (12 zero bytes) then an Ethernet-shaped payload."""
    eth = bytes([0x52, 0x54, 0, 0x12, 0x34, 0x56, 0x52, 0x54, 0, 0x65, 0x43, 0x21, 0x08, 0x00])
    body = eth + pattern(max(0, n - 12 - len(eth)))
    return bytes(12) + body[:n - 12]


def write_corpus(target, seeds):
    directory = ROOT / 'fuzz' / 'corpus' / target
    shutil.rmtree(directory, ignore_errors=True)
    os.makedirs(directory)
    for name, data in seeds.items():
        (directory / name).write_bytes(data)
        print(f'{directory / name}: {len(data)} bytes')


# ---------------------------------------------------------------- virtio_net

def net_reg(index, value, read=CONFIG_READ):
    return register_op(NET_FIRST_REGISTER + index, value, read)


def net_program(queue, **kwargs):
    return program(net_reg, queue, **kwargs)


def tx_kick():
    return [net_reg(QUEUE_NOTIFY, TX_QUEUE)]


def net_seeds():
    out = {}

    img = Image()
    payload = tx_frame(64)
    img.put(DATA, payload)
    img.desc(0, DATA, len(payload), 0, 0)
    img.avail([0])
    out['tx_single_descriptor_frame'] = seed(img, net_program(TX_QUEUE) + tx_kick())

    img = Image()
    payload = tx_frame(64)
    img.put(DATA, payload)
    img.desc(0, DATA, 12, NEXT, 1)
    img.desc(1, DATA + 12, 14, NEXT, 2)
    img.desc(2, DATA + 26, 38, 0, 0)
    img.avail([0])
    out['tx_frame_chained_across_descriptors'] = seed(img, net_program(TX_QUEUE) + tx_kick())

    img = Image()
    img.put(DATA, pattern(256))
    img.desc(0, DATA, 256, NEXT, 0)
    img.avail([0])
    out['tx_self_referential_chain'] = seed(img, net_program(TX_QUEUE) + tx_kick())

    img = Image()
    img.put(DATA, pattern(128))
    img.desc(0, DATA, 128, NEXT, 9999)
    img.avail([0])
    out['tx_next_index_past_queue_size'] = seed(img, net_program(TX_QUEUE) + tx_kick())

    img = Image()
    img.put(DATA, pattern(64))
    img.desc(0, DATA, 0xFFFFFFFF, 0, 0)
    img.avail([0])
    out['tx_huge_descriptor_len'] = seed(img, net_program(TX_QUEUE) + tx_kick())

    img = Image()
    img.put(DATA, bytes(1))
    img.desc(0, UNMAPPED, 64, 0, 0)
    img.avail([0])
    out['tx_payload_at_unmapped_address'] = seed(img, net_program(TX_QUEUE) + tx_kick())

    # One mapped descriptor longer than the frame ceiling: the read is clamped,
    # so the assembled bytes fall short of the described ones.
    img = Image()
    img.put(DATA, bytes(1))
    img.desc(0, DATA, 70_000, 0, 0)
    img.avail([0])
    out['tx_descriptor_over_the_frame_ceiling'] = seed(img, net_program(TX_QUEUE) + tx_kick())

    # Two descriptors that together cross the ceiling while each is readable.
    img = Image()
    img.put(DATA, bytes(1))
    img.desc(0, DATA, 40_000, NEXT, 1)
    img.desc(1, DATA, 40_000, 0, 0)
    img.avail([0])
    out['tx_chain_over_the_frame_ceiling'] = seed(img, net_program(TX_QUEUE) + tx_kick())

    img = Image()
    img.put(DATA, pattern(64))
    img.desc(0, 0, 64, 0, 0)
    img.avail([0])
    out['tx_zero_address_descriptor'] = seed(img, net_program(TX_QUEUE) + tx_kick())

    img = Image()
    img.put(DATA, pattern(64))
    img.desc(0, DATA, 0, 0, 0)
    img.avail([0])
    out['tx_zero_length_descriptor'] = seed(img, net_program(TX_QUEUE) + tx_kick())

    img = Image()
    payload = tx_frame(64)
    img.put(DATA, payload)
    img.desc(0, DATA, len(payload), 0, 0)
    img.avail([0] * QUEUE_SIZE, idx=0xFFFF)
    out['tx_avail_idx_far_ahead'] = seed(img, net_program(TX_QUEUE) + tx_kick() + tx_kick())

    img = Image()
    img.desc(0, DATA, 2048, WRITE, 0)
    img.desc(1, DATA + 2048, 2048, WRITE, 0)
    img.avail([0, 1])
    img.put(DATA, bytes(1))
    out['rx_frame_into_posted_buffers'] = seed(
        img,
        net_program(RX_QUEUE)
        + [frame_op(NET_FLUSH_FRAME, tx_frame(100)), frame_op(NET_FLUSH_FRAME, tx_frame(60))])

    img = Image()
    img.desc(0, DATA, 32, WRITE | NEXT, 1)
    img.desc(1, DATA + 32, 32, WRITE | NEXT, 2)
    img.desc(2, DATA + 64, 32, WRITE, 0)
    img.avail([0])
    img.put(DATA, bytes(1))
    out['rx_frame_spans_descriptor_chain'] = seed(
        img, net_program(RX_QUEUE) + [frame_op(NET_FLUSH_FRAME, tx_frame(80))])

    img = Image()
    img.desc(0, DATA, 32, WRITE, 0)
    img.avail([0])
    img.put(DATA, bytes(1))
    out['rx_frame_longer_than_the_posted_chain'] = seed(
        img, net_program(RX_QUEUE) + [frame_op(NET_FLUSH_FRAME, tx_frame(100))])

    img = Image()
    img.desc(0, DATA, 0, WRITE | NEXT, 0)
    img.avail([0])
    img.put(DATA, bytes(1))
    out['rx_zero_length_descriptor_cycle'] = seed(
        img, net_program(RX_QUEUE) + [frame_op(NET_FLUSH_FRAME, tx_frame(64))])

    img = Image()
    img.desc(0, 0, 64, WRITE | NEXT, 0)
    img.avail([0])
    img.put(DATA, bytes(1))
    out['rx_zero_address_descriptor_cycle'] = seed(
        img, net_program(RX_QUEUE) + [frame_op(NET_FLUSH_FRAME, tx_frame(64))])

    img = Image()
    img.desc(0, DATA, 32, WRITE | NEXT, 9999)
    img.avail([0])
    img.put(DATA, bytes(1))
    out['rx_next_index_past_queue_size'] = seed(
        img, net_program(RX_QUEUE) + [frame_op(NET_FLUSH_FRAME, tx_frame(64))])

    img = Image()
    img.desc(0, UNMAPPED, 2048, WRITE, 0)
    img.avail([0])
    img.put(DATA, bytes(1))
    out['rx_descriptor_at_unmapped_address'] = seed(
        img, net_program(RX_QUEUE) + [frame_op(NET_FLUSH_FRAME, tx_frame(64))])

    img = Image()
    img.desc(0, DATA, 2048, WRITE, 0)
    img.avail([0])
    img.put(DATA, bytes(1))
    out['rx_frame_buffered_until_kick'] = seed(
        img,
        net_program(RX_QUEUE)
        + [frame_op(NET_QUEUE_FRAME, tx_frame(90)[12:]), net_reg(QUEUE_NOTIFY, RX_QUEUE)])

    # Three frames buffered, one buffer posted: the kick delivers one and
    # re-buffers the rest.
    img = Image()
    img.desc(0, DATA, 2048, WRITE, 0)
    img.avail([0])
    img.put(DATA, bytes(1))
    out['rx_batch_exceeds_posted_buffers'] = seed(
        img,
        net_program(RX_QUEUE)
        + [frame_op(NET_QUEUE_FRAME, tx_frame(50)[12:]) for _ in range(3)]
        + [net_reg(QUEUE_NOTIFY, RX_QUEUE)])

    img = Image()
    img.desc(0, DATA, 2048, WRITE, 0)
    img.avail([], idx=0)
    img.put(DATA, bytes(1))
    out['rx_no_buffers_posted'] = seed(
        img,
        net_program(RX_QUEUE)
        + [frame_op(NET_FLUSH_FRAME, tx_frame(64)), net_reg(QUEUE_NOTIFY, RX_QUEUE)])

    img = Image()
    img.put(DATA, pattern(64))
    img.desc(0, DATA, 64, 0, 0)
    img.avail([0])
    out['wild_ring_addresses'] = seed(
        img,
        net_program(TX_QUEUE, desc=0xFFFFFFFFFFFFFFF0, avail=0xFFFFFFFF00000000,
                    used=0x7FFFFFFFFFFFFFF0)
        + tx_kick())

    img = Image()
    payload = tx_frame(64)
    img.put(DATA, payload)
    img.desc(0, DATA, len(payload), 0, 0)
    img.avail([0])
    out['reset_before_kick'] = seed(img, net_program(TX_QUEUE) + [net_reg(STATUS, 0)] + tx_kick())

    return out


# ---------------------------------------------------------------- virtio_blk

def blk_program(**kwargs):
    return program(register_op, 0, **kwargs)


def blk_chain(img, req_type=BLK_T_IN, sector=0, data=None, hdr_addr=HDR, hdr_len=16,
              hdr_flags=NEXT, hdr_next=1, status_flags=WRITE, status_len=1):
    """One request: a header, data descriptors, and a status byte, published."""
    if data is None:
        data = [(DATA, 512, NEXT | WRITE)]
    img.put(HDR, struct.pack('<IIQ', req_type, 0, sector))
    img.desc(0, hdr_addr, hdr_len, hdr_flags, hdr_next)
    for i, (addr, length, flags) in enumerate(data):
        img.desc(1 + i, addr, length, flags, 2 + i)
    img.desc(1 + len(data), STATUS_BYTE, status_len, status_flags, 0)
    img.put(STATUS_BYTE, b'\xff')
    img.put(DATA, bytes(1))
    img.avail([0])


def blk_seeds():
    out = {}

    def kicked(img):
        return seed(img, blk_program() + [register_op(QUEUE_NOTIFY, 0)])

    img = Image()
    blk_chain(img)
    out['read_one_sector'] = kicked(img)

    img = Image()
    blk_chain(img, sector=15, data=[(DATA, 1024, NEXT | WRITE)])
    out['read_spanning_the_end_of_the_disk'] = kicked(img)

    img = Image()
    blk_chain(img, sector=0xFFFFFFFFFFFFFFFF)
    out['read_past_the_end_of_the_disk'] = kicked(img)

    img = Image()
    blk_chain(img, sector=1, data=[(DATA, 256, NEXT | WRITE), (DATA + 256, 256, NEXT | WRITE),
                                   (DATA + 512, 256, NEXT | WRITE)])
    out['read_across_several_data_descriptors'] = kicked(img)

    # Longer than the device's one I/O buffer, so the read moves in steps.
    img = Image()
    blk_chain(img, data=[(DATA, 200 * 1024, NEXT | WRITE)])
    out['read_streamed_in_several_chunks'] = kicked(img)

    img = Image()
    blk_chain(img, req_type=BLK_T_OUT)
    out['write_request_on_a_read_only_disk'] = kicked(img)

    img = Image()
    blk_chain(img, req_type=BLK_T_FLUSH)
    out['unknown_request_type'] = kicked(img)

    img = Image()
    blk_chain(img, hdr_len=8)
    out['header_shorter_than_a_request'] = kicked(img)

    img = Image()
    blk_chain(img, hdr_flags=NEXT | WRITE)
    out['header_marked_device_writable'] = kicked(img)

    img = Image()
    blk_chain(img, hdr_addr=UNMAPPED)
    out['header_at_unmapped_address'] = kicked(img)

    img = Image()
    blk_chain(img, status_flags=0)
    out['status_descriptor_not_writable'] = kicked(img)

    img = Image()
    blk_chain(img, status_len=0)
    out['status_descriptor_of_zero_length'] = kicked(img)

    img = Image()
    blk_chain(img, data=[(DATA, 512, NEXT)])
    out['data_descriptor_not_writable'] = kicked(img)

    # Under the request ceiling, past the guest's memory: only the mapping check
    # can reject it.
    img = Image()
    blk_chain(img, data=[(DATA, 1024 * 1024, NEXT | WRITE)])
    out['data_descriptor_names_unmapped_memory'] = kicked(img)

    img = Image()
    blk_chain(img, data=[(DATA, 0xFFFFFFFF, NEXT | WRITE)])
    out['huge_data_descriptor_len'] = kicked(img)

    img = Image()
    blk_chain(img, data=[(DATA, 0x01000001, NEXT | WRITE), (DATA, 0x01000001, NEXT | WRITE)])
    out['request_over_the_byte_ceiling'] = kicked(img)

    img = Image()
    blk_chain(img, hdr_next=0)
    out['self_referential_chain'] = kicked(img)

    img = Image()
    blk_chain(img, hdr_next=200)
    out['next_index_past_queue_size'] = kicked(img)

    img = Image()
    blk_chain(img, hdr_flags=0)
    out['chain_of_one_descriptor'] = kicked(img)

    img = Image()
    blk_chain(img)
    img.avail([0] * QUEUE_SIZE, idx=0x1000)
    out['avail_idx_far_ahead'] = kicked(img)

    img = Image()
    blk_chain(img)
    out['wild_ring_addresses'] = seed(
        img,
        blk_program(desc=0xFFFFFFFFFFFFFFF0, avail=0xFFFFFFFFFFFFFFFE, used=0xFFFFFFFFFFFFFFF8)
        + [register_op(QUEUE_NOTIFY, 0)])

    img = Image()
    blk_chain(img)
    out['zero_queue_size'] = seed(img, blk_program(num=0) + [register_op(QUEUE_NOTIFY, 0)])

    img = Image()
    blk_chain(img)
    out['kick_before_ready'] = seed(
        img, blk_program(ready=False) + [register_op(QUEUE_NOTIFY, 0), register_op(QUEUE_NOTIFY, 0)])

    return out


if __name__ == '__main__':
    write_corpus('virtio_net', net_seeds())
    write_corpus('virtio_blk', blk_seeds())
