//! Odin / LOKE wire protocol.
//!
//! Byte layouts transcribed from the odin4 implementation and its protocol
//! reference (`docs/THOR_PROTOCOL.md`), cross-checked against Heimdall's
//! `ControlPacket` / `ResponsePacket` classes. Where the two disagree, odin4 is
//! followed because odin4 is the client that actually works with modern
//! (non-Heimdall-compatible) bootloaders such as the SM-A135F.
//!
//! Source references:
//!   * https://github.com/Llucs/odin4  src/protocol/thor_protocol.h
//!   * https://github.com/Llucs/odin4  src/usb/odin_protocol.cpp
//!   * https://github.com/Benjamin-Dobell/Heimdall  heimdall/source/BridgeManager.cpp

#![allow(dead_code)]

// ---------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------

/// Host -> device, plain **bulk OUT**, exactly 4 bytes. There is no control
/// transfer in the LOKE handshake despite what some write-ups claim.
pub const HANDSHAKE_OUT: &[u8; 4] = b"ODIN";
/// Device -> host, plain **bulk IN**, first 4 bytes must be this.
pub const HANDSHAKE_IN: &[u8; 4] = b"LOKE";
/// Some newer bootloaders accept the "THOR" token instead. Tried as a fallback.
pub const HANDSHAKE_OUT_ALT: &[u8; 4] = b"THOR";

/// Timeout odin4 uses for control commands / handshake.
pub const TIMEOUT_CONTROL_MS: u64 = 1000;

// ---------------------------------------------------------------------------
// Request box (1024 bytes, all multi-byte fields little-endian)
// ---------------------------------------------------------------------------
//
//   0x000  4  id        command type (0x64 | 0x65 | 0x66 | 0x67)
//   0x004  4  data      command parameter / sub-command
//   0x008 36  intData[9] integer argument array
//   0x02C 128 charData   character argument buffer
//   0x0AC 32  md5        reserved
//   0x0CC 820 dummy      zero padding

pub const REQUEST_SIZE: usize = 1024;
pub const INT_DATA_COUNT: usize = 9;
pub const DATA_INT_OFFSET: usize = 8;
pub const DATA_CHAR_OFFSET: usize = 0x2C;
pub const MD5_OFFSET: usize = 0xAC;

// Response box: 8 bytes minimum, always read at least this much.
pub const RESPONSE_SIZE: usize = 8;

// ---- command types (request box `id`) ------------------------------------
pub const RQT_INIT: u32 = 0x64; // session
pub const RQT_PIT: u32 = 0x65; // partition information table
pub const RQT_XMIT: u32 = 0x66; // file transfer
pub const RQT_CLOSE: u32 = 0x67; // session close / control

// ---- RQT_INIT parameters (request box `data`) ----------------------------
pub const RQT_INIT_TARGET: u32 = 0; // -> device type / protocol version
pub const RQT_INIT_RESETTIME: u32 = 1; // -> reset flash count
pub const RQT_INIT_TOTALSIZE: u32 = 2;
pub const RQT_INIT_OEMSTATE: u32 = 3;
pub const RQT_INIT_NOOEMSTATE: u32 = 4;
pub const RQT_INIT_PACKETSIZE: u32 = 5;
pub const RQT_INIT_XMIT_SIZE: u32 = 6;

// ---- RQT_PIT parameters --------------------------------------------------
pub const RQT_PIT_SET: u32 = 0;
pub const RQT_PIT_GET: u32 = 1; // request PIT dump, ack = total size
pub const RQT_PIT_START: u32 = 2; // intData[0] = block index -> raw 500-byte block
pub const RQT_PIT_COMPLETE: u32 = 3; // end PIT dump

/// PIT is dumped in fixed 500-byte blocks.
pub const PIT_BLOCK_SIZE: usize = 500;

// ---- RQT_CLOSE parameters ------------------------------------------------
pub const RQT_CLOSE_END: u32 = 0;
pub const RQT_CLOSE_REBOOT: u32 = 1;
pub const RQT_CLOSE_DISCONNECT: u32 = 2;
pub const RQT_CLOSE_REBOOT_RECOVERY: u32 = 3;

/// `id` field sentinel meaning the bootloader rejected the command.
pub const BOOTLOADER_FAIL: u32 = 0xFFFF_FFFF;
/// `ack` sentinel also treated as failure.
pub const ACK_SENTINEL_FAIL: i32 = i32::MIN;

/// Catch-all protocol version request: device replies with its own version.
pub const MAX_PROTOCOL_VERSION: u32 = 0x7FFF_FFFF;

// ---------------------------------------------------------------------------
// Request construction
// ---------------------------------------------------------------------------

/// Build a 1024-byte request box, LE encoded, zero padded.
pub fn make_request(cmd: u32, subcmd: u32, ints: &[u32], chars: &[u8]) -> [u8; REQUEST_SIZE] {
    let mut b = [0u8; REQUEST_SIZE];
    b[0..4].copy_from_slice(&cmd.to_le_bytes());
    b[4..8].copy_from_slice(&subcmd.to_le_bytes());
    for (i, v) in ints.iter().take(INT_DATA_COUNT).enumerate() {
        let off = DATA_INT_OFFSET + i * 4;
        b[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    let n = chars.len().min(128);
    b[DATA_CHAR_OFFSET..DATA_CHAR_OFFSET + n].copy_from_slice(&chars[..n]);
    b
}

/// RQT_INIT / RQT_INIT_TARGET with the catch-all protocol version.
pub fn begin_session_request() -> [u8; REQUEST_SIZE] {
    make_request(RQT_INIT, RQT_INIT_TARGET, &[MAX_PROTOCOL_VERSION], &[])
}

pub fn device_type_request() -> [u8; REQUEST_SIZE] {
    make_request(RQT_INIT, RQT_INIT_TARGET, &[], &[])
}

pub fn set_packet_size_request(size: u32) -> [u8; REQUEST_SIZE] {
    make_request(RQT_INIT, RQT_INIT_PACKETSIZE, &[size], &[])
}

pub fn set_total_bytes_request(total: u64) -> [u8; REQUEST_SIZE] {
    let lo = total as u32;
    let hi = (total >> 32) as u32;
    make_request(RQT_INIT, RQT_INIT_TOTALSIZE, &[lo, hi], &[])
}

pub fn pit_dump_request() -> [u8; REQUEST_SIZE] {
    make_request(RQT_PIT, RQT_PIT_GET, &[], &[])
}

pub fn pit_block_request(index: u32) -> [u8; REQUEST_SIZE] {
    make_request(RQT_PIT, RQT_PIT_START, &[index], &[])
}

pub fn pit_complete_request() -> [u8; REQUEST_SIZE] {
    make_request(RQT_PIT, RQT_PIT_COMPLETE, &[], &[])
}

pub fn end_session_request() -> [u8; REQUEST_SIZE] {
    make_request(RQT_CLOSE, RQT_CLOSE_END, &[], &[])
}

pub fn command_name(cmd: u32) -> &'static str {
    match cmd {
        RQT_INIT => "RQT_INIT(0x64)",
        RQT_PIT => "RQT_PIT(0x65)",
        RQT_XMIT => "RQT_XMIT(0x66)",
        RQT_CLOSE => "RQT_CLOSE(0x67)",
        _ => "RQT_UNKNOWN",
    }
}

pub fn u32_le(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

pub fn i32_le(buf: &[u8], off: usize) -> i32 {
    i32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

// ---------------------------------------------------------------------------
// Response decoding
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Response {
    pub id: u32,
    pub ack: i32,
    pub raw: Vec<u8>,
    /// extra bytes beyond id+ack, verbatim (PIT blocks read here, or in a
    /// separate transfer for the 500-byte path)
    pub extra: Vec<u8>,
}

impl Response {
    pub fn parse(raw: Vec<u8>) -> Option<Response> {
        if raw.len() < RESPONSE_SIZE {
            return None;
        }
        Some(Response {
            id: u32_le(&raw, 0),
            ack: i32_le(&raw, 4),
            extra: raw[RESPONSE_SIZE..].to_vec(),
            raw,
        })
    }

    pub fn is_bootloader_fail(&self) -> bool {
        self.id == BOOTLOADER_FAIL
    }

    /// odin4 `odin_fail_check` semantics.
    pub fn is_ok_for(&self, expected_id: u32) -> bool {
        !self.is_bootloader_fail()
            && self.id == expected_id
            && self.ack != ACK_SENTINEL_FAIL
            && self.ack >= 0
    }

    pub fn describe(&self, expected_id: u32) -> String {
        let mut s = format!(
            "id=0x{:08x} ({}), ack=0x{:08x} ({})",
            self.id,
            command_name(self.id),
            self.ack as u32,
            self.ack
        );
        if self.is_bootloader_fail() {
            s.push_str("  <-- BOOTLOADER_FAIL (command rejected)");
        } else if self.id != expected_id {
            s.push_str(&format!(
                "  <-- UNEXPECTED: expected id 0x{expected_id:02x} ({})",
                command_name(expected_id)
            ));
        } else if self.ack < 0 {
            s.push_str("  <-- negative status");
        } else {
            s.push_str("  <-- OK");
        }
        s
    }

    /// Bootloader / protocol version from a RQT_INIT_TARGET ack.
    pub fn protocol_version(&self) -> u16 {
        (((self.ack as u32) >> 16) & 0x7FFF) as u16
    }

    pub fn supports_compressed(&self) -> bool {
        (((self.ack as u32) >> 16) & 0x8000) != 0
    }
}

// ---------------------------------------------------------------------------
// PIT file parsing (libpit / odin4 `PitTable`)
// ---------------------------------------------------------------------------

pub const PIT_MAGIC: u32 = 0x12349876;
pub const PIT_HEADER_SIZE: usize = 28;
pub const PIT_ENTRY_SIZE: usize = 132;

#[derive(Debug, Clone)]
pub struct PitEntry {
    pub binary_type: u32,
    pub device_type: u32,
    pub identifier: u32,
    pub attributes: u32,
    pub update_attributes: u32,
    pub block_size_or_offset: u32,
    pub block_count: u32,
    pub file_offset: u32,
    pub file_size: u32,
    pub partition_name: String,
    pub file_name: String,
    pub fota_name: String,
}

impl PitEntry {
    pub fn dev_type_name(&self) -> &'static str {
        match self.device_type {
            0 => "OneNAND",
            1 => "FAT",
            2 => "MMC",
            _ => "?",
        }
    }
    pub fn binary_name(&self) -> &'static str {
        match self.binary_type {
            0 => "AP",
            1 => "CP",
            _ => "?",
        }
    }
    /// Approximate byte size of the partition.
    pub fn size_bytes(&self) -> u64 {
        self.block_size_or_offset as u64 * self.block_count as u64
    }
}

#[derive(Debug, Clone)]
pub struct PitData {
    pub entry_count: u32,
    pub com_tar2: [u8; 8],
    pub cpu_bl_id: [u8; 8],
    pub entries: Vec<PitEntry>,
}

fn field_string(raw: &[u8]) -> String {
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    String::from_utf8_lossy(&raw[..end]).trim().to_string()
}

impl PitData {
    pub fn unpack(buf: &[u8]) -> Result<PitData, String> {
        if buf.len() < PIT_HEADER_SIZE {
            return Err(format!("PIT too short: {} bytes", buf.len()));
        }
        let magic = u32_le(buf, 0);
        if magic != PIT_MAGIC {
            return Err(format!("bad PIT magic 0x{magic:08x}, expected 0x{PIT_MAGIC:08x}"));
        }
        let entry_count = u32_le(buf, 4);
        if entry_count == 0 || entry_count > 512 {
            return Err(format!("implausible PIT entry count {entry_count}"));
        }

        let mut com_tar2 = [0u8; 8];
        com_tar2.copy_from_slice(&buf[8..16]);
        let mut cpu_bl_id = [0u8; 8];
        cpu_bl_id.copy_from_slice(&buf[16..24]);

        let mut entries = Vec::with_capacity(entry_count as usize);
        for i in 0..entry_count as usize {
            let off = PIT_HEADER_SIZE + i * PIT_ENTRY_SIZE;
            if off + PIT_ENTRY_SIZE > buf.len() {
                return Err(format!(
                    "PIT truncated at entry {i}: need {} bytes, have {}",
                    off + PIT_ENTRY_SIZE,
                    buf.len()
                ));
            }
            let e = &buf[off..off + PIT_ENTRY_SIZE];
            entries.push(PitEntry {
                binary_type: u32_le(e, 0),
                device_type: u32_le(e, 4),
                identifier: u32_le(e, 8),
                attributes: u32_le(e, 12),
                update_attributes: u32_le(e, 16),
                block_size_or_offset: u32_le(e, 20),
                block_count: u32_le(e, 24),
                file_offset: u32_le(e, 28),
                file_size: u32_le(e, 32),
                partition_name: field_string(&e[36..68]),
                file_name: field_string(&e[68..100]),
                fota_name: field_string(&e[100..132]),
            });
        }

        Ok(PitData {
            entry_count,
            com_tar2,
            cpu_bl_id,
            entries,
        })
    }
}
