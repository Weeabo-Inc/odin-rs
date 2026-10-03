//! Windows VCOM / serial backend for the Odin protocol.
//!
//! WHY THIS EXISTS
//! brokkr-flash - the current maintained Samsung flashing client, which
//! explicitly supports PID 0x685D - does **not** use WinUSB on Windows. Its
//! Windows backend (`src/platform/windows/usbfs_device.cpp`) opens the device
//! as a COM port with `CreateFile("\\.\COMx")` and drives it with
//! `SetCommState` / `SetCommTimeouts` / `PurgeComm`, i.e. through the CDC
//! serial stack (usbser.sys), exactly like real Samsung Odin.
//!
//! The download-mode gadget on this device advertises
//! `USB\Class_02&SubClass_02&Prot_01`, a standard CDC-ACM compatible ID, and
//! exposes a CDC control interface (interrupt IN 0x82) plus a CDC data
//! interface. Zadig has force-bound WinUSB to it, which bypasses the serial
//! stack entirely. When the device is bound to usbser.sys instead, a COM port
//! appears and the same protocol can be spoken over it.
//!
//! This module provides the identical command exchange as `client.rs` by
//! implementing the shared `session::Wire` trait, so both transports provably
//! put the same bytes on the wire.

#![cfg(windows)]

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::time::{Duration, Instant};

use windows_sys::Win32::Devices::Communication::{
    ClearCommError, GetCommState, PurgeComm, SetCommState, SetCommTimeouts, COMMTIMEOUTS, COMSTAT,
    DCB, PURGE_RXABORT, PURGE_RXCLEAR, PURGE_TXABORT, PURGE_TXCLEAR,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};

use crate::log;
use crate::session::Wire;

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;

/// Baud rate the reference client configures. Notional for a USB CDC link, but
/// the driver/bootloader state machine expects a well-formed DCB.
const BAUD_115200: u32 = 115_200;
const NOPARITY: u8 = 0;
const ONESTOPBIT: u8 = 0;

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

fn win32_error(context: &str) -> String {
    let code = unsafe { GetLastError() };
    format!("{context} failed (Win32 error {code})")
}

/// A COM-port handle carrying the Odin protocol.
pub struct SerialClient {
    handle: HANDLE,
    pub port_name: String,
    pub timeout: Duration,
    pub reads: usize,
    pub writes: usize,
    /// Consecutive zero-byte reads, for diagnostics.
    pub empty_reads: usize,
}

impl SerialClient {
    /// COM ports known to the serial device map.
    pub fn find_samsung_ports() -> Vec<String> {
        match read_serialcomm_registry() {
            Ok(p) => p,
            Err(e) => {
                log::warn(&format!("could not enumerate COM ports: {e}"));
                Vec::new()
            }
        }
    }

    pub fn open(port_name: &str, timeout: Duration) -> Result<SerialClient, String> {
        let path = if port_name.starts_with("\\\\.\\") {
            port_name.to_string()
        } else {
            format!("\\\\.\\{port_name}")
        };
        let wpath = wide(&path);

        let handle = unsafe {
            CreateFileW(
                wpath.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };

        if handle == INVALID_HANDLE_VALUE {
            return Err(win32_error(&format!("CreateFileW({path})")));
        }

        let mut client = SerialClient {
            handle,
            port_name: port_name.to_string(),
            timeout,
            reads: 0,
            writes: 0,
            empty_reads: 0,
        };

        if let Err(e) = client.configure() {
            client.close();
            return Err(e);
        }

        log::ok(&format!(
            "opened serial port {port_name} as {path} (115200 8N1, read timeout {} ms)",
            timeout.as_millis()
        ));
        Ok(client)
    }

    /// Mirror brokkr's DCB setup (115200 8N1). The control-flow bitfields are
    /// left exactly as `GetCommState` reported them and only the line
    /// parameters are applied, which avoids having to hand-encode the
    /// `fBinary`/`fDtrControl`/... bitfield layout.
    fn configure(&mut self) -> Result<(), String> {
        let mut dcb: DCB = unsafe { std::mem::zeroed() };
        dcb.DCBlength = std::mem::size_of::<DCB>() as u32;

        if unsafe { GetCommState(self.handle, &mut dcb) } == 0 {
            return Err(win32_error("GetCommState"));
        }

        dcb.BaudRate = BAUD_115200;
        dcb.ByteSize = 8;
        dcb.StopBits = ONESTOPBIT;
        dcb.Parity = NOPARITY;

        if unsafe { SetCommState(self.handle, &dcb) } == 0 {
            return Err(win32_error("SetCommState"));
        }

        // ReadIntervalTimeout = MAXDWORD with a constant overall timeout is the
        // canonical "return as soon as something arrives, else time out"
        // configuration, and matches what brokkr sets up.
        let ms = self.timeout.as_millis().min(u32::MAX as u128) as u32;
        let timeouts = COMMTIMEOUTS {
            ReadIntervalTimeout: u32::MAX,
            ReadTotalTimeoutMultiplier: u32::MAX,
            ReadTotalTimeoutConstant: ms,
            WriteTotalTimeoutMultiplier: 0,
            WriteTotalTimeoutConstant: ms.max(1),
        };

        if unsafe { SetCommTimeouts(self.handle, &timeouts) } == 0 {
            return Err(win32_error("SetCommTimeouts"));
        }

        Ok(())
    }

    /// Discard anything stale in either direction (brokkr's `reset_device`).
    pub fn purge(&self) {
        unsafe {
            PurgeComm(
                self.handle,
                PURGE_RXABORT | PURGE_RXCLEAR | PURGE_TXABORT | PURGE_TXCLEAR,
            );
        }
        log::info(&format!("purged the {} buffers", self.port_name));
    }

    pub fn write(&mut self, data: &[u8], timeout: Duration) -> Result<usize, String> {
        let start = Instant::now();
        let mut total = 0usize;
        while total < data.len() {
            let mut written: u32 = 0;
            let ok = unsafe {
                WriteFile(
                    self.handle,
                    data[total..].as_ptr(),
                    (data.len() - total) as u32,
                    &mut written,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(win32_error(&format!(
                    "WriteFile on {} after {total} byte(s) in {} ms",
                    self.port_name,
                    start.elapsed().as_millis()
                )));
            }
            if written == 0 {
                return Err(format!(
                    "WriteFile on {} wrote 0 bytes after {} ms",
                    self.port_name,
                    start.elapsed().as_millis()
                ));
            }
            total += written as usize;
            if start.elapsed() > timeout * 4 {
                return Err(format!(
                    "WriteFile on {} timed out after {total} of {} bytes",
                    self.port_name,
                    data.len()
                ));
            }
        }
        self.writes += 1;
        log::debug(&format!(
            "serial write {total} byte(s) to {} in {} ms",
            self.port_name,
            start.elapsed().as_millis()
        ));
        Ok(total)
    }

    pub fn read(&mut self, len: usize, timeout: Duration) -> Result<Vec<u8>, String> {
        let start = Instant::now();
        let mut buf = vec![0u8; len.max(1)];
        loop {
            let mut read: u32 = 0;
            let ok = unsafe {
                ReadFile(
                    self.handle,
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                    &mut read,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                let mut errors: u32 = 0;
                let mut stat: COMSTAT = unsafe { std::mem::zeroed() };
                unsafe { ClearCommError(self.handle, &mut errors, &mut stat) };
                return Err(format!(
                    "ReadFile on {} failed after {} ms (Win32 error {}, comm errors 0x{errors:08x}, {} byte(s) queued)",
                    self.port_name,
                    start.elapsed().as_millis(),
                    unsafe { GetLastError() },
                    stat.cbInQue
                ));
            }
            if read > 0 {
                buf.truncate(read as usize);
                self.reads += 1;
                log::debug(&format!(
                    "serial read {read} byte(s) from {} in {} ms",
                    self.port_name,
                    start.elapsed().as_millis()
                ));
                return Ok(buf);
            }
            self.empty_reads += 1;
            if start.elapsed() >= timeout {
                return Err(format!(
                    "ReadFile on {} returned 0 bytes after {} ms (timeout, {} empty read(s))",
                    self.port_name,
                    start.elapsed().as_millis(),
                    self.empty_reads
                ));
            }
        }
    }

    pub fn close(&mut self) {
        if self.handle != INVALID_HANDLE_VALUE && !self.handle.is_null() {
            unsafe { CloseHandle(self.handle) };
            self.handle = INVALID_HANDLE_VALUE;
        }
    }
}

impl Drop for SerialClient {
    fn drop(&mut self) {
        self.close();
    }
}

impl Wire for SerialClient {
    fn write_all(&mut self, data: &[u8], timeout: Duration) -> Result<(), String> {
        self.write(data, timeout).map(|_| ())
    }

    fn read(&mut self, len: usize, timeout: Duration) -> Result<Vec<u8>, String> {
        SerialClient::read(self, len, timeout)
    }

    fn clear_halt(&mut self) {
        self.purge();
    }

    fn label(&self) -> String {
        format!("COM port {}", self.port_name)
    }
}

/// `HKLM\HARDWARE\DEVICEMAP\SERIALCOMM` -> ["COM3", ...]
fn read_serialcomm_registry() -> Result<Vec<String>, String> {
    let out = std::process::Command::new("reg")
        .args(["query", r"HKLM\HARDWARE\DEVICEMAP\SERIALCOMM"])
        .output()
        .map_err(|e| format!("could not run reg.exe: {e}"))?;

    if !out.status.success() {
        return Err(format!(
            "reg.exe exited with {} - no serial device map (no COM ports)",
            out.status
        ));
    }

    let text = String::from_utf8_lossy(&out.stdout);
    let mut ports = Vec::new();
    for line in text.lines() {
        // Lines look like: "    \Device\Serial0    REG_SZ    COM3"
        if let Some(idx) = line.find("REG_SZ") {
            let name = line[idx + "REG_SZ".len()..].trim();
            if !name.is_empty() && name.to_ascii_uppercase().starts_with("COM") {
                ports.push(name.to_string());
            }
        }
    }
    Ok(ports)
}

/// Report COM ports and the PnP device behind each, so we can tell a Samsung
/// download-mode port apart from an unrelated serial device.
pub fn describe_ports() {
    match read_serialcomm_registry() {
        Ok(ports) if ports.is_empty() => {
            println!(
                "  no COM ports present - the Samsung device is still bound to WinUSB (Zadig),\n\
                 \x20 so the CDC serial stack never claims it"
            );
        }
        Ok(ports) => {
            println!("  COM ports present: {}", ports.join(", "));
            for p in ports {
                let desc = device_for_port(&p);
                println!("    {p}: {desc}");
            }
        }
        Err(e) => println!("  COM port enumeration failed: {e}"),
    }
}

fn device_for_port(port: &str) -> String {
    let script = format!(
        "(Get-CimInstance Win32_PnPEntity -Filter \"Name like '%{port}%'\" | \
         Select-Object -First 1 -ExpandProperty Name)"
    );
    std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "<unknown device>".to_string())
}
