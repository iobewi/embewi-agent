# USB radio POC

Standalone ESP32-S3 proof of concept for the Metronic 477144 children's player.

The original POC discovered the real hardware/USB behaviour and remains the functional
reference at golden commit `c118e0c`. This branch incrementally composes portable
IOBEWI services while preserving product settings and the tested USB protocol.

Target stream:

```text
http://icecast.radiofrance.fr/monpetitfranceinter-midfi.mp3
```

## Goal

Make the Metronic see an ESP32-S3 as a USB mass-storage device containing a readable
`RADIO.MP3`, then progressively replace the static/diagnostic payload with the live MP3
stream.

## Stages

The P1/P2/P3 hardware results below describe the historical golden POC. They remain
the functional reference and do not qualify the current IOBEWI recomposition.

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

### P2 — live HTTP MP3 source — PASS

P1 proved on real hardware that the Metronic enumerates the device, finds `RADIO.MP3`,
shows `MP3` / `F001` and starts its playback counter. P2 then proved the complete live
path on the real Metronic: Radio France audio is audible without perceptible lag.

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

### P3 — continuous stream and USB-session rebasing

P3 removes the measured ~131 s P2 limit without turning the ESP into a huge storage
device.

The HTTP stream now uses a monotonic absolute byte position and runs continuously. Each
USB MSC connection creates a new session:

```text
infinite HTTP stream
        |
        | rolling 96 KiB RAM window
        v
current live position
        |
        +-- retain about 64 KiB before "now"
        |
        v
USB session base = RADIO.MP3 offset 0
```

If the Metronic disconnects and re-enumerates, offset 0 is therefore mapped to a fresh
position near the current live stream instead of the expired bytes from the first boot.

The virtual FAT16 geometry is also expanded:

- sector: 512 bytes;
- cluster: 32 KiB (64 sectors);
- `RADIO.MP3`: 1 GiB virtual size;
- about 18 h 38 min at 128 kbit/s before the host reaches the logical EOF;
- FAT entries are still generated on demand; the 1 GiB file is not stored in RAM/flash.

The network producer remains bounded to 80 KiB ahead of the USB consumer while a session
is active. With no USB session, the live stream keeps moving and the ring retains only
the latest window.

MSC logging is aggregated (one progress line per 256 READ(10) commands) instead of one
blocking UART line per read. Embassy USB internal trace is disabled by default and can be
restored with the `usb-debug` Cargo feature.

P3 hardware acceptance:

- provisioned Wi-Fi reconnects normally;
- Radio France becomes audible as in P2;
- playback continues beyond the former ~2 min 10 s boundary;
- no `stream: ... full` condition exists;
- after USB unplug/replug or host re-enumeration, a new `stream: session start ...`
  appears and playback starts from the current stream window rather than expired offset 0;
- no repeated `stream data expired` appears in normal forward playback.

## Build

From the repository root:

```sh
cd poc/usb-radio
cargo test -p usb-radio-core

cargo +esp build -p usb-radio-firmware --release \
  -Z build-std=core,alloc \
  --target xtensa-esp32s3-none-elf
```

The firmware contains **no Wi-Fi credentials**: they are entered at runtime (see
[Wi-Fi provisioning](#wi-fi-provisioning-improv-serial)), so `dist/` can be versioned.

Flash/monitor, assuming `espflash` is installed:

```sh
cargo +esp run -p usb-radio-firmware --release \
  -Z build-std=core,alloc \
  --target xtensa-esp32s3-none-elf
```

## Wi-Fi provisioning (Improv Serial)

Wi-Fi is configured over the USB-UART port with [Improv Serial](https://www.improv-wifi.com/serial/),
the protocol ESP Web Tools speaks after flashing.

- `improv-serial` and IOBEWI's portable manager/core/configuration contracts compose with
  `iobewi-esp-wifi` for radio and stack mechanics. Product code retains UART provisioning,
  credential persistence and the no-op for unchanged credentials while online.
- Credentials are validated first (association + DHCP) and only then committed to two flash
  sectors of the default NVS partition (`0x9000`/`0xA000`, A/B with generation + CRC, see
  `core/src/config_store.rs`). A write interrupted by a power cut keeps the previous record.
- Reflashing the merged image rewrites that region: provision again after each flash.
- Flash writes stall interrupts for a few ms: provision with the OTG port **unplugged**.

Flow: flash with ESP Web Tools, choose **Connect to Wi-Fi** in its dialog (it lists the
networks seen by the board), enter the password. Boot log on success:

```text
wifi: no saved credentials; waiting for Improv provisioning
improv: provisioning ssid=...
wifi: associated ...
wifi: got IP ...
improv: provisioned, credentials saved
```

On later boots the saved network is connected automatically (`wifi: ready`), with
reconnection/backoff handled by `WifiManager`.

## Flash the POC

Board: ESP32-S3. Two different USB connectors are involved:

| Port | Pins | Role |
| --- | --- | --- |
| native USB OTG | D+ GPIO20, D- GPIO19 | the POC's USB mass-storage device: plug into the Metronic |
| USB-UART (CP210x/CH340 bridge, "UART" label) | UART0 | flashing and serial console: plug into the PC |

The firmware owns GPIO19/20 as USB OTG, so the native port does **not** show a serial
console; logs (`esp-println`, `uart` feature) come out on the USB-UART port only.

`dist/` holds the current (P2, Improv-provisioned) image; it contains no credentials. The
older P1-only image remains in Git history (commit `5277330`).

The flashable image is a single merged image (bootloader + partition table + app) written
at `0x0`; the matching ELF is emitted beside it.

### Browser (ESP Web Tools)

Serve `poc/usb-radio/dist/` (it has its own `index.html` + `manifest.json`). If your local
web flasher expects the image under `web/firmware/esp32s3-usb-radio/`, copy the BIN there;
that directory is ignored by Git.

### Command line

```sh
poc/usb-radio/scripts/flash.sh
poc/usb-radio/scripts/flash.sh --port /dev/ttyUSB0
```

The port is auto-detected by `espflash` unless `--port` is given. Monitor only:
`espflash monitor --chip esp32s3`.

### Rebuild the artefacts

```sh
poc/usb-radio/scripts/build-release.sh
```

Runs the core tests, the release build (real link), and `espflash save-image --merge`.
Regenerates `dist/`.

`SOURCE_DATE_EPOCH` is the date of the last commit touching the POC sources, excluding
both delivery directories. This also avoids the old false "uncommitted changes" report
caused solely by regenerating `dist/`.

### Expected boot log

```text
usb-radio POC: P3 continuous HTTP MP3 -> USB MSC
usb-radio POC: DP=GPIO20 DM=GPIO19
stream: http://icecast.radiofrance.fr/monpetitfranceinter-midfi.mp3
```

When a host enumerates the device:

```text
msc: connected
msc: READ10 lba=... blocks=...
```

Only with `--features usb-debug` (not in the delivered image): bus lifecycle
(`usb: enabled=true`, `usb: bus reset`, `usb: addressed=N`, `usb: configured=true`,
`usb: suspended=...`), `msc: cmd op=0x.. xfer=.. tag=..` (first 24 non-READ10 commands per
session), `msc: get max lun`, and embassy-usb control-request tracing. No `bus reset` = the
host never drove the bus; reset/addressed without `configured=true` = enumeration stops
before SET_CONFIGURATION.

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

P3 provides a long-lived view, not a mathematically infinite FAT file. One USB session
exposes 1 GiB (about 18 h 38 min at the measured ~128 kbit/s). A new USB session rebases
the file to the current live window. The rolling RAM window remains 96 KiB, so very large
backward seeks are intentionally unsupported; the measured Metronic access pattern is
forward-only.

## Framework extraction in progress

The reference remains golden `c118e0c`. The recomposed firmware uses portable
`iobewi-rolling-stream` for retention, backpressure and rebased consumer positions.
The product retains its 96 KiB capacity, 64 KiB prebuffer, 80 KiB maximum lead,
far-ahead zero policy, critical-section lock, logs and HTTP reconnect loop.

FAT16 metadata generation and the 512-byte read-only block contract now come from
`iobewi-fat16`, pinned to framework commit
`dc55d66c677b0e6d3ddade8bd844888af21a8b95`. `usb-radio-core` is the product wrapper:
it selects the golden 32 KiB clusters, 1 GiB file extent, 32 root entries, `RADIO.MP3`,
`RADIOUSB` label and volume serial. The wrapper also implements the portable
read-only block contract. The framework host tests compare all 261 golden metadata
sectors byte-for-byte with an independently generated fixture. Product host
tests retain the BPB, root directory, cluster chain and sector mapping expectations.

The MSC BOT/SCSI class now comes from `iobewi-usb-msc`, pinned to the same framework
commit as FAT16 so both use one block-trait identity. Its protocol remains the golden
read-only subset; it consumes the block contract implemented by the product FAT wrapper.
`firmware/src/msc.rs` retains only the golden SCSI inquiry identity and unavailable-data
policy: Pending retries every five milliseconds until five seconds, then sends zeros;
Expired sends zeros immediately. The class forwards USB session boundaries to the source.
USB configuration, platform driver, prebuffer exposure and product logs remain local.
MSC diagnostics use the existing logger with `iobewi_usb_msc=info`; `usb-debug` forwards
to the class feature as well as Embassy USB logging.

The framework class preserves the reference's incomplete BOT/SCSI behaviour, including
malformed-CBW handling and BULK-ONLY RESET acknowledgement. It does not claim complete
conformance. BG-USB-MSC and the complete USB Radio hardware replay remain unqualified
until exercised on the physical ESP32-S3 + Metronic. This extraction introduces neither
USB Audio nor decoding: MP3 bytes still pass directly to the player.

Hardware replay on ESP32-S3 + Metronic is required before qualification of this
recomposition, including audio beyond P2, unplug/replug and a correctly rebased session.

The versioned `dist/` delivery remains the golden `c118e0c` image and was not regenerated
during these extractions. For hardware replay, rebuild with `scripts/build-release.sh`
or use a CI firmware artifact built from this branch's actual HEAD; flashing the old
`dist/` image does not qualify the recomposition.

### Wi-Fi platform reuse

Radio initialization, scanning, association, DHCP and the Embassy runner now use
`iobewi-esp-wifi` at `e21885ac905f3ef31c3303076962aefd448f5d0d`.
The product wrapper preserves the golden unchanged-credentials no-op while online,
so restarting maintenance after an Improv request does not deliberately bounce audio.
The link observer publishes the initialized stack through a Signal; HTTP starts once
it is ready and keeps that stable stack through reconnections. The Signal and stack handles
remain on the same executor: Embassy stack handles are not Send and must not be moved
to another core. USB still waits for prebuffer.

This changes platform mechanics: initialization is lazy; scan selects strongest BSSID;
link loss is observed through DHCP configuration loss rather than a separate radio event;
reconfiguration waits for the old lease to disappear. Association and DHCP each have
20-second timeouts, but disconnect/configuration-down waits do not. Initialization failure
can consume the radio peripheral. The framework seeds the network stack from its clock.
These differences require hardware replay, including Improv during audio and Wi-Fi loss
and recovery. Credential flash storage remains the local golden adapter for now.
