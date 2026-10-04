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

### P2 — static real MP3

Replace the diagnostic file source with a known-good MP3 sample without changing the USB
or FAT layers.

Acceptance:

- Metronic lists the file;
- playback starts;
- LBA trace is captured from start through steady playback.

### P3 — live HTTP source

Add Wi-Fi + HTTP ingestion from the Radio France URL and a rolling MP3 window behind the
same virtual file-sector interface.

The USB/FAT side must remain unchanged. The LBA trace from P1/P2 determines how much
history the rolling buffer must retain.

## Build

From the repository root:

```sh
cd poc/usb-radio
cargo test -p usb-radio-core

cargo +esp build -p usb-radio-firmware --release \
  -Z build-std=core \
  --target xtensa-esp32s3-none-elf
```

Flash/monitor, assuming `espflash` is installed:

```sh
cargo +esp run -p usb-radio-firmware --release \
  -Z build-std=core \
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
usb-radio POC: P1 static virtual FAT16 MSC
usb-radio POC: DP=GPIO20 DM=GPIO19
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

This POC intentionally reports a fixed FAT16 file size. A live radio stream is infinite,
while FAT/MSC is random-access and finite. P3 therefore depends on the actual Metronic
access pattern observed in P1/P2; no streaming strategy is assumed in advance.
