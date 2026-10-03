# odin-rs

**A Rust client for the Samsung Odin/Thor download protocol** — built to diagnose why every
existing tool failed to talk to a Galaxy A13 in Download Mode.

This is a **diagnostic instrument**, not a flasher. It was written to answer one question:
*what is actually happening on the wire?*

---

## Why it exists

Every established tool refused to communicate with the device:

| Tool | Result |
|---|---|
| `heimdall detect` | "detected" — but performs **zero I/O**, only a VID/PID match |
| `heimdall print-pit` | worked **once**, early, then never again |
| `heimdall` (any operation) | `Protocol initialisation failed!` |
| brokkr-flash | no handshake |

Rather than keep guessing, this implements the protocol directly and runs a matrix of
controlled experiments against it.

**What it found:** the device is a standard CDC-ACM composite gadget. Windows had bound
**WinUSB** to it (via Zadig), but the maintained clients — and real Odin — talk to these
devices over a **COM port** through `usbser.sys`. The firmware implements **no CDC class
requests at all**, so the WinUSB path can never work.

---

## The protocol

| Element | Detail |
|---|---|
| Handshake | **BULK** (not a control transfer): send ASCII `"ODIN"`, expect `"LOKE"` |
| Request box | **1024 bytes** — `id@0x00`, `data@0x04`, `intData[9]@0x08`, `charData[128]@0x2C`, `md5[32]@0xAC` |
| Response | **8 bytes** (`id` + `ack`). `0xFFFFFFFF` = `BOOTLOADER_FAIL` |
| Response read | with a **512-byte** buffer — reading exactly 8 fails |
| `BeginSession` | `0x64/0x00` with `intData[0] = 0x7FFFFFFF`, **must** be followed by `0x64/0x05` if version ≥ 2 |
| `END_SESSION` | `0x67/0x00` |
| Commands | `0x67/0x01` reboot · `0x67/0x02` reboot-to-Odin · `0x67/0x03` shutdown |
| Pre-handshake ASCII | `RESET` → `"+RESET: OK\n"` (shuts the device down), `PROMPTsetenv REBOOT_MODE 7`, `ATQ0`, `FPGM` |

---

## The experiment matrix

[`src/experiments.rs`](src/experiments.rs) runs **10 read-only experiments** and reports each
one. It exists to convert "it doesn't work" into a specific, falsifiable result.

Representative findings:

| Experiment | Result | What it proves |
|---|---|---|
| Bulk OUT `"ODIN"` | `Ok(4)` in **0 ms**, every time | the endpoint is alive and accepting |
| Bulk IN expecting `"LOKE"` | **timeout**, not one byte, ever | the firmware never answers on this transport |
| `SET_CONTROL_LINE_STATE` | **STALL** | firmware implements **no CDC class requests** |
| Interrupt endpoint `0x82` | `NotFound` | it never produces notifications |
| 1024-byte write | **1003 ms** | the endpoint is real, not stalled — 4 bytes took 0 ms |

That last row is the useful one: a stalled endpoint would time out fast or error. **A
1003 ms write means the data went somewhere.** Combined with the STALL on class requests,
it localises the fault precisely.

**A note on `heimdall detect`.** It performs **zero I/O**. It matches a VID/PID and reports
success. So the phrase "heimdall detects my device" — repeated across every forum thread on
this topic, including by the author of this repository, several times, with mounting
confidence — tells you approximately nothing. It is a heartbeat monitor that beeps
unconditionally.

---

## Transports

Both are implemented, because determining *which* one works is the entire point:

- **libusb / WinUSB** (`src/client.rs`) — descriptor tree walking, interface scoring,
  bulk transfers
- **Win32 serial / `usbser.sys`** (`src/serial.rs`) — `CreateFile("\\.\COMx")`,
  `SetCommState` at 115200 8N1, `SetCommTimeouts`, `PurgeComm` — the path real Odin and
  brokkr-flash actually use

```console
$ odin-probe --list-com            # enumerate COM ports
$ odin-probe --serial COM3         # full read-only session over serial
$ odin-probe --matrix              # run the 10-experiment diagnostic matrix
```

**Every session ends with `0x67/0x00` `END_SESSION` and deliberately does not reboot the
device.** Nothing in this repository flashes, erases, writes a partition, or writes a PIT.

---

## The finding that matters most

**`heimdall detect` does no I/O.** It matches a VID/PID and reports success. So "heimdall
detects my device" — a phrase repeated across every forum thread on this topic — tells you
essentially nothing about whether the protocol works.

**The real variable is which driver Windows binds.** For these devices that must be the
inbox CDC driver (`usbser.sys`), producing a COM port. A WinUSB binding via Zadig
*removes* the transport the firmware expects.

```powershell
# uninstall the WinUSB binding so usbser.sys can take over
pnputil /delete-driver oem45.inf /uninstall /force
pnputil /remove-device "USB\VID_04E8&PID_685D\<instance>"
pnputil /scan-devices
```

**Note:** deleting the driver *package* is not enough — the per-device driver-key override
survives. The device node itself must be removed.

---

## Repo contents

| Path | What |
|---|---|
| [`src/client.rs`](src/client.rs) | libusb/WinUSB backend, descriptor walking, interface scoring |
| [`src/serial.rs`](src/serial.rs) | Win32 VCOM backend |
| [`src/protocol.rs`](src/protocol.rs) | 1024-byte request box, 8-byte response, PIT parsing |
| [`src/session.rs`](src/session.rs) | transport-agnostic handshake + read-only session |
| [`src/experiments.rs`](src/experiments.rs) | the 10-experiment diagnostic matrix |
| [`REPORT.md`](REPORT.md) | full measurement report |
| [`ref/`](ref/) | reference implementations consulted (brokkr, Odin protocol notes) |

```console
$ cargo build --release     # 0 errors, 0 warnings
```

---

## Scope, honestly

**What this conclusively established:** the handshake is bulk, not control; the response is
8 bytes read with a 512-byte buffer; the firmware implements no CDC class requests; the
endpoint is electrically alive but the WinUSB transport can never carry this protocol.

**What it did not do:** complete a full Odin session, because the device was never reachable
on a working transport. That is a *host driver configuration* problem, not a protocol one.

**Also worth stating plainly:** a locked bootloader rejects unsigned images regardless of
transport. `SECURE CHECK FAIL` and `SW REV. CHECK FAIL` are the bootloader refusing, not the
tool failing. This repository cannot change that — only an unlocked bootloader can, and
unlocking requires a physical Volume-Up long-press in Download Mode.

---

## License

MIT
