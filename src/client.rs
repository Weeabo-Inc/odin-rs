//! libusb (rusb) transport: descriptor enumeration, interface selection,
//! the LOKE handshake and Odin request/response command exchange.

use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use rusb::{Context, DeviceHandle, Direction, TransferType, UsbContext};

use crate::log;
#[allow(unused_imports)]
use crate::protocol::*;

pub const SAMSUNG_VID: u16 = 0x04e8;
/// Product IDs Samsung uses in download mode (odin4 `is_known_download_pid`).
pub const KNOWN_DOWNLOAD_PIDS: &[u16] = &[0x6601, 0x685d, 0x68c3, 0x68ef, 0x4eee, 0x4eef];
/// The interface class Odin/Heimdall latch onto.
pub const USB_CLASS_CDC_DATA: u8 = 0x0a;

pub const USB_RETRY_COUNT: u32 = 5;

// libusb can only have one outstanding transfer context per process in
// practice; tests are serialised so a failed attempt cannot poison the next.
fn usb_lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

pub fn acquire_usb() -> MutexGuard<'static, ()> {
    match usb_lock().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

// ---------------------------------------------------------------------------
// Descriptors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct EndpointInfo {
    pub address: u8,
    pub direction: Direction,
    pub transfer_type: TransferType,
    pub max_packet_size: u16,
    pub interval: u8,
}

impl EndpointInfo {
    pub fn dir_str(&self) -> &'static str {
        match self.direction {
            Direction::In => "IN ",
            Direction::Out => "OUT",
        }
    }
    pub fn is_bulk(&self) -> bool {
        self.transfer_type == TransferType::Bulk
    }
    pub fn is_bulk_in(&self) -> bool {
        self.is_bulk() && self.direction == Direction::In
    }
    pub fn is_bulk_out(&self) -> bool {
        self.is_bulk() && self.direction == Direction::Out
    }
}

#[derive(Debug, Clone)]
pub struct AltSettingInfo {
    pub number: u8,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub endpoints: Vec<EndpointInfo>,
}

#[derive(Debug, Clone)]
pub struct InterfaceInfo {
    pub number: u8,
    pub altsettings: Vec<AltSettingInfo>,
}

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub bus: u8,
    pub address: u8,
    pub port_numbers: Vec<u8>,
    pub speed: String,
    pub vendor_id: u16,
    pub product_id: u16,
    pub bcd_usb: u16,
    pub bcd_device: u16,
    pub device_class: u8,
    pub device_subclass: u8,
    pub device_protocol: u8,
    pub max_packet_size0: u8,
    pub num_configurations: u8,
    pub num_interfaces: usize,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub serial: Option<String>,
    pub interfaces: Vec<InterfaceInfo>,
}

impl DeviceInfo {
    pub fn path(&self) -> String {
        let mut s = format!("{:03}:{:03}", self.bus, self.address);
        if !self.port_numbers.is_empty() {
            s.push(':');
            s.push_str(
                &self
                    .port_numbers
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join("."),
            );
        }
        s
    }
}

pub fn is_known_download_pid(pid: u16) -> bool {
    KNOWN_DOWNLOAD_PIDS.contains(&pid)
}

/// rusb's `Version(u8, u8, u8)` holds (major, minor, sub-minor) of a BCD-coded
/// field; reassemble it so the dump prints the raw `bcdUSB` / `bcdDevice`.
fn version_to_bcd(v: rusb::Version) -> u16 {
    let rusb::Version(major, minor, sub) = v;
    ((major as u16) << 8) | (((minor as u16) & 0x0f) << 4) | ((sub as u16) & 0x0f)
}

/// Find every Samsung device that looks like a download-mode gadget.
/// odin4 accepts any Samsung VID device whose interface scores well, plus
/// known download PIDs, so we do the same but record *why* each matched.
pub fn find_odin_devices(ctx: &Context) -> rusb::Result<Vec<(rusb::Device<Context>, String)>> {
    let mut found = Vec::new();
    for dev in ctx.devices()?.iter() {
        let desc = match dev.device_descriptor() {
            Ok(d) => d,
            Err(e) => {
                log::debug(&format!("device_descriptor failed: {e:?}"));
                continue;
            }
        };
        if desc.vendor_id() != SAMSUNG_VID {
            continue;
        }
        let pid = desc.product_id();
        let known = is_known_download_pid(pid);
        let cdc = has_cdc_data_interface(&dev).unwrap_or(false);
        if known || cdc {
            let reason = match (known, cdc) {
                (true, true) => "known download-mode PID + CDC-Data interface",
                (true, false) => "known download-mode PID",
                (false, true) => "CDC-Data interface on Samsung VID",
                _ => unreachable!(),
            };
            found.push((dev, reason.to_string()));
        } else {
            log::debug(&format!(
                "skipping Samsung {:04x}:{pid:04x} (not a known download PID and no CDC-Data interface)",
                SAMSUNG_VID
            ));
        }
    }
    // Prefer known download PIDs, and among those the newest-PID-first order
    // does not matter; stable sort keeps bus order.
    found.sort_by_key(|(d, _)| {
        let pid = d.device_descriptor().map(|x| x.product_id()).unwrap_or(0);
        if is_known_download_pid(pid) { 0 } else { 1 }
    });
    Ok(found)
}

fn has_cdc_data_interface(dev: &rusb::Device<Context>) -> rusb::Result<bool> {
    let cfg = dev.active_config_descriptor()?;
    for iface in cfg.interfaces() {
        for alt in iface.descriptors() {
            if alt.class_code() == USB_CLASS_CDC_DATA && alt.num_endpoints() == 2 {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub fn describe(
    dev: &rusb::Device<Context>,
    handle: Option<&DeviceHandle<Context>>,
) -> rusb::Result<DeviceInfo> {
    let desc = dev.device_descriptor()?;
    let speed = match dev.speed() {
        rusb::Speed::Low => "1.5 Mbit/s (low speed)".to_string(),
        rusb::Speed::Full => "12 Mbit/s (full speed)".to_string(),
        rusb::Speed::High => "480 Mbit/s (high speed)".to_string(),
        rusb::Speed::Super => "5 Gbit/s (super speed)".to_string(),
        rusb::Speed::Unknown => "unknown".to_string(),
        other => format!("{other:?}"),
    };

    let read_string = |idx: u8| -> Option<String> {
        if idx == 0 {
            return None;
        }
        handle?.read_string_descriptor_ascii(idx).ok()
    };

    // Collect ALL configurations, not just the active one, so the descriptor
    // dump is genuinely complete.
    let mut interfaces = Vec::new();
    let mut num_interfaces = 0usize;
    for n in 0..desc.num_configurations() {
        let cfg = match dev.config_descriptor(n) {
            Ok(c) => c,
            Err(e) => {
                log::warn(&format!("config_descriptor({n}) failed: {e:?}"));
                continue;
            }
        };
        for iface in cfg.interfaces() {
            num_interfaces += 1;
            let mut altsettings = Vec::new();
            for alt in iface.descriptors() {
                let mut endpoints = Vec::new();
                for ep in alt.endpoint_descriptors() {
                    endpoints.push(EndpointInfo {
                        address: ep.address(),
                        direction: ep.direction(),
                        transfer_type: ep.transfer_type(),
                        max_packet_size: ep.max_packet_size(),
                        interval: ep.interval(),
                    });
                }
                altsettings.push(AltSettingInfo {
                    number: alt.setting_number(),
                    class: alt.class_code(),
                    subclass: alt.sub_class_code(),
                    protocol: alt.protocol_code(),
                    endpoints,
                });
            }
            interfaces.push(InterfaceInfo {
                number: iface.number(),
                altsettings,
            });
        }
    }

    Ok(DeviceInfo {
        bus: dev.bus_number(),
        address: dev.address(),
        port_numbers: dev.port_numbers().unwrap_or_default(),
        speed,
        vendor_id: desc.vendor_id(),
        product_id: desc.product_id(),
        bcd_usb: version_to_bcd(desc.usb_version()),
        bcd_device: version_to_bcd(desc.device_version()),
        device_class: desc.class_code(),
        device_subclass: desc.sub_class_code(),
        device_protocol: desc.protocol_code(),
        max_packet_size0: desc.max_packet_size(),
        num_configurations: desc.num_configurations(),
        num_interfaces,
        manufacturer: read_string(desc.manufacturer_string_index().unwrap_or(0)),
        product: read_string(desc.product_string_index().unwrap_or(0)),
        serial: read_string(desc.serial_number_string_index().unwrap_or(0)),
        interfaces,
    })
}

pub fn dump_tree(info: &DeviceInfo) {
    println!(
        "Bus {:03} Device {:03}  ID {:04x}:{:04x}   path {}{}",
        info.bus,
        info.address,
        info.vendor_id,
        info.product_id,
        info.path(),
        if is_known_download_pid(info.product_id) {
            "  [known Samsung download-mode PID]"
        } else {
            ""
        }
    );
    println!("  bcdUSB              : 0x{:04x}", info.bcd_usb);
    println!("  bcdDevice           : 0x{:04x}", info.bcd_device);
    println!(
        "  bDeviceClass        : 0x{:02x}.0x{:02x}.0x{:02x}",
        info.device_class, info.device_subclass, info.device_protocol
    );
    println!("  bMaxPacketSize0     : {}", info.max_packet_size0);
    println!(
        "  bNumConfigurations  : {}  (interfaces seen: {})",
        info.num_configurations, info.num_interfaces
    );
    println!("  negotiated speed    : {}", info.speed);
    println!(
        "  iManufacturer       : {}",
        info.manufacturer.as_deref().unwrap_or("<unavailable>")
    );
    println!(
        "  iProduct            : {}",
        info.product.as_deref().unwrap_or("<unavailable>")
    );
    println!(
        "  iSerialNumber       : {}",
        info.serial.as_deref().unwrap_or("<unavailable>")
    );

    for iface in &info.interfaces {
        for alt in &iface.altsettings {
            println!(
                "  interface[{}].altsetting[{}] : class 0x{:02x} subclass 0x{:02x} protocol 0x{:02x}  endpoints={}{}",
                iface.number,
                alt.number,
                alt.class,
                alt.subclass,
                alt.protocol,
                alt.endpoints.len(),
                if alt.class == USB_CLASS_CDC_DATA && alt.endpoints.len() == 2 {
                    "   <== CDC-Data with 2 endpoints (Heimdall/odin4 target)"
                } else {
                    ""
                }
            );
            for (k, ep) in alt.endpoints.iter().enumerate() {
                println!(
                    "      endpoint[{k}] addr=0x{:02x} {} {:?}  wMaxPacketSize={}  bInterval={}",
                    ep.address,
                    ep.dir_str(),
                    ep.transfer_type,
                    ep.max_packet_size,
                    ep.interval
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Interface / endpoint selection (odin4 `find_best_interface` scoring)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndpointPair {
    pub score: i32,
    pub interface: u8,
    pub altsetting: u8,
    pub ep_in: u8,
    pub ep_out: u8,
    pub ep_in_max_packet: u16,
    pub ep_out_max_packet: u16,
    pub interface_class: u8,
    pub num_endpoints: u8,
    pub bulk_endpoints: u8,
}

impl EndpointPair {
    pub fn heimdall_match(&self) -> bool {
        self.interface_class == USB_CLASS_CDC_DATA && self.num_endpoints == 2
    }
}

/// Rank every interface that offers one bulk IN and one bulk OUT endpoint.
/// Highest score first, mirroring odin4's scoring so the primary choice is
/// identical to the client that is known to work with this device family.
pub fn rank_endpoint_pairs(info: &DeviceInfo) -> Vec<EndpointPair> {
    let mut out = Vec::new();

    for iface in &info.interfaces {
        for alt in &iface.altsettings {
            let in_ep = alt.endpoints.iter().find(|e| e.is_bulk_in());
            let out_ep = alt.endpoints.iter().find(|e| e.is_bulk_out());
            let (Some(in_ep), Some(out_ep)) = (in_ep, out_ep) else {
                continue;
            };
            let bulk_endpoints = alt.endpoints.iter().filter(|e| e.is_bulk()).count() as u8;

            let mut score = 0i32;
            if alt.class == USB_CLASS_CDC_DATA && alt.endpoints.len() == 2 {
                score += 100;
            }
            score += 50;
            if bulk_endpoints == 2 {
                score += 10;
            }

            out.push(EndpointPair {
                score,
                interface: iface.number,
                altsetting: alt.number,
                ep_in: in_ep.address,
                ep_out: out_ep.address,
                ep_in_max_packet: in_ep.max_packet_size,
                ep_out_max_packet: out_ep.max_packet_size,
                interface_class: alt.class,
                num_endpoints: alt.endpoints.len() as u8,
                bulk_endpoints,
            });
        }
    }

    // Stable sort, descending score.
    out.sort_by_key(|p| -p.score);
    out
}

// ---------------------------------------------------------------------------
// Transfer reporting
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct TransferReport {
    pub endpoint: u8,
    pub requested: usize,
    pub result: Result<usize, rusb::Error>,
    pub elapsed_ms: u128,
}

impl TransferReport {
    pub fn outcome_str(&self) -> String {
        match &self.result {
            Ok(n) => format!("Ok({n}) in {} ms", self.elapsed_ms),
            Err(e) => format!("{e:?} ({e}) in {} ms", self.elapsed_ms),
        }
    }
    pub fn is_ok(&self) -> bool {
        self.result.is_ok()
    }
    pub fn len(&self) -> usize {
        self.result.unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub struct Client {
    pub handle: DeviceHandle<Context>,
    pub pair: EndpointPair,
    pub claimed: bool,
    /// Kept for diagnostics: counts handshake writes issued on this handle.
    #[allow(dead_code)]
    pub handshake_writes: usize,
    /// ZLP support is disabled for the session after the first failure,
    /// exactly like odin4's `odin_supports_zlp`.
    pub zlp_ok: bool,
    pub out_max_packet: u16,
}

impl Client {
    pub fn open(dev: &rusb::Device<Context>, pair: EndpointPair) -> Result<Client, String> {
        let handle = dev
            .open()
            .map_err(|e| format!("libusb_open failed: {e:?} ({e})"))?;

        // Best effort: on Linux this detaches the kernel driver; on Windows
        // with WinUSB there is nothing to detach and the call is a no-op or
        // an error we can safely ignore.
        match handle.set_auto_detach_kernel_driver(true) {
            Ok(()) => log::debug("auto-detach kernel driver enabled"),
            Err(e) => log::debug(&format!(
                "set_auto_detach_kernel_driver -> {e:?} (expected on Windows/WinUSB)"
            )),
        }

        handle
            .claim_interface(pair.interface)
            .map_err(|e| format!("claim_interface({}) failed: {e:?} ({e})", pair.interface))?;

        if let Err(e) = handle.set_alternate_setting(pair.interface, pair.altsetting) {
            log::warn(&format!(
                "set_alternate_setting({}, {}) failed: {e:?} ({e}) - continuing",
                pair.interface, pair.altsetting
            ));
        }

        // Clear any halt/stall left behind by a previous aborted session.
        for ep in [pair.ep_in, pair.ep_out] {
            if let Err(e) = handle.clear_halt(ep) {
                log::debug(&format!("clear_halt(0x{ep:02x}) -> {e:?}"));
            }
        }

        let out_max_packet = if pair.ep_out_max_packet == 0 {
            64
        } else {
            pair.ep_out_max_packet
        };

        Ok(Client {
            handle,
            pair,
            claimed: true,
            handshake_writes: 0,
            zlp_ok: true,
            out_max_packet,
        })
    }

    pub fn release(&mut self) {
        if self.claimed {
            if let Err(e) = self.handle.release_interface(self.pair.interface) {
                log::warn(&format!("release_interface({}) -> {e:?}", self.pair.interface));
            }
            self.claimed = false;
        }
    }

    pub fn clear_halt(&self, endpoint: u8) {
        match self.handle.clear_halt(endpoint) {
            Ok(()) => log::info(&format!("clear_halt(0x{endpoint:02x}) -> ok")),
            Err(e) => log::warn(&format!("clear_halt(0x{endpoint:02x}) -> {e:?} ({e})")),
        }
    }

    // -- raw transfers ------------------------------------------------------

    #[allow(dead_code)]
    fn log_transfer(&self, rep: &TransferReport, data: Option<&[u8]>, tag: &str) {
        let dir = if rep.endpoint & 0x80 != 0 { "IN " } else { "OUT" };
        println!(
            "    bulk {dir} ep=0x{:02x} len={:<5} [{}] -> {}",
            rep.endpoint,
            rep.requested,
            tag,
            rep.outcome_str()
        );
        if let Some(d) = data {
            if !d.is_empty() {
                println!("        raw  : {}", log::hex(d));
                println!("        text : \"{}\"", log::ascii(d));
            }
        }
    }

    /// Read up to `len` bytes in one bulk transfer (odin4 `bulk_read_once`).
    pub fn read_bulk(&self, len: usize, timeout: Duration) -> (TransferReport, Vec<u8>) {
        let mut buf = vec![0u8; len.max(1)];
        let start = Instant::now();
        let result = self.handle.read_bulk(self.pair.ep_in, &mut buf, timeout);
        let n = result.unwrap_or(0).min(buf.len());
        buf.truncate(n);
        (
            TransferReport {
                endpoint: self.pair.ep_in,
                requested: len,
                result,
                elapsed_ms: start.elapsed().as_millis(),
            },
            buf,
        )
    }

    /// odin4 `send_zlp`: a zero-length OUT transfer terminates a burst whose
    /// length is an exact multiple of wMaxPacketSize.
    pub fn send_zlp(&mut self, timeout: Duration) -> bool {
        let start = Instant::now();
        let res = self.handle.write_bulk(self.pair.ep_out, &[], timeout);
        match res {
            Ok(_) => {
                log::debug(&format!("ZLP sent in {} ms", start.elapsed().as_millis()));
                true
            }
            Err(e) => {
                log::warn(&format!("ZLP failed: {e:?} ({e}) - disabling ZLP for this session"));
                self.zlp_ok = false;
                false
            }
        }
    }

    /// odin4 `bulk_write_all`: retry with exponential backoff, clear halt on
    /// EPIPE, shrink the chunk after a timeout/pipe error.
    pub fn write_all(&mut self, data: &[u8], timeout: Duration) -> TransferReport {
        let overall = Instant::now();
        let mut chunk_limit = data.len().max(1);
        let mut offset = 0usize;
        let mut last: Result<usize, rusb::Error> = Ok(0);

        for attempt in 0..USB_RETRY_COUNT {
            while offset < data.len() {
                let to_send = (data.len() - offset).min(chunk_limit);
                let start = Instant::now();
                let r = self
                    .handle
                    .write_bulk(self.pair.ep_out, &data[offset..offset + to_send], timeout);
                match r {
                    Ok(n) => {
                        last = Ok(n);
                        if n == 0 {
                            break;
                        }
                        offset += n;
                        let _ = start;
                    }
                    Err(rusb::Error::NoDevice) => {
                        return TransferReport {
                            endpoint: self.pair.ep_out,
                            requested: data.len(),
                            result: Err(rusb::Error::NoDevice),
                            elapsed_ms: overall.elapsed().as_millis(),
                        };
                    }
                    Err(e) => {
                        if e == rusb::Error::Pipe {
                            self.clear_halt(self.pair.ep_out);
                        }
                        if e == rusb::Error::Pipe || e == rusb::Error::Timeout {
                            // odin4 reduces the chunk size to 16 KiB
                            chunk_limit = 0x4000.min(chunk_limit);
                            log::warn(&format!(
                                "{e:?} on bulk OUT - reducing chunk size to {chunk_limit} bytes"
                            ));
                        }
                        last = Err(e);
                        break;
                    }
                }
            }

            if offset == data.len() {
                if self.zlp_ok
                    && self.out_max_packet != 0
                    && data.len().is_multiple_of(self.out_max_packet as usize)
                {
                    self.send_zlp(timeout);
                }
                return TransferReport {
                    endpoint: self.pair.ep_out,
                    requested: data.len(),
                    result: Ok(data.len()),
                    elapsed_ms: overall.elapsed().as_millis(),
                };
            }

            if attempt + 1 < USB_RETRY_COUNT {
                let delay = retry_backoff_ms(attempt);
                log::warn(&format!(
                    "bulk OUT incomplete ({} of {} bytes) - retry {} in {delay} ms",
                    offset,
                    data.len(),
                    attempt + 1
                ));
                std::thread::sleep(Duration::from_millis(delay));
            }
        }

        TransferReport {
            endpoint: self.pair.ep_out,
            requested: data.len(),
            result: if offset == data.len() { Ok(offset) } else { last.map(|_| offset) },
            elapsed_ms: overall.elapsed().as_millis(),
        }
    }

    /// odin4 `bulk_read_once` but retried like the write path.
    pub fn read_with_retry(&mut self, len: usize, timeout: Duration) -> (TransferReport, Vec<u8>) {
        let mut last_rep = TransferReport {
            endpoint: self.pair.ep_in,
            requested: len,
            result: Err(rusb::Error::Timeout),
            elapsed_ms: 0,
        };
        for attempt in 0..USB_RETRY_COUNT {
            let (rep, buf) = self.read_bulk(len, timeout);
            match &rep.result {
                Ok(_) => return (rep, buf),
                Err(rusb::Error::NoDevice) => return (rep, buf),
                Err(e) => {
                    if *e == rusb::Error::Pipe {
                        self.clear_halt(self.pair.ep_in);
                    }
                    log::error(&format!("bulk IN failed: {e:?} ({e})"));
                    if attempt + 1 < USB_RETRY_COUNT {
                        let delay = retry_backoff_ms(attempt);
                        std::thread::sleep(Duration::from_millis(delay));
                    }
                    last_rep = rep;
                }
            }
        }
        (last_rep, Vec::new())
    }

    // -- handshake ---------------------------------------------------------

    /// The handshake and all command exchange live in `session.rs` so the USB
    /// and serial backends provably send identical bytes. `UsbWire` below
    /// adapts this client to that interface.

    /// Diagnostic only: the original skeleton's bRequest 0x42 control probe.
    /// The published protocol is bulk-only, so this is not the real path.
    pub fn control_handshake_probe(&mut self, timeout: Duration) -> bool {
        println!("\n>>> control-transfer probe (bRequest 0x42) - diagnostic only");
        let mut any_ok = false;
        for (label, value, index) in [
            ("value=0x0000 index=0x0000", 0x0000u16, 0x0000u16),
            ("value=0x0064 index=0x0000", 0x0064u16, 0x0000u16),
            ("value=0x0064 index=0x0000 idx=iface", 0x0064u16, self.pair.interface as u16),
        ] {
            let mut buf = vec![0u8; 1024];
            let start = Instant::now();
            match self
                .handle
                .read_control(0xc0, 0x42, value, index, &mut buf, timeout)
            {
                Ok(n) => {
                    buf.truncate(n);
                    any_ok = true;
                    println!(
                        "    read_control (0xC0,0x42,{label}) -> Ok({n}) in {} ms",
                        start.elapsed().as_millis()
                    );
                    println!("        raw : {}", log::hex(&buf));
                    println!("        text: \"{}\"", log::ascii(&buf));
                }
                Err(e) => println!(
                    "    read_control (0xC0,0x42,{label}) -> {e:?} ({e}) in {} ms",
                    start.elapsed().as_millis()
                ),
            }
        }
        if !any_ok {
            log::fail("control-transfer probe: every read_control errored (bulk is the real path)");
        }
        any_ok
    }
}

/// Adapter so the shared session logic can drive this USB client.
pub struct UsbWire<'a> {
    pub client: &'a mut Client,
}

impl<'a> UsbWire<'a> {
    pub fn new(client: &'a mut Client) -> Self {
        UsbWire { client }
    }
}

impl<'a> crate::session::Wire for UsbWire<'a> {
    fn write_all(&mut self, data: &[u8], timeout: Duration) -> Result<(), String> {
        let rep = self.client.write_all(data, timeout);
        let _ = &rep;
        if rep.is_ok() && rep.len() == data.len() {
            Ok(())
        } else {
            Err(format!(
                "bulk OUT ep=0x{:02x} ({} bytes): {}",
                self.client.pair.ep_out,
                data.len(),
                rep.outcome_str()
            ))
        }
    }

    fn read(&mut self, len: usize, timeout: Duration) -> Result<Vec<u8>, String> {
        let (rep, buf) = self.client.read_with_retry(len, timeout);
        if rep.is_ok() {
            Ok(buf)
        } else {
            Err(format!(
                "bulk IN ep=0x{:02x} ({len} bytes requested): {}",
                self.client.pair.ep_in,
                rep.outcome_str()
            ))
        }
    }

    fn clear_halt(&mut self) {
        self.client.clear_halt(self.client.pair.ep_out);
        self.client.clear_halt(self.client.pair.ep_in);
    }

    fn label(&self) -> String {
        format!(
            "WinUSB iface {} (bulk OUT 0x{:02x} / bulk IN 0x{:02x})",
            self.client.pair.interface, self.client.pair.ep_out, self.client.pair.ep_in
        )
    }
}

pub fn retry_backoff_ms(attempt: u32) -> u64 {
    let mut ms = 100u64;
    for _ in 0..attempt {
        ms *= 2;
        if ms > 1500 {
            return 1500;
        }
    }
    ms
}
