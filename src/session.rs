//! Transport-agnostic Odin session logic.
//!
//! Both backends - raw WinUSB bulk (`client.rs`) and the Windows VCOM COM-port
//! path (`serial.rs`) - go through this module, so the *bytes on the wire* are
//! provably identical and any difference in behaviour can only come from the
//! transport itself. That is exactly the variable under investigation here.

use std::time::Duration;

use crate::log as lg;
use crate::protocol::*;

/// Minimum two-way transport the protocol needs.
pub trait Wire {
    fn write_all(&mut self, data: &[u8], timeout: Duration) -> Result<(), String>;
    /// Read up to `len` bytes; implementations may return fewer.
    fn read(&mut self, len: usize, timeout: Duration) -> Result<Vec<u8>, String>;
    fn clear_halt(&mut self);
    fn label(&self) -> String;
}

// ---------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct HandshakeAttempt {
    pub token_label: String,
    pub token_bytes: Vec<u8>,
    pub write_result: Result<usize, String>,
    pub read_result: Result<usize, String>,
    pub reply: Vec<u8>,
    pub ok: bool,
    pub elapsed_ms: u128,
}

impl HandshakeAttempt {
    pub fn verdict(&self) -> String {
        if self.ok {
            return format!(
                "SUCCESS: b\"{}\" accepted, device replied b\"LOKE\" ({} bytes) after {} ms",
                self.token_label,
                self.reply.len(),
                self.elapsed_ms
            );
        }
        match &self.write_result {
            Err(e) => format!(
                "STEP 1 FAILED: bulk OUT b\"{}\" -> {e}. The write never completed, so the \
                 device is not listening on this pipe.",
                self.token_label
            ),
            Ok(n) if *n != self.token_bytes.len() => format!(
                "STEP 1 FAILED: bulk OUT b\"{}\" short write ({n} of {} bytes)",
                self.token_label,
                self.token_bytes.len()
            ),
            Ok(_) => match &self.read_result {
                Ok(n) => format!(
                    "STEP 2 FAILED: device replied with {n} byte(s) = {} (\"{}\"), not b\"LOKE\"",
                    lg::hex(&self.reply),
                    lg::ascii(&self.reply)
                ),
                Err(e) => format!(
                    "STEP 2 FAILED: no b\"LOKE\" reply -> {e}. The b\"{}\" write WAS accepted, \
                     so the pipe is live, but the bootloader produced no handshake answer.",
                    self.token_label
                ),
            },
        }
    }
}

/// Token variants in priority order. The 5-byte NUL-terminated form is what
/// brokkr-flash sends on USB; Heimdall and odin4 send 4 bytes.
pub const TOKEN_VARIANTS: [(&str, &[u8]); 4] = [
    ("ODIN\\0", b"ODIN\0"),
    ("ODIN", b"ODIN"),
    ("THOR", b"THOR"),
    ("THOR\\0", b"THOR\0"),
];

pub fn handshake_with<W: Wire + ?Sized>(
    wire: &mut W,
    token_label: &str,
    token: &[u8],
    timeout: Duration,
) -> HandshakeAttempt {
    println!(
        "\n>>> LOKE handshake on {}: OUT <- b\"{token_label}\" ({} bytes {})",
        wire.label(),
        token.len(),
        lg::hex(token)
    );
    println!("    then IN -> expect b\"LOKE\" (first 4 bytes of a 512-byte read buffer)");

    let start = std::time::Instant::now();

    // Drain stale bytes so a leftover byte is never mistaken for a reply.
    match wire.read(64, Duration::from_millis(60)) {
        Ok(b) if !b.is_empty() => lg::warn(&format!(
            "drained {} stale byte(s) before the handshake: {}",
            b.len(),
            lg::hex(&b)
        )),
        _ => {}
    }

    let write_result = wire.write_all(token, timeout).map(|()| token.len());
    match &write_result {
        Ok(n) => println!("    OUT -> Ok({n}) in {} ms", start.elapsed().as_millis()),
        Err(e) => println!("    OUT -> {e}"),
    }

    if write_result.is_err() {
        return HandshakeAttempt {
            token_label: token_label.to_string(),
            token_bytes: token.to_vec(),
            write_result,
            read_result: Err("not attempted (write failed)".into()),
            reply: Vec::new(),
            ok: false,
            elapsed_ms: start.elapsed().as_millis(),
        };
    }

    let (read_result, reply) = match wire.read(512, timeout) {
        Ok(buf) => {
            println!(
                "    IN  -> Ok({}) in {} ms",
                buf.len(),
                start.elapsed().as_millis()
            );
            if !buf.is_empty() {
                println!("        raw  : {}", lg::hex(&buf));
                println!("        text : \"{}\"", lg::ascii(&buf));
            }
            (Ok(buf.len()), buf)
        }
        Err(e) => {
            println!("    IN  -> {e}");
            (Err(e), Vec::new())
        }
    };

    let ok = reply.len() >= 4 && &reply[..4] == HANDSHAKE_IN;

    HandshakeAttempt {
        token_label: token_label.to_string(),
        token_bytes: token.to_vec(),
        write_result,
        read_result,
        reply,
        ok,
        elapsed_ms: start.elapsed().as_millis(),
    }
}

/// Sweep every documented token variant, stopping at the first success.
pub fn handshake_sweep<W: Wire + ?Sized>(wire: &mut W, timeout: Duration) -> Vec<HandshakeAttempt> {
    let mut out = Vec::new();
    for (i, (label, tok)) in TOKEN_VARIANTS.iter().enumerate() {
        if i > 0 {
            wire.clear_halt();
            std::thread::sleep(Duration::from_millis(200));
        }
        let a = handshake_with(wire, label, tok, timeout);
        let good = a.ok;
        out.push(a);
        if good {
            break;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Command exchange
// ---------------------------------------------------------------------------

/// Send a 1024-byte request box and read the 8-byte response box.
pub fn command<W: Wire + ?Sized>(
    wire: &mut W,
    request: &[u8; REQUEST_SIZE],
    expected_id: u32,
    label: &str,
    timeout: Duration,
) -> Result<Response, String> {
    let cmd = u32_le(request, 0);
    let sub = u32_le(request, 4);
    println!(
        "\n>>> {label}: {} cmd=0x{cmd:02x} sub=0x{sub:02x} ({REQUEST_SIZE} bytes, LE, zero padded)",
        command_name(cmd)
    );
    if lg::enabled(lg::DEBUG) {
        println!("    request[0..64] = {}", lg::hex(&request[..64]));
    }

    wire.write_all(request, timeout)
        .map_err(|e| format!("{label}: request write failed: {e}"))?;

    let buf = wire
        .read(512, timeout)
        .map_err(|e| format!("{label}: response read failed: {e}"))?;

    if buf.len() < RESPONSE_SIZE {
        return Err(format!(
            "{label}: response too short ({} bytes, need {RESPONSE_SIZE})",
            buf.len()
        ));
    }

    println!("    response hexdump ({} bytes):", buf.len());
    println!("{}", lg::hexdump(&buf[..buf.len().min(64)]));

    let resp = Response::parse(buf).ok_or_else(|| format!("{label}: could not parse response"))?;
    let line = resp.describe(expected_id);
    if resp.is_ok_for(expected_id) {
        lg::ok(&format!("{label}: {line}"));
    } else {
        lg::fail(&format!("{label}: {line}"));
    }
    Ok(resp)
}

// ---------------------------------------------------------------------------
// Read-only session: begin, device type, PIT
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct SessionReport {
    pub session_ack: Option<u32>,
    pub protocol_version: Option<u16>,
    pub compressed: bool,
    pub device_model: Option<String>,
    pub pit_ok: bool,
    pub pit_entries: usize,
}

pub fn run_readonly_session<W: Wire + ?Sized>(
    wire: &mut W,
    timeout: Duration,
    do_pit: bool,
) -> SessionReport {
    let mut rep = SessionReport::default();

    // ---- 0x64/0x00 : begin session, negotiate protocol version ------------
    let req = begin_session_request();
    if let Ok(r) = command(
        wire,
        &req,
        RQT_INIT,
        "BeginSession (0x64/0x00, proto 0x7FFFFFFF)",
        timeout,
    ) {
        rep.session_ack = Some(r.ack as u32);
        rep.protocol_version = Some(r.protocol_version());
        rep.compressed = r.supports_compressed();
        lg::ok(&format!(
            "bootloader protocol version = {} (ack 0x{:08x}); compressed download {}",
            r.protocol_version(),
            r.ack as u32,
            if rep.compressed { "SUPPORTED" } else { "not supported" }
        ));
        let (pkt, parts) = if r.protocol_version() <= 1 {
            (131_072u32, 240u32)
        } else {
            (1_048_576u32, 30u32)
        };
        println!("    negotiated transfer params: packet size {pkt} bytes, max {parts} parts/sequence");

        // Protocol >= 2 requires the host to declare the part size.
        if r.protocol_version() >= 2 {
            let req = set_packet_size_request(pkt);
            let _ = command(wire, &req, RQT_INIT, "SetFilePartSize (0x64/0x05)", timeout);
        }
    } else {
        return rep;
    }

    // ---- 0x64/0x01 : device type / model code ----------------------------
    let req = device_type_request();
    if let Ok(r) = command(wire, &req, RQT_INIT, "DeviceType (0x64/0x01)", timeout) {
        let model_code = if r.raw.len() >= 12 {
            u32_le(&r.raw, 8)
        } else {
            r.ack as u32
        };
        let model = format!("SM-{model_code}");
        lg::ok(&format!("device model code = {model_code} -> \"{model}\""));
        println!("    raw device-type response payload:");
        println!("      hex : {}", lg::hex(&r.raw));
        println!("      text: \"{}\"", lg::ascii(&r.raw));
        rep.device_model = Some(model);
    }

    // ---- 0x65 : read-only PIT download -----------------------------------
    if do_pit {
        match download_pit(wire, timeout) {
            Ok(n) => {
                rep.pit_ok = true;
                rep.pit_entries = n;
            }
            Err(e) => lg::fail(&format!("PIT download failed: {e}")),
        }
    } else {
        lg::info("PIT download skipped");
    }

    // ---- 0x67/0x00 : close the session cleanly (no reboot) ---------------
    let req = end_session_request();
    let _ = command(wire, &req, RQT_CLOSE, "EndSession (0x67/0x00) - no reboot", timeout);

    rep
}

/// Heimdall/odin4 `ReceivePitFile`, read-only.
pub fn download_pit<W: Wire + ?Sized>(wire: &mut W, timeout: Duration) -> Result<usize, String> {
    println!("\n---- READ-ONLY PIT download (0x65) ----");

    // 1. request PIT size
    let req = pit_dump_request();
    let resp = command(wire, &req, RQT_PIT, "RequestPitDump (0x65/0x01)", timeout)?;
    if resp.is_bootloader_fail() {
        return Err(format!("bootloader rejected the PIT request (ack {})", resp.ack));
    }

    let size = resp.ack;
    if size <= 0 || size > 1_048_576 {
        return Err(format!("implausible PIT size reported by device: {size}"));
    }
    lg::ok(&format!("device reports PIT size = {size} bytes"));

    let size = size as usize;
    let blocks = size.div_ceil(PIT_BLOCK_SIZE);
    println!("    {blocks} block(s) of {PIT_BLOCK_SIZE} bytes");

    let mut pit = vec![0u8; size];
    let mut trailing: Vec<u8> = Vec::new();

    for i in 0..blocks as u32 {
        let req = pit_block_request(i);
        let resp = command(
            wire,
            &req,
            RQT_PIT,
            &format!("PitBlock {i} (0x65/0x02)"),
            timeout,
        )?;
        if resp.is_bootloader_fail() {
            return Err(format!("bootloader rejected PIT block {i} (ack {})", resp.ack));
        }

        let off = i as usize * PIT_BLOCK_SIZE;
        let want = (size - off).min(PIT_BLOCK_SIZE);

        // The raw block may be glued to the 8-byte response box or may arrive
        // as a separate transfer. Handle both and log which happened.
        let mut data = resp.extra.clone();
        if data.len() < want {
            println!(
                "    block {i}: response box carried {} payload byte(s); reading {} more",
                data.len(),
                want - data.len()
            );
            let extra = wire
                .read(want - data.len(), timeout.max(Duration::from_millis(5000)))
                .map_err(|e| format!("PIT block {i}: extra read failed: {e}"))?;
            data.extend_from_slice(&extra);
        }

        if data.len() < want {
            return Err(format!(
                "PIT block {i}: got {} bytes, needed {want}",
                data.len()
            ));
        }

        pit[off..off + want].copy_from_slice(&data[..want]);
        if data.len() > want {
            trailing.extend_from_slice(&data[want..]);
        }
        println!(
            "    block {i}: ok ({} bytes payload, {} bytes in this step)",
            want,
            data.len()
        );
    }

    if !trailing.is_empty() {
        lg::warn(&format!(
            "{} trailing byte(s) after the final block: {}",
            trailing.len(),
            lg::hex(&trailing)
        ));
    }
    if let Ok(b) = wire.read(512, Duration::from_millis(200)) {
        if !b.is_empty() {
            lg::warn(&format!("drained {} unexpected byte(s) before EndPitDump", b.len()));
        }
    }

    let req = pit_complete_request();
    if let Ok(resp) = command(wire, &req, RQT_PIT, "EndPitDump (0x65/0x03)", timeout) {
        if resp.is_ok_for(RQT_PIT) {
            lg::ok("EndPitDump acknowledged");
        }
    }

    // ---- persist + parse -------------------------------------------------
    let path = std::env::current_dir()
        .unwrap_or_else(|_| ".".into())
        .join("device.pit");
    match std::fs::write(&path, &pit) {
        Ok(()) => lg::ok(&format!("wrote {} ({} bytes)", path.display(), pit.len())),
        Err(e) => lg::warn(&format!("could not write {}: {e}", path.display())),
    }

    println!(
        "\n    first 160 bytes of the PIT:\n{}",
        lg::hexdump(&pit[..pit.len().min(160)])
    );

    let parsed = PitData::unpack(&pit)?;
    println!(
        "\n    PIT header: magic OK, entry_count={}, com_tar2=\"{}\", cpu_bl_id=\"{}\"",
        parsed.entry_count,
        lg::ascii(&parsed.com_tar2),
        lg::ascii(&parsed.cpu_bl_id)
    );

    println!(
        "\n  {:<22} {:>4} {:>4} {:>6} {:>14} {:>14} {:>12}",
        "partition", "bin", "type", "ident", "block_size", "block_count", "size (MiB)"
    );
    println!("  {}", "-".repeat(92));
    for e in &parsed.entries {
        println!(
            "  {:<22} {:>4} {:>4} {:>6} {:>14} {:>14} {:>12.1}",
            if e.partition_name.is_empty() {
                "<unnamed>"
            } else {
                &e.partition_name
            },
            e.binary_name(),
            e.dev_type_name(),
            e.identifier,
            e.block_size_or_offset,
            e.block_count,
            e.size_bytes() as f64 / (1024.0 * 1024.0)
        );
    }

    lg::ok(&format!(
        "PIT parsed successfully: {} partition entries",
        parsed.entries.len()
    ));
    Ok(parsed.entries.len())
}
