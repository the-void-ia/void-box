//! Fuzz harnesses for the host-side parsers that read guest-controlled bytes.
//!
//! Several surfaces sit between a guest and the host VMM's address space, and
//! each parses bytes the guest chose: the control-channel frame decoder
//! (`void-box-protocol` framing plus the multiplex request-id prefix), the
//! userspace vsock connection state machine that routes guest packets, the
//! split-virtqueue reader that walks descriptor chains out of guest memory, and
//! the 9P server that answers file operations against a host directory —
//! together with the transport beneath it, which parses guest data of its own. A
//! panic in any of them takes down the VMM process for every sandbox it hosts,
//! and an allocation sized from a guest-supplied length is a host memory
//! exhaustion the guest triggers at will. Nothing else in the tree drives these
//! with bytes the guest is free to choose.
//!
//! Each entry point takes a raw byte slice, so one harness serves both callers:
//! `cargo fuzz` mutates the slice under libFuzzer, and `tests/fuzz_corpus.rs`
//! replays the committed corpus and past crash artifacts on stable Rust, inside
//! the ordinary `cargo test` gate. A harness asserts only invariants that hold
//! for every input — it must never assume the bytes are well-formed, because
//! rejecting malformed input cleanly is the behavior under test.
//!
//! Every harness returns the number of units of work it performed: frames
//! decoded, packets routed, chains popped, requests dispatched, registers
//! written. A harness that reaches none of its parser still returns without
//! panicking, so the replay gate would pass while covering nothing; the count is
//! what lets that gate hold each committed seed to a floor and fail when a
//! harness stops reaching the code it names. The count is reported, never
//! asserted here: under `cargo fuzz` an input that does no work is an ordinary
//! outcome, and an assertion would turn it into a reported crash.
//!
//! The module is `#[doc(hidden)]` rather than feature-gated: a harness behind an
//! off-by-default feature is a harness the corpus replay silently stops
//! covering the day someone drops `--all-features` from a command.

use std::io::Cursor;

use void_box_protocol::{parse_ping_payload, parse_pong_payload, Message};

use crate::backend::multiplex::{build_frame, decode_payload};

#[cfg(target_os = "linux")]
use std::os::unix::net::UnixStream;
#[cfg(target_os = "linux")]
use std::path::Path;

#[cfg(target_os = "linux")]
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

#[cfg(target_os = "linux")]
use crate::devices::virtio_9p::Virtio9pDevice;
#[cfg(target_os = "linux")]
use crate::devices::virtio_net::mmio;
#[cfg(target_os = "linux")]
use crate::devices::virtqueue::SplitVirtqueue;
#[cfg(target_os = "linux")]
use crate::devices::vsock_connection::{
    ConnState, VsockConnection, VsockConnectionMap, VsockHeader, VSOCK_HEADER_SIZE,
};

/// Guest memory the virtqueue harness maps. Large enough to hold a descriptor
/// table, both rings, and payload buffers; small enough that every fuzz
/// iteration maps it in microseconds.
#[cfg(target_os = "linux")]
const FUZZ_GUEST_MEM_BYTES: u64 = 64 * 1024;

/// Descriptor-chain pops per virtqueue iteration. The reader advances
/// `last_avail_idx` once per pop, so a handful of pops walks the ring wrap and
/// the used-ring writeback without letting one input spin indefinitely.
#[cfg(target_os = "linux")]
const FUZZ_VIRTQUEUE_POPS: usize = 8;

/// Cap on a single synthesized vsock body, so one input cannot ask the harness
/// to materialize gigabytes before reaching the code under test.
#[cfg(target_os = "linux")]
const FUZZ_MAX_VSOCK_BODY: usize = 8 * 1024;

/// Cap on 9P requests driven from one input, bounding the filesystem work a
/// single iteration can do against the caller's root directory.
#[cfg(target_os = "linux")]
const FUZZ_MAX_9P_REQUESTS: usize = 64;

/// MMIO writes driven per transport iteration. Enough to program a queue and
/// kick it several times; bounded so one input cannot loop indefinitely.
#[cfg(target_os = "linux")]
const FUZZ_MAX_MMIO_WRITES: usize = 48;

/// A cursor that turns a fuzzer's byte slice into harness parameters.
///
/// Exhaustion yields zeros and empty slices instead of panicking, so a harness
/// never has to guard on remaining length: a truncated input simply drives a
/// shorter, still-valid scenario. Hand-rolled rather than taken from
/// `arbitrary` because the committed corpus is tied to the exact
/// byte-consumption order — a crate upgrade that reorders or repacks its reads
/// would silently repoint every seed at a different scenario, and the corpus
/// would stop covering what its filenames claim.
///
/// Only the device harnesses parameterize themselves this way, and `devices` is
/// Linux-only, so on other platforms this compiles out with them.
#[cfg(target_os = "linux")]
struct Input<'a> {
    data: &'a [u8],
    pos: usize,
}

#[cfg(target_os = "linux")]
impl<'a> Input<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn is_empty(&self) -> bool {
        self.pos >= self.data.len()
    }

    fn take(&mut self, count: usize) -> &'a [u8] {
        let start = self.pos.min(self.data.len());
        let end = start.saturating_add(count).min(self.data.len());
        self.pos = end;
        &self.data[start..end]
    }

    fn u8(&mut self) -> u8 {
        self.take(1).first().copied().unwrap_or(0)
    }

    fn u16(&mut self) -> u16 {
        let mut buf = [0u8; 2];
        let taken = self.take(2);
        buf[..taken.len()].copy_from_slice(taken);
        u16::from_le_bytes(buf)
    }

    fn u32(&mut self) -> u32 {
        let mut buf = [0u8; 4];
        let taken = self.take(4);
        buf[..taken.len()].copy_from_slice(taken);
        u32::from_le_bytes(buf)
    }

    fn u64(&mut self) -> u64 {
        let mut buf = [0u8; 8];
        let taken = self.take(8);
        buf[..taken.len()].copy_from_slice(taken);
        u64::from_le_bytes(buf)
    }

    fn rest(&mut self) -> &'a [u8] {
        self.take(self.data.len())
    }
}

/// Decode `data` as control-channel frames the way the host reads them off a
/// guest stream.
///
/// Covers the layers in the order the reader thread applies them: the
/// length-prefixed `Message` header, the multiplex `[request_id][body]` payload
/// prefix, and the handshake payload parsers. The streaming decoder gets its own
/// pass because it and the whole-slice decoder size their allocation from the
/// same guest-supplied length field but recover from a short buffer differently.
///
/// Returns the number of frames the streaming decoder produced.
pub fn vsock_frame(data: &[u8]) -> usize {
    // Whole-slice decode: the length field is guest-chosen and indexes into a
    // buffer the guest also sized.
    let _ = Message::deserialize(data);

    // Streaming decode: `read_from_sync` allocates the declared payload length
    // before reading it, so this is where a guest-supplied size reaches an
    // allocator. Draining the cursor also exercises frame boundaries — the
    // reader thread's real workload is back-to-back frames, not one in
    // isolation.
    let mut cursor = Cursor::new(data);
    let mut frames = 0;
    while let Ok(message) = Message::read_from_sync(&mut cursor) {
        frames += 1;
        let Some((request_id, body)) = decode_payload(&message.payload) else {
            continue;
        };
        // Reframing a decoded frame must reproduce the payload byte for byte.
        // The two helpers are the only places the request-id prefix layout is
        // written down, and a disagreement between them corrupts every RPC
        // after the first.
        let reframed = build_frame(message.msg_type, request_id, body);
        let reframed_payload = &reframed[void_box_protocol::HEADER_SIZE..];
        assert_eq!(
            reframed_payload,
            &message.payload[..],
            "build_frame/decode_payload disagree on the request-id prefix"
        );
    }

    // Handshake payloads: the guest sends the Ping, so both parsers read
    // guest-chosen bytes before the session secret is ever compared.
    let _ = parse_ping_payload(data);
    let _ = parse_pong_payload(data);

    frames
}

/// Drive the userspace vsock connection map with guest TX packets.
///
/// The map is the host-side state machine behind `virtio_vsock_userspace`: it
/// reads a 44-byte header out of a guest descriptor and routes the packet by its
/// op, ports, and credit fields. One established connection is seeded over a
/// `socketpair`, because the paths worth reaching — credit accounting,
/// short-write buffering — are only reachable on a connection in `Connected`
/// state.
///
/// An input that opens with a full 44-byte header drives the state machine with
/// exactly that header, so a wire-format seed exercises the op it encodes. Once
/// those bytes are consumed the harness switches to a compact per-packet
/// encoding, which lets the mutator steer op and credit fields without spending
/// 44 bytes on every packet.
///
/// Returns the number of packets routed through the state machine.
#[cfg(target_os = "linux")]
pub fn vsock_packet(data: &[u8]) -> usize {
    const GUEST_CID: u64 = 3;
    const GUEST_PORT: u32 = 1234;
    const HOST_PORT: u32 = 50000;

    let mut map = VsockConnectionMap::new_without_listener(GUEST_CID);

    // The peer end stays bound for the whole call and is never read, so its
    // socket buffer fills and `write_to_host` takes the short-write path instead
    // of always completing. The 256 KiB per-connection cap above that is out of
    // reach here — one input is at most 64 KiB — so this covers partial writes
    // and the cursor arithmetic, not the cap itself.
    let peer = match UnixStream::pair() {
        Ok((host_side, peer)) => {
            let mut conn = VsockConnection::new(GUEST_PORT, HOST_PORT, host_side);
            conn.state = ConnState::Connected;
            map.connections.insert((GUEST_PORT, HOST_PORT), conn);
            Some(peer)
        }
        Err(_) => None,
    };

    let mut bytes = Input::new(data);
    let mut packets = 0;

    // A leading wire-format header is parsed and fed to the state machine as
    // itself, so a seed written in the on-wire layout drives the op it encodes.
    // The round-trip through `to_bytes` pins the field offsets on the way: a
    // shifted offset would silently reinterpret every field.
    if let Some(header) = VsockHeader::from_bytes(bytes.take(VSOCK_HEADER_SIZE)) {
        let reparsed = VsockHeader::from_bytes(&header.to_bytes())
            .expect("a serialized header must parse back");
        assert_eq!(header.src_port, reparsed.src_port);
        assert_eq!(header.dst_port, reparsed.dst_port);
        assert_eq!(header.len, reparsed.len);
        assert_eq!(header.op, reparsed.op);
        assert_eq!(header.buf_alloc, reparsed.buf_alloc);
        assert_eq!(header.fwd_cnt, reparsed.fwd_cnt);

        let body_len = (header.len as usize).min(FUZZ_MAX_VSOCK_BODY);
        let body = bytes.take(body_len);
        map.process_guest_tx(&header, body);
        packets += 1;
        let _ = map.drain_rx();
    }

    while !bytes.is_empty() {
        let header = VsockHeader {
            src_cid: GUEST_CID,
            dst_cid: 2,
            // Alternate between the seeded connection's ports and fuzzer-chosen
            // ones, so both the routed and the unknown-connection paths run.
            src_port: if bytes.u8() & 1 == 0 {
                GUEST_PORT
            } else {
                bytes.u32()
            },
            dst_port: if bytes.u8() & 1 == 0 {
                HOST_PORT
            } else {
                bytes.u32()
            },
            len: bytes.u32(),
            r#type: 1,
            op: u16::from(bytes.u8()),
            flags: bytes.u32(),
            buf_alloc: bytes.u32(),
            fwd_cnt: bytes.u32(),
        };
        let body_len = usize::from(bytes.u16()).min(FUZZ_MAX_VSOCK_BODY);
        let body = bytes.take(body_len);

        map.process_guest_tx(&header, body);
        packets += 1;

        // The map queues guest-bound packets with no consumer here; draining
        // keeps one input from growing `rx_queue` without bound and stands in
        // for the device's own drain.
        let _ = map.drain_rx();
        let _ = map.drain_host_streams();
    }

    drop(peer);

    packets
}

/// Walk descriptor chains out of guest memory the way a userspace virtio device
/// does.
///
/// The queue geometry — size and the three ring addresses — comes from MMIO
/// registers the guest writes, and the descriptor table the reader walks lives
/// in memory the guest owns. So both the parameters and the parsed bytes are
/// guest-controlled, and the harness fuzzes them together: geometry from the
/// front of the input, guest memory from the rest.
///
/// Returns the number of descriptor chains the reader walked.
#[cfg(target_os = "linux")]
pub fn virtqueue(data: &[u8]) -> usize {
    let mut bytes = Input::new(data);

    let num = bytes.u16();
    // Each ring address decides separately whether to keep its raw 64-bit value —
    // what a guest writing a wild value into the queue registers produces — or to
    // fold into the mapped region so the reader gets past its first `read_obj`
    // and actually walks a chain. Deciding once for all three would make the
    // mixed cases unreachable, and those are the interesting ones: a mapped
    // available ring with a wild used ring is the only way to reach `push_used`'s
    // overflow branch at all.
    let ring_addr = |input: &mut Input<'_>| {
        let unmapped = input.u8().is_multiple_of(4);
        let raw = input.u64();
        if unmapped {
            raw
        } else {
            raw % FUZZ_GUEST_MEM_BYTES
        }
    };
    let desc_table_addr = ring_addr(&mut bytes);
    let avail_ring_addr = ring_addr(&mut bytes);
    let used_ring_addr = ring_addr(&mut bytes);
    let write_offset = bytes.u16() as u64 % FUZZ_GUEST_MEM_BYTES;

    let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), FUZZ_GUEST_MEM_BYTES as usize)])
        .expect("mapping fuzz guest memory");

    // Whatever is left becomes the descriptor table, rings, and buffers the
    // reader parses.
    let contents = bytes.rest();
    let writable = contents
        .len()
        .min((FUZZ_GUEST_MEM_BYTES - write_offset) as usize);
    let _ = memory.write_slice(&contents[..writable], GuestAddress(write_offset));

    // Both eventfds are -1: the harness never signals, and `signal_guest` is the
    // only method that would write to them.
    let mut queue = SplitVirtqueue::new(
        num,
        desc_table_addr,
        avail_ring_addr,
        used_ring_addr,
        -1,
        -1,
    );

    let mut chains = 0;
    for _ in 0..FUZZ_VIRTQUEUE_POPS {
        let _ = queue.has_avail(&memory);
        let Some(chain) = queue.pop_avail(&memory) else {
            break;
        };
        chains += 1;
        // A chain must never be longer than the queue: the reader bounds its
        // walk by `num` precisely so a descriptor loop cannot spin forever.
        assert!(
            chain.descriptors.len() <= usize::from(num),
            "descriptor chain ({}) exceeds queue size ({num})",
            chain.descriptors.len()
        );
        let written = chain
            .descriptors
            .iter()
            .fold(0u32, |total, desc| total.saturating_add(desc.len));
        queue.push_used(&memory, chain.head_index, written);
    }

    chains
}

/// Answer 9P requests against `root`, a directory the caller owns.
///
/// The 9P server is the widest guest-controlled parser in the tree: every
/// request carries guest-chosen lengths, fids, offsets, and path components,
/// and the handlers turn them into host filesystem calls. The harness drives
/// requests in sequence rather than one at a time, because fid state carries
/// across them — a walk followed by an open followed by a read is a different
/// code path from any of the three alone.
///
/// `root` must be a directory the caller is willing to see modified: the
/// read-write pass creates, renames, and unlinks inside it. Give it a fresh
/// temporary directory per run.
///
/// Returns the number of requests dispatched to the parser.
#[cfg(target_os = "linux")]
pub fn nine_p(root: &Path, data: &[u8]) -> usize {
    let mut bytes = Input::new(data);
    // Both modes matter: read-only exercises the rejection path every mutating
    // handler starts with, read-write exercises the handlers themselves.
    let read_only = bytes.u8() & 1 == 0;
    let mut device = Virtio9pDevice::new(root, "fuzz", read_only);
    let mut requests = 0;

    for _ in 0..FUZZ_MAX_9P_REQUESTS {
        if bytes.is_empty() {
            break;
        }
        // Requests are length-delimited by the harness, not by the message's own
        // size field, so the mutator can hand a handler a payload that
        // contradicts its header — which is exactly what a hostile guest does.
        let request_len = usize::from(bytes.u16());
        let request = bytes.take(request_len);
        if request.is_empty() {
            continue;
        }

        let response = device.handle_9p_request(request);
        requests += 1;

        // Every reply the guest kernel reads starts with its own total length.
        // A reply whose size field disagrees with its length desynchronizes the
        // guest's 9P stream for every later request on the mount.
        assert!(
            response.len() >= 7,
            "9P reply shorter than its own header: {} bytes",
            response.len()
        );
        let declared = u32::from_le_bytes([response[0], response[1], response[2], response[3]]);
        assert_eq!(
            declared as usize,
            response.len(),
            "9P reply declares {declared} bytes but is {} long",
            response.len()
        );
    }

    requests
}

/// Drive the 9P device through its MMIO registers and guest memory, the way a
/// guest driver does.
///
/// [`nine_p`] hands requests straight to the message parser, which skips
/// everything between the guest and it: the queue registers the guest programs,
/// the descriptor chain the device walks, and the reply write-back. That layer
/// parses guest data too — descriptor lengths size host allocations, `next`
/// indices steer the walk, and the ring base addresses come from unvalidated
/// register writes — so it needs its own harness rather than sharing one with
/// the parser above it.
///
/// The input supplies both halves at once: a sequence of register writes, and
/// the guest memory those registers point into. `root` must be a directory the
/// caller is willing to see modified.
///
/// Returns the number of MMIO register writes executed. Reaching the queue
/// walker takes a whole bring-up sequence — size, three ring addresses, ready,
/// notify — so a count near zero means the input never programmed a queue and
/// the layer below went untouched, however cleanly the call returned.
#[cfg(target_os = "linux")]
pub fn nine_p_transport(root: &Path, data: &[u8]) -> usize {
    /// Registers a driver programs to bring a queue up, plus the notify that
    /// kicks it. Offsets come from the shared virtio-mmio layout.
    const REGISTERS: &[u64] = &[
        mmio::STATUS,
        mmio::QUEUE_SEL,
        mmio::QUEUE_NUM,
        mmio::QUEUE_DESC_LOW,
        mmio::QUEUE_DESC_HIGH,
        mmio::QUEUE_DRIVER_LOW,
        mmio::QUEUE_DRIVER_HIGH,
        mmio::QUEUE_DEVICE_LOW,
        mmio::QUEUE_DEVICE_HIGH,
        mmio::QUEUE_READY,
        mmio::QUEUE_NOTIFY,
        mmio::DRIVER_FEATURES,
        mmio::DRIVER_FEATURES_SEL,
    ];

    let mut input = Input::new(data);
    let read_only = input.u8() & 1 == 0;
    let mut device = Virtio9pDevice::new(root, "fuzz", read_only);

    let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), FUZZ_GUEST_MEM_BYTES as usize)])
        .expect("mapping fuzz guest memory");

    // Seed guest memory before any register write, so a kick has a descriptor
    // table, rings, and request buffers to find.
    //
    // The image length is its own field rather than "everything remaining":
    // guest memory is 64 KiB and an input is at most that, so taking the rest of
    // the slice would swallow the whole input and leave the register loop below
    // nothing to run — the harness would construct a device and stop, never
    // reaching `process_queue`. Splitting explicitly puts the boundary under the
    // mutator's control and keeps a register program reachable from every input.
    let write_offset = input.u16() as u64 % FUZZ_GUEST_MEM_BYTES;
    let image_len = usize::from(input.u16()).min((FUZZ_GUEST_MEM_BYTES - write_offset) as usize);
    let contents = input.take(image_len);
    let _ = memory.write_slice(contents, GuestAddress(write_offset));

    let mut writes = 0;
    for _ in 0..FUZZ_MAX_MMIO_WRITES {
        if input.is_empty() {
            break;
        }
        let register = REGISTERS[usize::from(input.u8()) % REGISTERS.len()];
        let value = input.u32();
        device.mmio_write(register, &value.to_le_bytes(), Some(&memory));
        writes += 1;

        // Reads share the register decode and the config-space path.
        let mut scratch = [0u8; 4];
        device.mmio_read(u64::from(input.u8()) * 4, &mut scratch);
    }

    writes
}
