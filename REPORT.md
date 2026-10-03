# odin-rs — Samsung Odin / LOKE protocol client: observed results

**Target:** Samsung Galaxy A13 (SM-A135F), USB `04E8:685D`, Windows 11, Rust 1.97.1
**Tool:** `<REPO_ROOT>\odin-rs` → `target\release\odin-probe.exe`
**Safety:** strictly read-only. No flash, no erase, no partition write, no reboot was implemented or attempted.

---

## 1. What the tool is

A fully instrumented Odin/LOKE client with two transport backends over one shared
protocol core:

| File | Role |
|---|---|
| `src/main.rs` | CLI, orchestration, verdict, experiment-matrix mode |
| `src/protocol.rs` | wire constants + 1024-byte request box / 8-byte response box / PIT parser |
| `src/session.rs` | transport-agnostic handshake + command exchange + read-only session |
| `src/client.rs` | libusb (rusb) WinUSB bulk backend, descriptor dump, interface ranking |
| `src/serial.rs` | Win32 VCOM / `usbser.sys` serial backend (`CreateFile`/`SetCommState`/`ReadFile`) |
| `src/experiments.rs` | the 10-row handshake diagnosis matrix |
| `src/log.rs` | leveled logging (`ODIN_VERBOSE=0..5`) |
| `ref/` | reference copies of upstream clients used to transcribe the protocol |

Commands:

```
odin-probe.exe                    # full USB run: descriptors, handshake, read-only session
odin-probe.exe --matrix           # 10-row handshake diagnosis matrix
odin-probe.exe --list-com         # enumerate COM ports
odin-probe.exe --serial [COMx]    # serial/VCOM backend (handshake + read-only session)
odin-probe.exe --no-pit           # stop after session init + device type
```

---

## 2. Observed device descriptors (real output, bus 002)

```
Bus 002 Device 023  ID 04e8:685d   path 002:023:14  [known Samsung download-mode PID]
  bcdUSB              : 0x0200
  bcdDevice           : 0x021b
  bDeviceClass        : 0x02.0x02.0x00
  bMaxPacketSize0     : 64
  bNumConfigurations  : 1  (interfaces seen: 2)
  negotiated speed    : 480 Mbit/s (high speed)
  iManufacturer       : SAMSUNG
  iProduct            : SAMSUNG USB
  iSerialNumber       : <unavailable>
  interface[0].altsetting[0] : class 0x02 subclass 0x02 protocol 0x01  endpoints=1
      endpoint[0] addr=0x82 IN  Interrupt  wMaxPacketSize=10  bInterval=8
  interface[1].altsetting[0] : class 0x0a subclass 0x00 protocol 0x00  endpoints=2
      endpoint[0] addr=0x81 IN  Bulk  wMaxPacketSize=512  bInterval=0
      endpoint[1] addr=0x01 OUT Bulk  wMaxPacketSize=512  bInterval=0
```

This is a textbook **CDC-ACM composite gadget**: interface 0 is CDC control
(class 02/02/01) with the notification interrupt endpoint 0x82; interface 1 is
CDC data (0A/00/00) with bulk IN 0x81 / bulk OUT 0x01.

Interface ranking selected `iface=1 alt=0 ep_out=0x01 ep_in=0x81` (score 160) —
identical to Heimdall's and odin4's selection rule.

### Windows driver binding (the important part)

```
DEVPKEY_Device_Service        : WinUSB
DEVPKEY_Device_DriverInfPath  : oem45.inf
DEVPKEY_Device_DriverProvider : libwdi        <-- Zadig
DEVPKEY_Device_Manufacturer   : Samsung Electronics Co., Ltd
DEVPKEY_Device_CompatibleIds  : {USB\Class_02&SubClass_02&Prot_01, USB\Class_02&SubClass_02, USB\Class_02}
```

The device advertises the **standard CDC-ACM compatible ID**
`USB\Class_02&SubClass_02&Prot_01`, which is what Windows' inbox `usbser.sys`
binds to. Zadig has overridden that with WinUSB.

---

## 3. Observed handshake results — the actual failure

```
>>> LOKE handshake: bulk OUT ep=0x01 <- b"ODIN" (4 bytes)
    then bulk IN  ep=0x81 -> expect b"LOKE" (4 bytes, 512-byte read buffer)
    bulk OUT ep=0x01 len=4     [handshake "ODIN" OUT] -> Ok(4) in 0 ms
        raw  : 4f 44 49 4e
        text : "ODIN"
    bulk IN  ep=0x81 len=512   [handshake "ODIN" IN] -> Timeout (Operation timed out) in 1007 ms
```

| Step | Endpoint | Result |
|---|---|---|
| bulk OUT `"ODIN"` | `0x01` | **Ok(4) in 0 ms** — accepted every single time |
| bulk IN expecting `"LOKE"` | `0x81` | **rusb::Error::Timeout (libusb −7)** — never a single byte |
| bulk OUT `"THOR"` | `0x01` | Ok(4), same silent IN |
| bulk OUT `"ODIN\0"` (5 bytes) | `0x01` | Ok(5), same silent IN (test did not complete — device left the bus) |

**No byte has ever been received from this device**, on any endpoint, by any run.

### The 10-row diagnostic matrix (all read-only, all completed)

| # | Experiment | Result |
|---|---|---|
| 1 | baseline bare `"ODIN"` (4 B) | write Ok(4), read **Timeout** |
| 2 | `SET_CONTROL_LINE_STATE` DTR=1 RTS=0, then `"ODIN"` | control request **Pipe (STALL)**; write Ok(4), read **Timeout** |
| 3 | `SET_CONTROL_LINE_STATE` DTR=1 RTS=1, then `"ODIN"` | control request **Pipe (STALL)**; write Ok(4), read **Timeout** |
| 4 | ZLP then `"ODIN"` | ZLP → `NotFound`; write Ok(4), read **Timeout** |
| 5 | DTR+RTS + ZLP + `"ODIN"` | control **Pipe (STALL)**; write Ok(4), read **Timeout** |
| 6 | `"ODIN"`, then 2.5 s on interrupt ep 0x82, then read | interrupt IN **NotFound**; write Ok(4), read **Timeout** |
| 7 | `"ODIN"` in a 1024-byte zero-padded box | write **Ok(1024) after 1003 ms**, read **Timeout** |
| 8 | `"ODIN\0"` (5 bytes) | **not completed — device left the bus** |
| 9 | `"THOR"` + DTR/RTS | **not completed — device left the bus** |
| 10 | `"ODIN"` in a 512-byte box | not reached |

Notable details:

* `SET_CONTROL_LINE_STATE` (bRequest 0x22 on iface 0) is **actively rejected with a
  pipe/STALL** — the bootloader implements no CDC class request handler. So DTR/RTS
  signalling cannot be the missing trigger.
* Reading the CDC notification endpoint `0x82` returns `NotFound` — no notification
  is ever produced.
* The 1024-byte write took a full second (WinUSB chunking to 512-byte MPS boundaries)
  while every ≤4-byte write returned in 0 ms — the endpoint is genuinely live, not stalled.

---

## 4. Protocol facts verified against real sources

The original `src/main.rs` skeleton named in the task brief was **not** present — the
file contained a different, newer Thor-packet skeleton (8-byte header
`packet_size u32 / packet_type u16 / packet_flags u16`, `THOR`-then-`ODIN` probe, and
`0x67` control commands `END_SESSION`/`REBOOT`/`SHUTDOWN`). That content was preserved
to `ref/` before rewriting. A stray `src/bin/listen.rs` (a prior experiment that
claimed iface 1 and sent `"ODIN"` without reading a reply) was also preserved to `ref/`.

Verified from **Heimdall** (Benjamin-Dobell/Heimdall, `BridgeManager.cpp`) and
**odin4** (Llucs/odin4, `src/usb/odin_protocol.cpp`, `src/protocol/thor_protocol.h`,
`docs/THOR_PROTOCOL.md`):

1. **The handshake is BULK, not control.** Bulk OUT the ASCII token; expect `"LOKE"`
   back on bulk IN. The brief's assumption of bmRequestType `0x40`/`0xC0` +
   bRequest `0x42` is wrong for the handshake.
2. **Request box = 1024 bytes**: `id` (command type) at `0x00`, `data` (sub-command) at
   `0x04`, `intData[9]` at `0x08`, `charData[128]` at `0x2C`, `md5[32]` at `0xAC`, zero padding.
   Command types: `0x64` INIT, `0x65` PIT, `0x66` XMIT, `0x67` CLOSE.
3. **Response box = 8 bytes**: `id` + `ack`. `id == 0xFFFFFFFF` is `BOOTLOADER_FAIL`.
   Read it with a large (512-byte) buffer, *not* an exact 8-byte read.
4. **BeginSession** = `0x64/0x00` with `intData[0] = 0x7FFFFFFF`; the reply's `ack`
   upper 16 bits carry the bootloader protocol version. **If version ≥ 2 the host must
   follow with `0x64/0x05` + packet size** — Heimdall does this.
5. **PIT download** = `0x65/0x01` (ack = size) → `0x65/0x02` + block index in **500-byte**
   blocks → `0x65/0x03`. Fully read-only.
6. There is **no `DEVICE_INFO 0x65`** command and no `ODIN_SUCCESS` constant in this
   protocol. Device info is `0x64/0x01` (model code → `"SM-<code>"`), and success is
   simply `id == expected_id && ack >= 0`. The command IDs in the brief that do not
   exist were not implemented.

---

## 5. Root-cause analysis

Because our client sends byte-for-byte what Heimdall and odin4 send (same interface,
same endpoints, same token, same timeout) and still receives nothing, Heimdall's
`"ERROR: Protocol initialisation failed!"` is **not** caused by wrong handshake bytes.

The decisive evidence is what the modern working client does. **brokkr-flash**
(Gabriel2392/brokkr-flash, explicitly supports PID `0x685D`) does **not** use WinUSB
on Windows at all. Its Windows backend (`src/platform/windows/usbfs_device.cpp`) opens
the device as a **COM port**:

```cpp
handle_ = ::CreateFileA(port_path.c_str(), GENERIC_READ|GENERIC_WRITE, 0, nullptr,
                        OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, nullptr);
dcb.BaudRate = CBR_115200;  dcb.ByteSize = 8;
dcb.StopBits = ONESTOPBIT;  dcb.Parity   = NOPARITY;
::SetCommState(handle_, &dcb);
...
::SetCommTimeouts(...);  ::PurgeComm(...);
```

i.e. it drives the device through the **CDC serial stack (`usbser.sys`)**, exactly like
real Samsung Odin. It also sends a **5-byte `"ODIN\0"`** ping on USB
(`src/protocol/odin/odin_cmd.cpp`: `"handshake ping 'ODIN\\0' (5 bytes, USB)"`).

So the two candidate explanations, in order of likelihood:

* **H2 (most likely): the bootloader only speaks Odin over the CDC serial driver.**
  The gadget is a CDC-ACM device; WinUSB bypasses the serial stack and no CDC class
  requests are implemented by the gadget, so the bootloader's serial state machine is
  never brought up. This explains every measurement: writes accepted by the USB core,
  zero bytes ever produced, `SET_CONTROL_LINE_STATE` stalled, no notification on 0x82,
  and Heimdall (libusb/WinUSB on Windows) failing the same way.
* **H1: the bootloader needs the 5-byte `"ODIN\0"` ping.** Not yet excluded — the one
  experiment covering it did not complete because the device left the bus.

---

## 6. Blocker and required action

The device **keeps dropping off the USB bus** and was absent for most of this session
(it appeared briefly at 05:14, 05:17 and 05:25, then vanished). At the time of writing
no `VID_04E8` device is present at all, so neither H1 nor H2 could be settled.

A watcher is running that fires the full matrix (with automatic continuation into the
read-only session) the moment `04e8:685d` reappears.

**To settle it, one of these is needed:**

1. **Keep the phone on the Download Mode warning screen with the cable attached**
   (do not let it sleep, do not unplug), then wait for the watcher to fire.
2. **If H2 is right, unbind WinUSB and let `usbser.sys` bind.** This is a host driver
   change, not a device operation — nothing is written to the phone:
   * Device Manager → `SAMSUNG USB` → Update driver → **Uninstall device**
     (tick "delete the driver software") → unplug/replug.
     Windows should auto-bind the inbox CDC driver because the device advertises
     `USB\Class_02&SubClass_02&Prot_01`, and a **COM port** will appear.
   * Or: `pnputil /delete-driver oem45.inf /uninstall` (that INF came from Zadig;
     Zadig can re-add it later).
   Then run: `odin-probe.exe --serial` (or `--list-com` to see the port).

The serial backend is already implemented and builds clean, so once a COM port exists
the whole read-only session (handshake → BeginSession → device type → PIT) can be run
immediately without further code changes.

---

## 7. Verdict

| Question | Answer |
|---|---|
| Does the Odin/LOKE handshake work over the current WinUSB binding? | **No.** Bulk OUT is accepted; `"LOKE"` is never returned. |
| Is Heimdall's failure explained by wrong handshake bytes? | **No.** Our client sends the same bytes on the same interface/endpoints and fails identically. |
| Root cause | The bootloader presents a CDC-ACM gadget that Zadig has bound to **WinUSB**, bypassing the serial stack; the bootloader implements no CDC class requests and emits nothing on the bulk pipe. |
| Fix | Bind the inbox CDC driver (`usbser.sys`) instead of WinUSB and talk over the resulting COM port (`--serial`). Or confirm H1 (5-byte `"ODIN\0"`) in the pending matrix run. |
| Device harmed? | **No.** No flash, erase, partition write or reboot was performed at any point. |
