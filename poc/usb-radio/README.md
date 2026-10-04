# USB radio POC

Standalone ESP32-S3 proof of concept for the Metronic 477144 children's player.

The POC deliberately does **not** use IOBEWI. Its purpose is to discover the real
hardware/USB behaviour first, then later provide a stable reference implementation for
a separate IOBEWI porting exercise.

Target stream:

```text
http://icecast.radiofrance.fr/monpetitfranceinter-midfi.mp3
```

## Goal

Make the Metronic see an ESP32-S3 as a USB mass-storage device containing a readable
`RADIO.MP3`, then progressively replace the static/diagnostic payload with the live MP3
stream.

## Stages

### P0 — virtual FAT16 model

The pure `usb-radio-core` crate implements a deterministic read-only FAT16 disk:

- 512-byte sectors;
- 4 MiB virtual medium;
- one root file: `RADIO.MP3`;
- 2 MiB file extent;
- data supplied by a `FileSource` trait;
- host unit tests validate the BPB, FAT chain and root entry.

This layer has no ESP or USB dependency.

### P1 — ESP32-S3 USB MSC

The `usb-radio-firmware` crate exposes the virtual disk through the ESP32-S3 native
USB OTG peripheral using `esp-hal` + `embassy-usb`.

The MSC implementation is intentionally small and read-only. It supports the SCSI
commands needed by a normal removable-disk host and logs every READ(10) request:

```text
msc: READ10 lba=<...> blocks=<...>
```

This trace is the main deliverable of the first Metronic test. It tells us whether the
player reads sequentially, reads ahead, seeks, or rereads old sectors.

At this stage the contents of `RADIO.MP3` are diagnostic bytes, not playable audio.
The acceptance criterion is enumeration + FAT mount + file discovery.

### P2 — live HTTP MP3 source

P1 proved on real hardware that the Metronic enumerates the device, finds `RADIO.MP3`,
shows `MP3` / `F001` and starts its playback counter. P2 therefore goes directly to
the real Radio France MP3 stream instead of adding an intermediate static-MP3 stage.

The ESP32-S3:

- joins Wi-Fi and gets an IPv4 configuration through DHCP;
- opens the Radio France HTTP MP3 stream;
- prebuffers 64 KiB before enabling USB MSC;
- keeps a 96 KiB rolling window;
- limits the network producer to at most 80 KiB ahead of the highest file offset consumed
  by USB;
- maps the live bytes to the existing `RADIO.MP3` sectors;
- waits when the Metronic asks for a sector that has not arrived yet.

The P1 FAT16 geometry is intentionally unchanged for the first streaming test. The
virtual file remains 2 MiB; this is enough to validate audible streaming before changing
file-system geometry or long-run behaviour.

Acceptance:

- Wi-Fi connects;
- the HTTP stream returns status 200;
- the 64 KiB prebuffer fills;
- USB enumeration starts only after prebuffer;
- Metronic shows `MP3` / `F001`;
- the Radio France stream is audible.

### P3 — stream hardening

Only after P2 hardware PASS: extend virtual duration, handle long runs/reconnects,
characterize underruns/backward reads, and tune buffer/FAT geometry if measurements
justify it.

## Build

P2 Wi-Fi credentials are compile-time inputs. They are deliberately not committed:

```sh
export WIFI_SSID='your-ssid'
export WIFI_PASSWORD='your-password'
```

An empty password selects an open network.

From the repository root:

```sh
cd poc/usb-radio
cargo test -p usb-radio-core

WIFI_SSID="$WIFI_SSID" WIFI_PASSWORD="$WIFI_PASSWORD" \
cargo +esp build -p usb-radio-firmware --release \
  -Z build-std=core,alloc \
  --target xtensa-esp32s3-none-elf
```

CI builds with no credentials to validate compilation only; such an image intentionally
stops before Wi-Fi initialization at runtime.

Flash/monitor, assuming `espflash` is installed:

```sh
cargo +esp run -p usb-radio-firmware --release \
  -Z build-std=core,alloc \
  --target xtensa-esp32s3-none-elf
```

## Flash the POC

Board: ESP32-S3. Two different USB connectors are involved:

| Port | Pins | Role |
| --- | --- | --- |
| native USB OTG | D+ GPIO20, D- GPIO19 | the POC's USB mass-storage device: plug into the Metronic |
| USB-UART (CP210x/CH340 bridge, "UART" label) | UART0 | flashing and serial console: plug into the PC |

The firmware owns GPIO19/20 as USB OTG, so the native port does **not** show a serial
console; logs (`esp-println`, `uart` feature) come out on the USB-UART port only.

Artefacts are in `dist/` (see `dist/BUILD.txt` for provenance, `dist/SHA256SUMS`):
`usb-radio-poc-esp32s3.bin` is a single merged image (bootloader + partition table + app)
written at `0x0`; `usb-radio-poc-esp32s3.elf` is the matching ELF.

### Browser (ESP Web Tools)

```sh
cd poc/usb-radio/dist
python3 -m http.server 8080      # or: http-server . -p 8080
```

Open `http://localhost:8080` in Chrome/Edge (Web Serial; HTTPS or localhost required),
click **Flash the POC**, pick the USB-UART serial port. Tick "Erase" for a clean install.
After flashing, **Logs & Console** in the dialog is the monitor.

### Command line

```sh
poc/usb-radio/scripts/flash.sh            # espflash write-bin 0x0 + espflash monitor
poc/usb-radio/scripts/flash.sh --port /dev/ttyUSB0
```

The port is auto-detected by `espflash` unless `--port` is given. Monitor only:
`espflash monitor --chip esp32s3`.

### Rebuild the artefacts

```sh
poc/usb-radio/scripts/build-release.sh
```

Runs the core tests, the release build (real link), `espflash save-image --merge`, and
regenerates `dist/`. Run it after committing source changes: `SOURCE_DATE_EPOCH` is the
date of the last commit touching the POC sources (it feeds the build date of the
ESP app descriptor), so the output is byte-identical for a given source commit.

### Expected boot log

```text
usb-radio POC: P2 live HTTP MP3 -> USB MSC
usb-radio POC: DP=GPIO20 DM=GPIO19
stream: http://icecast.radiofrance.fr/monpetitfranceinter-midfi.mp3
```

When a host enumerates the device:

```text
msc: connected
msc: READ10 lba=... blocks=...
```

Bus lifecycle (shows how far a host's enumeration gets): `usb: enabled=true`,
`usb: bus reset`, `usb: addressed=N`, `usb: configured=true`, `usb: suspended=...`.
No `bus reset` = the host never drove the bus; reset/addressed without `configured=true`
= enumeration stops before SET_CONFIGURATION.

Other lines: `msc: bulk-only reset`, `msc: unsupported SCSI opcode=0x.. xfer=..`
(answered with CSW status 1 + sense ILLEGAL REQUEST), `msc: disconnected`.

## Gate P1-METRONIC (hardware, manual)

Precondition: POC flashed on the ESP32-S3.

1. Start the serial monitor on the USB-UART port.
2. Plug the **native OTG** port into the Metronic 477144.
3. Wait for detection; check whether the player shows a drive / a file.
4. Save the full serial log.

Record: USB enumeration OK or not; `msc: connected` present or not; unsupported SCSI
opcodes; the READ10 sequence (first LBA, transfer sizes, re-reads, backward/forward
jumps); behaviour on unplug (`msc: disconnected`).

Not PASS without real hardware.

## Wiring

ESP32-S3 native USB FS:

- D+ = GPIO20
- D- = GPIO19

Use the board's native USB/OTG connector or a connector wired to those pins. Do not use a
USB-UART bridge port and assume it is the native OTG peripheral.

## Important limitation

P2 deliberately keeps the original 2 MiB virtual file and uses a bounded rolling RAM
window. It validates the bridge, not infinite playback. Extending the virtual duration
and hardening reconnect/seek behaviour belongs to P3 after the live-audio hardware gate.
