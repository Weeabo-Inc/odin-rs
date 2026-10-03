<div align="center">
	<img width=140 src="assets/cover.svg" />
	<h2>odin-rs</h2>
</div>

[![License](https://img.shields.io/badge/license-MIT-blue.svg?style=flat-square)]()
[![Platform](https://img.shields.io/badge/platform-Windows%20x64-0078D6.svg?style=flat-square)]()
[![Language](https://img.shields.io/badge/language-Rust-orange.svg?style=flat-square)]()
[![Status](https://img.shields.io/badge/status-diagnostic%20instrument-purple.svg?style=flat-square)]()

### A protocol client for the phone that would not talk.

A Rust client for the Samsung Odin/Thor download protocol. Built to answer one question and one question only: **what is actually happening on the wire?**

This is a diagnostic instrument, not a flasher. It never writes. It reads, reports, and tells you precisely where the conversation fails.

---

### Why does this exist?

Every established tool refused to communicate with a Galaxy A13 in Download Mode:

| Tool | Result |
|---|---|
| `heimdall detect` | "detected", but it performs **zero I/O**. Only a VID/PID match. |
| `heimdall print-pit` | worked **once**, early, then never again |
| `heimdall` (anything else) | `Protocol initialisation failed!` |
| brokkr-flash | no handshake |

Rather than keep guessing, this implements the protocol directly and runs a matrix of controlled experiments against it.

**What it found:** the device is a standard CDC-ACM composite gadget. Windows had bound **WinUSB** to it through Zadig, but the maintained clients, and real Odin, talk to these devices over a **COM port** through `usbser.sys`. The firmware implements **no CDC class requests at all**, so the WinUSB path can never work.

---

### The protocol

| Element | Detail |
|---|---|
| Handshake | **BULK**, not a control transfer: send ASCII `"ODIN"`, expect `"LOKE"` |
| Request box | **1024 bytes**: `id@0x00`, `data@0x04`, `intData[9]@0x08`, `charData[128]@0x2C`, `md5[32]@0xAC` |
| Response | **8 bytes** (`id` + `ack`). `0xFFFFFFFF` means `BOOTLOADER_FAIL`. |
| Response read | with a **512-byte buffer**. Reading exactly 8 fails. |
| `BeginSession` | `0x64/0x00` with `intData[0] = 0x7FFFFFFF`, and it **must** be followed by `0x64/0x05` if version >= 2 |
| `END_SESSION` | `0x67/0x00` |
| Commands | `0x67/0x01` reboot, `0x67/0x02` reboot to Odin, `0x67/0x03` shutdown |
| Pre-handshake ASCII | `RESET` returns `"+RESET: OK\n"` and shuts the device down. Also `PROMPTsetenv REBOOT_MODE 7`, `ATQ0`, `FPGM`. |

---

### The experiment matrix

`src/experiments.rs` runs **10 read-only experiments** and reports each one. It exists to convert "it does not work" into a specific, falsifiable result.

| Experiment | Result | What it proves |
|---|---|---|
| Bulk OUT `"ODIN"` | `Ok(4)` in **0 ms**, every time | the endpoint is alive and accepting |
| Bulk IN expecting `"LOKE"` | **timeout**, not one byte, ever | the firmware never answers on this transport |
| `SET_CONTROL_LINE_STATE` | **STALL** | the firmware implements **no CDC class requests** |
| Interrupt endpoint `0x82` | `NotFound` | it never produces notifications |
| 1024-byte write | **1003 ms** | the endpoint is real, not stalled. 4 bytes took 0 ms. |

That last row is the useful one. A stalled endpoint would error or time out fast. **A 1003 ms write means the data went somewhere.** Combined with the STALL on class requests, it localises the fault precisely.

**A note on `heimdall detect`.** It performs zero I/O. It matches a VID/PID and reports success. So the phrase "heimdall detects my device", repeated across every forum thread on this topic, including by the author of this repository, several times, with mounting confidence, tells you approximately nothing. It is a heartbeat monitor that beeps unconditionally.

---

### Transports

Both are implemented, because determining *which* one works is the entire point:

- **libusb / WinUSB** (`src/client.rs`): descriptor tree walking, interface scoring, bulk transfers
- **Win32 serial / `usbser.sys`** (`src/serial.rs`): `CreateFile("\\.\COMx")`, `SetCommState` at 115200 8N1, `SetCommTimeouts`, `PurgeComm`. This is the path real Odin and brokkr-flash actually use.

```console
$ odin-probe --list-com            # enumerate COM ports
$ odin-probe --serial COM3         # full read-only session over serial
$ odin-probe --matrix              # run the 10 experiment diagnostic matrix
```

**Every session ends with `0x67/0x00` END_SESSION and deliberately does not reboot the device.** Nothing in this repository flashes, erases, writes a partition, or writes a PIT.

---

### The finding that matters most

**For these devices, the only variable that counts is which driver Windows binds.** It must be the inbox CDC driver, `usbser.sys`, producing a COM port. A WinUSB binding through Zadig removes the transport the firmware expects.

```powershell
# uninstall the WinUSB binding so usbser.sys can take over
pnputil /delete-driver oem45.inf /uninstall /force
pnputil /remove-device "USB\VID_04E8&PID_685D\<instance>"
pnputil /scan-devices
```

**Note:** deleting the driver *package* is not enough. The per-device driver-key override survives it. The device node itself has to be removed.

---

### What it can and can't do

**Can do:**
- Speak the Odin handshake over both libusb and Win32 serial
- Walk the descriptor tree and score interfaces
- Run a 10 experiment diagnostic matrix and report each result
- Read a PIT over the correct transport
- Tell you exactly which stage of the handshake fails

**Can't do:**
- Flash anything. There is no write path in this repository by design.
- Complete a full session on a locked bootloader. A locked bootloader rejects unsigned images regardless of transport, and `SECURE CHECK FAIL` is the bootloader refusing, not the tool failing.
- Fix a host driver problem for you. It can tell you the driver is wrong. Changing it needs admin rights and a UAC prompt.

---

### Project layout

```
odin-rs/
├── assets/
│   └── cover.svg
├── src/
│   ├── client.rs       libusb/WinUSB backend, descriptor walking
│   ├── serial.rs       Win32 VCOM backend
│   ├── protocol.rs     1024 byte request box, 8 byte response, PIT parsing
│   ├── session.rs      transport agnostic handshake
│   ├── experiments.rs  the 10 experiment diagnostic matrix
│   ├── log.rs
│   └── main.rs         CLI and orchestration
├── REPORT.md           full measurement report
└── Cargo.toml
```

```console
$ cargo build --release     # 0 errors, 0 warnings
```

---

### Credits

- [Rust](https://www.rust-lang.org/) and the [rusb](https://github.com/a1ien/rusb) bindings
- The brokkr-flash project for documenting the COM port approach
- [Twemoji](https://github.com/twitter/twemoji) for the cover art, CC BY 4.0

---

<div align="center">
	<br/>
	<i>it reads. it does not write. that is the whole point.</i>
	<br/>
	<sub>for hardware you own, and phones you are prepared to unplug.</sub>
</div>
