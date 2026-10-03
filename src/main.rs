//! odin-probe - a heavily instrumented Samsung Odin / LOKE protocol client.
//!
//! READ-ONLY BY DESIGN. This tool never flashes, erases or writes a partition,
//! and it never reboots the device. The only commands it issues after a
//! successful handshake are:
//!
//!   * `0x64/0x00` RQT_INIT          - begin session / negotiate protocol version
//!   * `0x64/0x01` RQT_INIT_TARGET   - device type query (model code)
//!   * `0x65/0x01` RQT_PIT_GET       - read PIT size
//!   * `0x65/0x02` RQT_PIT_START     - read PIT block (raw data, read-only)
//!   * `0x65/0x03` RQT_PIT_COMPLETE  - end PIT dump
//!   * `0x67/0x00` RQT_CLOSE_END     - close the session cleanly
//!
//! Byte layouts are transcribed from odin4 (Llucs/odin4, GPLv3) and
//! cross-checked against Heimdall (Benjamin Dobell / Glass Echidna, GPLv3).

mod client;
mod experiments;
mod log;
mod protocol;
mod serial;
mod session;

use std::process::ExitCode;
use std::time::Duration;

use client::*;
use log as lg;
use protocol::*;
use rusb::{Context, DeviceHandle, UsbContext};

const TOTAL_STEPS: u32 = 6;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Options {
    control_probe: bool,
    pit: bool,
    try_all_interfaces: bool,
    timeout_ms: u64,
    verbose_libusb: bool,
    matrix: bool,
    /// `Some(None)` = auto-detect a Samsung COM port; `Some(Some(name))` = use it.
    serial: Option<Option<String>>,
    list_com: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            control_probe: false,
            pit: true,
            try_all_interfaces: true,
            timeout_ms: TIMEOUT_CONTROL_MS,
            verbose_libusb: true,
            matrix: false,
            serial: None,
            list_com: false,
        }
    }
}

fn parse_args() -> Options {
    let mut o = Options::default();
    let mut args = std::env::args().skip(1).peekable();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--control-probe" => o.control_probe = true,
            "--no-pit" => o.pit = false,
            "--one-interface" => o.try_all_interfaces = false,
            "--matrix" => o.matrix = true,
            "--no-libusb-log" => o.verbose_libusb = false,
            "--list-com" => o.list_com = true,
            "--serial" => {
                // Optional following COM name: `--serial COM5` or bare `--serial`.
                let next = args.peek().cloned();
                match next {
                    Some(n) if n.to_ascii_uppercase().starts_with("COM") => {
                        args.next();
                        o.serial = Some(Some(n));
                    }
                    _ => o.serial = Some(None),
                }
            }
            "--help" | "-h" => {
                println!(
                    "odin-probe [options]\n\
                     \n\
                     A read-only, fully instrumented Samsung Odin/LOKE protocol client.\n\
                     \n\
                     USB (WinUSB) options:\n\
                     \x20 --matrix            run the diagnostic experiment matrix for the handshake\n\
                     \x20 --one-interface     only try the best-ranked bulk endpoint pair\n\
                     \x20 --control-probe     also try the bRequest 0x42 control handshake\n\
                     \x20 --no-libusb-log     suppress libusb's own log output\n\
                     \n\
                     Serial (VCOM / usbser.sys) options:\n\
                     \x20 --list-com          list COM ports and exit\n\
                     \x20 --serial [COMx]     talk to the device over a COM port instead of WinUSB\n\
                     \n\
                     Common:\n\
                     \x20 --no-pit            stop after session init + device type\n\
                     \x20 --help              this text\n\
                     \n\
                     Environment:\n\
                     \x20 ODIN_VERBOSE=0..5   0=silent 1=error 2=warn 3=info (default) 4=debug 5=trace\n\
                     \x20 ODIN_TIMEOUT_MS=nnn control-command timeout in ms (default 1000)\n"
                );
                std::process::exit(0);
            }
            other => eprintln!("warning: ignoring unknown argument {other:?}"),
        }
    }
    if let Ok(t) = std::env::var("ODIN_TIMEOUT_MS") {
        if let Ok(v) = t.parse::<u64>() {
            o.timeout_ms = v.clamp(100, 60_000);
        }
    }
    o
}

// ---------------------------------------------------------------------------
// Serial (VCOM) mode
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn run_serial_mode(opts: &Options) -> ExitCode {
    lg::banner("odin-probe : serial / VCOM backend  [READ ONLY]");

    serial::describe_ports();
    if opts.list_com {
        return ExitCode::SUCCESS;
    }

    let timeout = Duration::from_millis(opts.timeout_ms.max(1000));
    let wanted: Option<String> = match opts.serial.clone().flatten() {
        Some(name) => Some(name),
        None => {
            let ports = serial::SerialClient::find_samsung_ports();
            if ports.is_empty() {
                lg::fail("no COM ports are present on this system");
                println!(
                    "\nVERDICT: the Samsung download-mode device is not exposed as a COM port.\n\
                     It is currently bound to WinUSB (Zadig). To use this backend, unbind\n\
                     WinUSB so the inbox CDC driver (usbser.sys) binds and a COM port appears:\n\
                     \n\
                     \x20 Device Manager -> SAMSUNG USB -> Update driver -> Uninstall device\n\
                     \x20 (tick \"delete the driver software\"), then unplug and replug.\n\
                     \n\
                     The device advertises CompatibleIds USB\\Class_02&SubClass_02&Prot_01,\n\
                     which is exactly the standard CDC-ACM compatible ID that usbser.sys binds to."
                );
                return ExitCode::from(20);
            }
            // Prefer the highest-numbered port, which is the most recently
            // added device.
            ports.last().cloned()
        }
    };

    let name = wanted.expect("serial port name resolved above");
    println!("\nusing serial port: {name}");

    let mut port = match serial::SerialClient::open(&name, timeout) {
        Ok(p) => p,
        Err(e) => {
            lg::fail(&e);
            return ExitCode::from(21);
        }
    };
    port.purge();

    let attempts = session::handshake_sweep(&mut port, timeout);
    let mut summary = Vec::new();
    for a in &attempts {
        if a.ok {
            lg::ok(&a.verdict());
        } else {
            lg::fail(&a.verdict());
        }
        summary.push(a.verdict());
    }

    let winner = attempts.iter().find(|a| a.ok);
    let Some(winner) = winner else {
        println!("\n{}", build_serial_verdict(&summary, None, None));
        return ExitCode::from(22);
    };

    // Re-establish a clean handshake with the exact winning token before
    // running the session, so the session starts from a known state.
    lg::info(&format!(
        "replaying the winning token b\"{}\" before the read-only session",
        winner.token_label
    ));
    let replay = session::handshake_with(&mut port, &winner.token_label, &winner.token_bytes, timeout);
    if !replay.ok {
        lg::fail(&format!("replay of the winning token failed: {}", replay.verdict()));
        println!("\n{}", build_serial_verdict(&summary, None, None));
        return ExitCode::from(23);
    }

    let rep = session::run_readonly_session(&mut port, timeout, opts.pit);
    println!("\n{}", build_serial_verdict(&summary, Some(&rep), Some(&winner.token_label)));
    ExitCode::SUCCESS
}

#[cfg(not(windows))]
fn run_serial_mode(_opts: &Options) -> ExitCode {
    lg::fail("the serial backend is Windows-only");
    ExitCode::from(24)
}

fn build_serial_verdict(
    attempts: &[String],
    report: Option<&session::SessionReport>,
    token: Option<&str>,
) -> String {
    let mut s = String::new();
    s.push_str("======================================================================\n");
    match report {
        None => {
            s.push_str("RESULT: SERIAL HANDSHAKE FAILED\n");
            s.push_str("======================================================================\n\n");
            for a in attempts {
                s.push_str(&format!("  * {a}\n"));
            }
            s.push_str(
                "\nThe device did not answer b\"LOKE\" over the COM port either. Capture the\n\
                 exchange with a serial/USB analyser while the real Odin or brokkr-flash talks\n\
                 to the device to see what a working host sends.\n",
            );
        }
        Some(r) => {
            s.push_str("RESULT: SERIAL HANDSHAKE SUCCEEDED\n");
            s.push_str("======================================================================\n\n");
            s.push_str(&format!("  handshake token      : b\"{}\"\n", token.unwrap_or("?")));
            s.push_str(&format!(
                "  begin session        : {}\n",
                match r.session_ack {
                    Some(a) => format!("ack 0x{a:08x}"),
                    None => "no valid response".to_string(),
                }
            ));
            s.push_str(&format!(
                "  bootloader protocol  : {}\n",
                match r.protocol_version {
                    Some(v) => format!(
                        "version {v}  (compressed download {})",
                        if r.compressed { "supported" } else { "no" }
                    ),
                    None => "unknown".to_string(),
                }
            ));
            s.push_str(&format!(
                "  device model         : {}\n",
                r.device_model.as_deref().unwrap_or("<no device-type response>")
            ));
            s.push_str(&format!(
                "  read-only PIT dump   : {}\n",
                if r.pit_ok {
                    format!("SUCCESS, {} partition entries read", r.pit_entries)
                } else {
                    "failed".to_string()
                }
            ));
            s.push_str(
                "\nThe VCOM / CDC-serial path works. This confirms that WinUSB (the Zadig\n\
                 binding) was the blocker: the bootloader only speaks the Odin protocol\n\
                 through the Windows serial stack, which is why Heimdall v1.4.0 (libusb /\n\
                 WinUSB) failed with \"Protocol initialisation failed!\" on this machine.\n\
                 \nThe device was NOT rebooted and no partition was written or erased.\n",
            );
        }
    }
    s.push_str("======================================================================");
    s
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct AttemptSummary {
    pair: EndpointPair,
    /// One verdict line per handshake attempt, for the final report.
    verdicts: Vec<String>,
    winner: Option<String>,
}
impl AttemptSummary {
    fn succeeded(&self) -> bool {
        self.winner.is_some()
    }
}

fn main() -> ExitCode {
    lg::init_from_env();
    let opts = parse_args();

    lg::banner("odin-probe : Samsung Odin / LOKE protocol client  [READ ONLY]");
    println!("verbosity        : {} (ODIN_VERBOSE=0..5)", lg::level());
    println!("control timeout  : {} ms", opts.timeout_ms);
    println!("PIT download     : {}", if opts.pit { "enabled (read-only)" } else { "disabled" });
    println!("write operations : NONE IMPLEMENTED (no flash / erase / pit write / reboot)");

    if opts.verbose_libusb {
        init_libusb_logging();
    }

    // ------------------------------------------------- serial (VCOM) backend
    // Taken when `--serial [COMx]` is given, before any libusb work, because
    // this path uses the CDC serial stack instead of WinUSB.
    if opts.serial.is_some() || opts.list_com {
        return run_serial_mode(&opts);
    }

    let ctx = match Context::new() {
        Ok(c) => c,
        Err(e) => {
            lg::fail(&format!("libusb Context::new() failed: {e:?} ({e})"));
            return ExitCode::from(1);
        }
    };

    // ------------------------------------------------------------- step 1
    lg::step(1, TOTAL_STEPS, "Enumerate USB devices");
    let devices = match find_odin_devices(&ctx) {
        Ok(d) => d,
        Err(e) => {
            lg::fail(&format!("libusb_get_device_list failed: {e:?} ({e})"));
            return ExitCode::from(2);
        }
    };

    if devices.is_empty() {
        lg::fail(&format!(
            "no Samsung download-mode device found (VID {:04x}, known PIDs {:04x?}, or CDC-Data interface)",
            SAMSUNG_VID, KNOWN_DOWNLOAD_PIDS
        ));
        println!("\nVERDICT: nothing to talk to. Re-enter Download Mode on the phone and retry.");
        return ExitCode::from(3);
    }

    for (dev, reason) in &devices {
        let its = dev
            .device_descriptor()
            .map(|d| format!("{:04x}:{:04x}", d.vendor_id(), d.product_id()))
            .unwrap_or_else(|_| "????:????".into());
        lg::ok(&format!("candidate {its} on bus {:03} addr {:03} - {reason}",
            dev.bus_number(), dev.address()));
    }

    let (dev, _reason) = &devices[0];

    // ------------------------------------------------------------- step 2
    lg::step(2, TOTAL_STEPS, "Descriptor tree");
    let string_handle: Option<DeviceHandle<Context>> = dev.open().ok();
    let info = match describe(dev, string_handle.as_ref()) {
        Ok(i) => i,
        Err(e) => {
            lg::fail(&format!("descriptor read failed: {e:?} ({e})"));
            return ExitCode::from(4);
        }
    };
    dump_tree(&info);
    drop(string_handle);

    // ------------------------------------------------------------- step 3
    lg::step(3, TOTAL_STEPS, "Interface / bulk endpoint selection");
    let pairs = rank_endpoint_pairs(&info);
    if pairs.is_empty() {
        lg::fail("no interface exposes both a bulk IN and a bulk OUT endpoint");
        println!("\nVERDICT: unusable descriptors - the WinUSB binding or driver is wrong.");
        return ExitCode::from(5);
    }

    for (i, p) in pairs.iter().enumerate() {
        println!(
            "  rank[{i}] score={:<4} iface={} alt={} class=0x{:02x} endpoints={} bulk={} ep_out=0x{:02x}(mps {}) ep_in=0x{:02x}(mps {}){}",
            p.score,
            p.interface,
            p.altsetting,
            p.interface_class,
            p.num_endpoints,
            p.bulk_endpoints,
            p.ep_out,
            p.ep_out_max_packet,
            p.ep_in,
            p.ep_in_max_packet,
            if p.heimdall_match() { "  <== Heimdall/odin4 primary target" } else { "" }
        );
    }

    let primary = pairs[0];
    lg::ok(&format!(
        "primary selection: iface={} alt={} ep_out=0x{:02x} ep_in=0x{:02x} (score {})",
        primary.interface, primary.altsetting, primary.ep_out, primary.ep_in, primary.score
    ));

    // ---------------------------------------------------- optional matrix mode
    if opts.matrix {
        lg::step(4, TOTAL_STEPS, "Diagnostic experiment matrix (read-only)");
        let timeout = Duration::from_millis(opts.timeout_ms.max(1000));
        let outcomes = experiments::run_matrix(dev, &info, primary, timeout);

        lg::step(6, TOTAL_STEPS, "MATRIX VERDICT");
        let answered: Vec<_> = outcomes.iter().filter(|o| o.answered).collect();
        println!("  experiments run     : {}", outcomes.len());
        println!("  experiments answered: {}", answered.len());
        println!("\n  {:<4} {:<44} outcome", "id", "experiment");
        println!("  {}", "-".repeat(110));
        for o in &outcomes {
            println!("  {:<4} {:<44} {}", o.id, o.name, o.verdict());
        }

        println!("\n  per-experiment detail:");
        for o in &outcomes {
            println!("    [{}] {}", o.id, o.name);
            println!("        token      : {}", o.token_label);
            println!("        setup      : {}", if o.setup_result.is_empty() { "(none)" } else { &o.setup_result });
            println!("        bulk write : {}", o.write_result);
            println!("        bulk read  : {}", o.read_result);
            if !o.reply.is_empty() {
                println!("        reply      : {} (\"{}\")", lg::hex(&o.reply), lg::ascii(&o.reply));
            }
            if !o.int_pending.is_empty() {
                println!("        intr IN    : {} (\"{}\")", lg::hex(&o.int_pending), lg::ascii(&o.int_pending));
            }
        }

        if answered.is_empty() {
            println!(
                "\nRESULT: no workaround produced a LOKE reply. In every experiment the bulk OUT\n\
                 transfer was accepted by WinUSB but the device never sent anything back.\n\
                 That points at the device/driver binding rather than at the handshake bytes:\n\
                 a WinUSB-bound download-mode gadget that answers no bulk IN at all, on an\n\
                 interface whose control endpoint (0x82, CDC interrupt) suggests the\n\
                 bootloader expects the Samsung VCOM driver's serial signalling.\n\
                 Next step: unbind WinUSB (Zadig) and let the Samsung/VCOM driver bind, then\n\
                 retry with the real Odin or Heimdall, or capture the VCOM handshake."
            );
            return ExitCode::from(11);
        }

        println!("\nRESULT: a working trigger was found - see the rows marked *** DEVICE ANSWERED ***.");

        // A winner means we can immediately continue into the read-only session
        // on this same matrix run, which is valuable because the device is not
        // reliably staying on the bus.
        let winner = answered[0];
        println!(
            "\n>>> winner: experiment {} ({}), token {} - continuing into the read-only session",
            winner.id, winner.name, winner.token_label
        );
        return run_session_on_pair(dev, winner.pair, &winner.token_bytes, winner.token_label, &opts);
    }

    // ------------------------------------------------------------- step 4
    lg::step(4, TOTAL_STEPS, "Claim interface + LOKE handshake");
    let max_attempts = if opts.try_all_interfaces { pairs.len() } else { 1 };
    let timeout = Duration::from_millis(opts.timeout_ms);
    let mut summaries: Vec<AttemptSummary> = Vec::new();
    let mut live: Option<(EndpointPair, Client, Option<String>)> = None;

    for (idx, pair) in pairs.iter().take(max_attempts).enumerate() {
        println!(
            "\n================ handshake attempt {}/{} : iface={} alt={} ep_out=0x{:02x} ep_in=0x{:02x} ================",
            idx + 1,
            max_attempts,
            pair.interface,
            pair.altsetting,
            pair.ep_out,
            pair.ep_in
        );

        let _guard = acquire_usb();

        let mut c = match Client::open(dev, *pair) {
            Ok(c) => c,
            Err(e) => {
                lg::fail(&format!("cannot use this interface: {e}"));
                summaries.push(AttemptSummary {
                    pair: *pair,
                    verdicts: vec!["interface could not be claimed".to_string()],
                    winner: None,
                });
                continue;
            }
        };
        lg::ok(&format!(
            "claimed iface {} and set alt setting {}",
            pair.interface, pair.altsetting
        ));

        // Sweep the documented token variants in priority order, through the
        // shared session logic so the bytes match the serial backend exactly.
        // The 5-byte NUL-terminated `"ODIN\0"` is what brokkr-flash (the
        // current maintained client) sends on USB; Heimdall and odin4 send 4.
        let attempts = {
            let mut wire = UsbWire::new(&mut c);
            session::handshake_sweep(&mut wire, timeout)
        };
        for a in &attempts {
            if a.ok {
                lg::ok(&a.verdict());
            } else {
                lg::fail(&a.verdict());
            }
        }

        let winner = attempts.iter().find(|a| a.ok).map(|a| a.token_label.clone());
        let verdicts: Vec<String> = attempts
            .iter()
            .map(|a| format!("[{}] {}", a.token_label, a.verdict()))
            .collect();

        if opts.control_probe && winner.is_none() {
            c.control_handshake_probe(timeout);
        }

        let summary = AttemptSummary {
            pair: *pair,
            verdicts,
            winner: winner.clone(),
        };

        if winner.is_some() {
            lg::ok(&format!(
                "handshake established on iface {} with token b\"{}\"",
                pair.interface,
                winner.clone().unwrap()
            ));
            // Keep this handle: odin4 keeps the same session for every
            // subsequent command, and re-open + re-handshake is not reliable.
            live = Some((*pair, c, winner.clone()));
            summaries.push(summary);
            break;
        }

        summaries.push(summary);
        c.release();
        drop(c);
        drop(_guard);
        std::thread::sleep(Duration::from_millis(300));
    }

    // ------------------------------------------------------------- step 5
    lg::step(5, TOTAL_STEPS, "Read-only session: protocol version, device type, PIT");

    let session_ack;
    let proto_version;
    let compressed;
    let device_model;
    let pit_ok;
    let pit_entries;
    let winning_token;

    match live.take() {
        None => {
            lg::fail("the LOKE handshake never succeeded - no session commands were sent");
            session_ack = None;
            proto_version = None;
            compressed = false;
            device_model = None;
            pit_ok = false;
            pit_entries = 0;
            winning_token = None;
        }
        Some((pair, mut c, token)) => {
            winning_token = token.clone();
            lg::ok(&format!(
                "continuing on the live handle: iface={} ep_out=0x{:02x} ep_in=0x{:02x} (token b\"{}\")",
                pair.interface,
                pair.ep_out,
                pair.ep_in,
                token.clone().unwrap_or_default()
            ));

            // All session commands go through the shared, transport-agnostic
            // logic so the USB and serial backends send identical bytes.
            let rep = {
                let mut wire = UsbWire::new(&mut c);
                session::run_readonly_session(&mut wire, timeout, opts.pit)
            };
            c.release();

            session_ack = rep.session_ack;
            proto_version = rep.protocol_version;
            compressed = rep.compressed;
            device_model = rep.device_model;
            pit_ok = rep.pit_ok;
            pit_entries = rep.pit_entries;
        }
    }
    let _ = &winning_token;

    // ------------------------------------------------------------- step 6
    lg::step(6, TOTAL_STEPS, "VERDICT");
    println!(
        "{}",
        build_verdict(
            &summaries,
            session_ack,
            proto_version,
            compressed,
            device_model.as_deref(),
            pit_ok,
            pit_entries,
        )
    );

    let handshake_ok = summaries.iter().any(|s| s.succeeded());
    if handshake_ok {
        println!("\nThe device was NOT rebooted and no partition was written or erased.");
    }

    if handshake_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(10)
    }
}

/// Re-open one interface, replay a known-good handshake token, then run the
/// read-only session. Used by `--matrix` when an experiment finds a working
/// trigger.
fn run_session_on_pair(
    dev: &rusb::Device<Context>,
    pair: EndpointPair,
    token: &[u8],
    token_label: &str,
    opts: &Options,
) -> ExitCode {
    lg::step(5, TOTAL_STEPS, "Read-only session on the winning trigger");

    let timeout = Duration::from_millis(opts.timeout_ms.max(1000));
    let _guard = acquire_usb();

    let mut c = match Client::open(dev, pair) {
        Ok(c) => c,
        Err(e) => {
            lg::fail(&format!("could not re-open the winning interface: {e}"));
            return ExitCode::from(12);
        }
    };

    let replay = {
        let mut wire = UsbWire::new(&mut c);
        session::handshake_with(&mut wire, token_label, token, timeout)
    };
    if !replay.ok {
        lg::fail(&format!(
            "replay of the winning token failed: {}",
            replay.verdict()
        ));
        c.release();
        return ExitCode::from(13);
    }
    lg::ok("handshake replayed successfully; starting the read-only session");

    let rep = {
        let mut wire = UsbWire::new(&mut c);
        session::run_readonly_session(&mut wire, timeout, opts.pit)
    };
    c.release();

    lg::step(6, TOTAL_STEPS, "VERDICT");
    let summary = AttemptSummary {
        pair,
        verdicts: vec![format!("[{}] {}", replay.token_label, replay.verdict())],
        winner: Some(replay.token_label.clone()),
    };
    println!(
        "{}",
        build_verdict(
            std::slice::from_ref(&summary),
            rep.session_ack,
            rep.protocol_version,
            rep.compressed,
            rep.device_model.as_deref(),
            rep.pit_ok,
            rep.pit_entries,
        )
    );
    println!("\nThe device was NOT rebooted and no partition was written or erased.");
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------------------
// Read-only PIT download
// ---------------------------------------------------------------------------

fn build_verdict(
    summaries: &[AttemptSummary],
    session_ack: Option<u32>,
    proto_version: Option<u16>,
    compressed: bool,
    device_model: Option<&str>,
    pit_ok: bool,
    pit_entries: usize,
) -> String {
    let mut s = String::new();
    s.push_str("======================================================================\n");

    let winner = summaries.iter().find(|s| s.succeeded());

    match winner {
        None => {
            s.push_str("RESULT: ODIN HANDSHAKE FAILED - the device never replied b\"LOKE\"\n");
            s.push_str("======================================================================\n\n");
            for sum in summaries {
                s.push_str(&format!(
                    "interface {} (ep_out 0x{:02x} / ep_in 0x{:02x}, score {}):\n",
                    sum.pair.interface, sum.pair.ep_out, sum.pair.ep_in, sum.pair.score
                ));
                if sum.verdicts.is_empty() {
                    s.push_str("  * interface could not be claimed\n");
                }
                for v in &sum.verdicts {
                    s.push_str(&format!("  * {v}\n"));
                }
            }
            s.push_str(
                "\nHow to read this:\n\
                 \x20 * \"STEP 1 FAILED ... Timeout\" on every interface means no bulk OUT\n\
                 \x20   transfer ever completed. Either WinUSB is not actually bound to the\n\
                 \x20   download-mode interface, or the phone is not in Download Mode.\n\
                 \x20 * \"STEP 1 FAILED ... Pipe (EPIPE/STALL)\" means the endpoint exists but is\n\
                 \x20   halted - the bootloader is not waiting for this handshake.\n\
                 \x20 * \"STEP 2 FAILED ... Timeout\" means the handshake write was accepted but\n\
                 \x20   no reply came back: the pipe is live, the LOKE protocol is not.\n\
                 \x20 * A non-\"LOKE\" reply is printed verbatim above - that raw answer is the\n\
                 \x20   single most useful piece of evidence.\n\
                 \nRECOVERY: unplug and replug the cable while holding Vol-Down + Vol-Up,\n\
                 then press Vol-Up at the Download Mode warning screen, and re-run this tool.\n",
            );
        }
        Some(sum) => {
            s.push_str("RESULT: ODIN HANDSHAKE SUCCEEDED\n");
            s.push_str("======================================================================\n\n");
            s.push_str(&format!(
                "  handshake token      : b\"{}\"\n  interface            : {} (ep_out 0x{:02x} / ep_in 0x{:02x}, score {})\n",
                sum.winner.clone().unwrap_or_default(),
                sum.pair.interface,
                sum.pair.ep_out,
                sum.pair.ep_in,
                sum.pair.score
            ));
            s.push_str(&format!(
                "  begin session        : {}\n",
                match session_ack {
                    Some(a) => format!("ack 0x{a:08x}"),
                    None => "no valid response".to_string(),
                }
            ));
            s.push_str(&format!(
                "  bootloader protocol  : {}\n",
                match proto_version {
                    Some(v) => format!("version {v}  (compressed download {})", if compressed { "supported" } else { "no" }),
                    None => "unknown".to_string(),
                }
            ));
            s.push_str(&format!(
                "  device model         : {}\n",
                device_model.unwrap_or("<no device-type response>")
            ));
            s.push_str(&format!(
                "  read-only PIT dump   : {}\n",
                if pit_ok {
                    format!("SUCCESS, {pit_entries} partition entries read")
                } else {
                    "failed".to_string()
                }
            ));

            s.push_str("\nTHE ODIN HANDSHAKE WORKS. The device speaks LOKE on the bulk pipes\n");
            s.push_str("of the interface listed above.\n");

            if pit_ok {
                s.push_str(
                    "\nFull read-only access is confirmed: session init, device type and the\n\
                     PIT were all downloaded successfully. Heimdall v1.4.0's\n\
                     \"Protocol initialisation failed!\" is therefore NOT caused by the\n\
                     handshake bytes or the interface choice - see the per-attempt log\n\
                     above for what our client did differently.\n",
                );
            } else if session_ack.is_some() {
                s.push_str(
                    "\nThe handshake and session init worked but the PIT dump did not\n\
                     complete; the detailed per-command log above names the exact command\n\
                     and response that failed.\n",
                );
            }
        }
    }

    s.push_str("======================================================================");
    s
}

fn init_libusb_logging() {
    let mut ctx = rusb::GlobalContext::default();
    let lvl = match lg::level() {
        0 => rusb::LogLevel::None,
        1 => rusb::LogLevel::Error,
        2 => rusb::LogLevel::Warning,
        3 => rusb::LogLevel::Info,
        _ => rusb::LogLevel::Debug,
    };
    ctx.set_log_level(lvl);
}
