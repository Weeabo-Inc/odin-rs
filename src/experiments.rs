//! Deterministic, read-only experiment matrix for the LOKE handshake.
//!
//! The plain handshake (bulk OUT `"ODIN"` -> bulk IN `"LOKE"`) writes
//! successfully but never produces a reply, so this module enumerates the
//! remaining plausible triggers, one row per hypothesis, and reports exactly
//! which one (if any) made the device answer.
//!
//! Every experiment is strictly read-only: no write, no erase, no reboot, and
//! nothing that can put the device into a different mode.
//!
//! Hypothesis space:
//!   * CDC-ACM `SET_CONTROL_LINE_STATE` (DTR/RTS) - VCOM drivers assert DTR,
//!     and some Samsung bootloaders gate their serial output on it. libusb
//!     never touches these lines, so this is the prime suspect.
//!   * A zero-length packet before the token (`ZLP`), as used by real Odin.
//!   * Interface 0 (CDC control) needing to be claimed first.
//!   * A wake-up / warm-up write, then a retry of the handshake.
//!   * A 1024-byte padded `"ODIN"` request box rather than a bare 4 bytes.
//!   * A 5-byte `"ODIN\0"` preamble.
//!   * Data already pending on the interrupt or bulk-IN endpoints.

use std::time::Duration;

use rusb::{Context, DeviceHandle, Direction, TransferType};

use crate::client::{self, Client, DeviceInfo, EndpointPair};
use crate::log as lg;
use crate::protocol::REQUEST_SIZE;

/// What the runner should do for one experiment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setup {
    /// Nothing beyond claiming the data interface.
    None,
    /// Send CDC SET_CONTROL_LINE_STATE (0x22) on the control interface.
    LineState { dtr: bool, rts: bool },
    /// Send a zero-length packet on the bulk OUT endpoint first.
    Zlp,
    /// Send the handshake token three times before reading, with short gaps.
    RepeatThree,
    /// Issue a USB port reset before the handshake.
    ResetDevice,
}

#[derive(Debug, Clone, Copy)]
pub struct Experiment {
    pub id: u32,
    pub name: &'static str,
    pub why: &'static str,
    pub setup: Setup,
    /// Payload sent as the handshake token.
    pub token: Token,
    /// Drain pending bytes from the interrupt IN endpoint first.
    pub read_int_in: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Token {
    Odin,
    Thor,
    OdinNul5,
    /// `"ODIN"` at offset 0 of a 1024-byte, zero-padded request box.
    OdinInBox,
    /// `"ODIN"` at offset 0 of a 512-byte, zero-padded box.
    OdinBox512,
}

impl Token {
    fn bytes(self) -> Vec<u8> {
        match self {
            Token::Odin => b"ODIN".to_vec(),
            Token::Thor => b"THOR".to_vec(),
            Token::OdinNul5 => vec![b'O', b'D', b'I', b'N', 0],
            Token::OdinInBox => {
                let mut v = vec![0u8; REQUEST_SIZE];
                v[..4].copy_from_slice(b"ODIN");
                v
            }
            Token::OdinBox512 => {
                let mut v = vec![0u8; 512];
                v[..4].copy_from_slice(b"ODIN");
                v
            }
        }
    }
    fn label(self) -> &'static str {
        match self {
            Token::Odin => "b\"ODIN\" (4 bytes)",
            Token::Thor => "b\"THOR\" (4 bytes)",
            Token::OdinNul5 => "b\"ODIN\\0\" (5 bytes)",
            Token::OdinInBox => "b\"ODIN\" at offset 0 of a 1024-byte zero-padded box",
            Token::OdinBox512 => "b\"ODIN\" at offset 0 of a 512-byte zero-padded box",
        }
    }
}

pub fn experiments() -> Vec<Experiment> {
    vec![
        Experiment {
            id: 1,
            name: "baseline: bare ODIN (4 bytes)",
            why: "control case - reproduces the failure of the plain handshake",
            setup: Setup::None,
            token: Token::Odin,
            read_int_in: false,
        },
        Experiment {
            id: 2,
            name: "b\"ODIN\\0\" (5 bytes) - brokkr USB handshake",
            why: "brokkr-flash sends a NUL-terminated 5-byte ping on USB; the 5th byte may be required",
            setup: Setup::None,
            token: Token::OdinNul5,
            read_int_in: false,
        },
        Experiment {
            id: 3,
            name: "b\"ODIN\\0\" then delayed read",
            why: "same 5-byte ping but allow the bootloader time to answer",
            setup: Setup::None,
            token: Token::OdinNul5,
            read_int_in: true,
        },
        Experiment {
            id: 4,
            name: "ODIN (4 bytes) repeated 3x, then read",
            why: "the bootloader may need more than one ping before it answers",
            setup: Setup::RepeatThree,
            token: Token::Odin,
            read_int_in: false,
        },
        Experiment {
            id: 5,
            name: "THOR (4 bytes)",
            why: "alternative token documented for newer Thor-generation bootloaders",
            setup: Setup::None,
            token: Token::Thor,
            read_int_in: false,
        },
        Experiment {
            id: 6,
            name: "USB device reset, then ODIN",
            why: "clears host-controller/WinUSB state; mirrors Heimdall PR #478",
            setup: Setup::ResetDevice,
            token: Token::Odin,
            read_int_in: false,
        },
        Experiment {
            id: 7,
            name: "USB reset, then b\"ODIN\\0\" (5 bytes)",
            why: "combination of the two most promising fixes",
            setup: Setup::ResetDevice,
            token: Token::OdinNul5,
            read_int_in: false,
        },
        Experiment {
            id: 8,
            name: "ZLP-terminated ODIN",
            why: "real Odin terminates USB bursts with a zero-length packet",
            setup: Setup::Zlp,
            token: Token::Odin,
            read_int_in: false,
        },
        Experiment {
            id: 9,
            name: "CDC SET_CONTROL_LINE_STATE DTR=1 RTS=1 then ODIN",
            why: "VCOM drivers assert DTR/RTS; a serial-mode bootloader may gate on it",
            setup: Setup::LineState { dtr: true, rts: true },
            token: Token::Odin,
            read_int_in: false,
        },
        Experiment {
            id: 10,
            name: "ODIN in a 512-byte zero-padded box",
            why: "control for a full-max-packet-size write",
            setup: Setup::None,
            token: Token::OdinBox512,
            read_int_in: false,
        },
    ]
}

#[derive(Debug)]
pub struct ExperimentOutcome {
    pub id: u32,
    pub name: &'static str,
    pub write_ok: bool,
    pub write_result: String,
    pub read_result: String,
    pub reply: Vec<u8>,
    pub answered: bool,
    pub int_pending: Vec<u8>,
    pub setup_result: String,
    /// The exact token byte sequence that was sent, needed to reuse a winner.
    pub token_bytes: Vec<u8>,
    pub token_label: &'static str,
    pub pair: EndpointPair,
}

impl ExperimentOutcome {
    pub fn verdict(&self) -> String {
        if self.answered {
            format!("*** DEVICE ANSWERED with {} (\"{}\") ***", lg::hex(&self.reply), lg::ascii(&self.reply))
        } else if !self.write_ok {
            format!("write failed: {}", self.write_result)
        } else {
            format!("no reply ({})", self.read_result)
        }
    }
}

/// Run the whole matrix against a freshly opened handle for each experiment so
/// no experiment can inherit state from the previous one.
pub fn run_matrix(
    dev: &rusb::Device<Context>,
    info: &DeviceInfo,
    pair: EndpointPair,
    timeout: Duration,
) -> Vec<ExperimentOutcome> {
    let mut out = Vec::new();

    // Interface 0 is usually the CDC control interface carrying the interrupt
    // endpoint that VCOM drivers read.
    let ctrl_ep_int_in = info
        .interfaces
        .iter()
        .flat_map(|i| i.altsettings.iter())
        .flat_map(|a| a.endpoints.iter())
        .find(|e| e.transfer_type == TransferType::Interrupt && e.direction == Direction::In)
        .map(|e| e.address);

    let ctrl_iface = info
        .interfaces
        .iter()
        .find(|i| {
            i.altsettings
                .iter()
                .any(|a| a.endpoints.iter().any(|e| e.transfer_type == TransferType::Interrupt))
        })
        .map(|i| i.number);

    println!("\n  control interface  : {:?}", ctrl_iface);
    println!("  interrupt IN ep    : {:?}", ctrl_ep_int_in.map(|a| format!("0x{a:02x}")));

    for ex in experiments() {
        println!("\n--------------- experiment {} : {} ---------------", ex.id, ex.name);
        println!("  hypothesis: {}", ex.why);
        println!("  setup     : {:?}", ex.setup);
        println!("  token     : {}", ex.token.label());

        let _guard = client::acquire_usb();

        let handle = match dev.open() {
            Ok(h) => h,
            Err(e) => {
                out.push(ExperimentOutcome {
                    id: ex.id,
                    name: ex.name,
                    write_ok: false,
                    write_result: format!("open failed: {e:?}"),
                    read_result: "-".into(),
                    reply: vec![],
                    answered: false,
                    int_pending: vec![],
                    setup_result: "-".into(),
                    token_bytes: ex.token.bytes(),
                    token_label: ex.token.label(),
                    pair,
                });
                continue;
            }
        };

        let setup_result = match apply_setup(&handle, ctrl_iface, pair.ep_out, &ex, timeout) {
            Ok(s) => s,
            Err(e) => format!("setup failed: {e}"),
        };
        if !setup_result.is_empty() {
            println!("  setup result: {setup_result}");
        }

        let mut int_pending = Vec::new();
        if ex.read_int_in {
            if let Some(ep) = ctrl_ep_int_in {
                let mut buf = [0u8; 64];
                match handle.read_interrupt(ep, &mut buf, Duration::from_millis(2500)) {
                    Ok(n) => {
                        int_pending = buf[..n].to_vec();
                        println!("  interrupt IN 0x{ep:02x} -> {} byte(s): {}", n, lg::hex(&int_pending));
                    }
                    Err(e) => println!("  interrupt IN 0x{ep:02x} -> {e:?} ({e})"),
                }
            }
        }

        drop(handle);

        let mut c = match Client::open(dev, pair) {
            Ok(c) => c,
            Err(e) => {
                out.push(ExperimentOutcome {
                    id: ex.id,
                    name: ex.name,
                    write_ok: false,
                    write_result: format!("claim failed: {e}"),
                    read_result: "-".into(),
                    reply: vec![],
                    answered: false,
                    int_pending,
                    setup_result,
                    token_bytes: ex.token.bytes(),
                    token_label: ex.token.label(),
                    pair,
                });
                continue;
            }
        };

        let token = ex.token.bytes();
        let w = c.write_all(&token, timeout);
        println!(
            "  bulk OUT ep=0x{:02x} {} -> {}",
            c.pair.ep_out,
            lg::hex(&token[..token.len().min(8)]),
            w.outcome_str()
        );

        let mut reply = Vec::new();
        let mut read_result = String::from("not attempted");
        if w.is_ok() {
            let (r, buf) = c.read_with_retry(512, timeout);
            read_result = r.outcome_str();
            println!("  bulk IN  ep=0x{:02x} -> {read_result}", c.pair.ep_in);
            if r.is_ok() {
                println!("    raw : {}", lg::hex(&buf));
                println!("    text: \"{}\"", lg::ascii(&buf));
            }
            reply = buf;
        }

        let answered = reply.len() >= 4 && &reply[..4] == b"LOKE";

        c.release();
        drop(c);
        drop(_guard);

        let oc = ExperimentOutcome {
            id: ex.id,
            name: ex.name,
            write_ok: w.is_ok(),
            write_result: w.outcome_str(),
            read_result,
            reply,
            answered,
            int_pending,
            setup_result,
            token_bytes: token.clone(),
            token_label: ex.token.label(),
            pair,
        };
        println!("  => {}", oc.verdict());

        // If the device dropped off the bus (a USB reset re-enumerates it, and
        // a failed session can too), stop rather than emitting a wall of
        // NoDevice errors.
        let dropped = !oc.write_ok && oc.write_result.contains("NoDevice");
        out.push(oc);

        if dropped {
            lg::warn("device disappeared mid-matrix - stopping the experiment run");
            break;
        }

        std::thread::sleep(Duration::from_millis(250));
    }

    out
}

fn apply_setup(
    handle: &DeviceHandle<Context>,
    ctrl_iface: Option<u8>,
    ep_out: u8,
    ex: &Experiment,
    timeout: Duration,
) -> Result<String, String> {
    let mut notes = Vec::new();
    let iface = ctrl_iface.unwrap_or(0);

    // Claim the control interface if it is ours to claim; ignore failure, it
    // is not required for the control-transfer request itself.
    let _ = handle.set_auto_detach_kernel_driver(true);
    let claimed = match handle.claim_interface(iface) {
        Ok(()) => {
            notes.push(format!("claimed control iface {iface}"));
            true
        }
        Err(e) => {
            notes.push(format!("could not claim control iface {iface}: {e:?} (continuing)"));
            false
        }
    };

    // CDC SET_CONTROL_LINE_STATE: bmRequestType 0x21 (class, host->device,
    // interface), bRequest 0x22, wValue = DTR(bit0) | RTS(bit1).
    if let Setup::LineState { dtr, rts } = ex.setup {
        let value = (if dtr { 1u16 } else { 0 }) | (if rts { 2u16 } else { 0 });
        match handle.write_control(0x21, 0x22, value, iface as u16, &[], timeout) {
            Ok(n) => notes.push(format!(
                "SET_CONTROL_LINE_STATE iface {iface} wValue=0x{value:02x} -> Ok({n})"
            )),
            Err(e) => notes.push(format!(
                "SET_CONTROL_LINE_STATE iface {iface} wValue=0x{value:02x} -> {e:?} ({e})"
            )),
        }
    }

    if claimed {
        let _ = handle.release_interface(iface);
    }

    if ex.setup == Setup::Zlp {
        // The bulk OUT endpoint belongs to the data interface, which is not
        // claimed yet, so issue the ZLP on a bare handle. WinUSB rejects
        // zero-length writes with a parameter error while still performing the
        // transfer, so an error here is not necessarily fatal - log it.
        match handle.write_bulk(ep_out, &[], timeout) {
            Ok(n) => notes.push(format!("raw ZLP on bulk OUT 0x{ep_out:02x} -> Ok({n})")),
            Err(e) => notes.push(format!("raw ZLP on bulk OUT 0x{ep_out:02x} -> {e:?} ({e})")),
        }
    }

    if ex.setup == Setup::RepeatThree {
        for i in 0..3 {
            match handle.write_bulk(ep_out, b"ODIN", timeout) {
                Ok(n) => notes.push(format!("pre-ping {} -> Ok({n})", i + 1)),
                Err(e) => {
                    notes.push(format!("pre-ping {} -> {e:?} ({e})", i + 1));
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    if ex.setup == Setup::ResetDevice {
        // Heimdall PR #478: reset the port so the host controller state is
        // clean before the handshake. This is a USB port reset, not a device
        // command: it does not reboot the phone or leave download mode, it
        // just re-initialises the link (the device re-enumerates with the same
        // VID/PID). Read-only in the sense that matters here.
        match handle.reset() {
            Ok(()) => notes.push("libusb_reset_device -> ok (device re-enumerated)".to_string()),
            Err(e) => notes.push(format!("libusb_reset_device -> {e:?} ({e})")),
        }
    }

    Ok(notes.join("; "))
}
