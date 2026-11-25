# spotify-tap

An in-process audio tap for Spotify on Windows.

`spotify-tap` builds a small `cdylib` that is loaded into Spotify's main process. It hooks the process-wide audio render path, copies the rendered float32 stereo PCM into a shared-memory ring, and zeroes Spotify's own render buffer so nothing is heard from the default output device. Any separate process can map the ring and use the audio — for visualizers, recording, streaming, custom routing, or further DSP.

The tap is self-contained and host-agnostic: it exports a single `StartTap()` function and a documented shared-memory ABI. Nothing in this repository depends on a particular consumer.

> Not affiliated with or endorsed by Spotify. Use at your own risk.

## How it works

1. `StartTap()` creates a per-process mutex and a per-process file mapping, then opens the default render endpoint (`eRender` / `eConsole`) in shared mode to learn the mix format.
2. It resolves the `IAudioRenderClient` vtable and overwrites slots 3 and 4 (`GetBuffer`, `ReleaseBuffer`) with hooks. The original entries are kept and always called, so Spotify's own audio pipeline continues to run.
3. `GetBuffer` records the buffer pointer and the requested frame count for that `this` instance.
4. `ReleaseBuffer` looks the buffer up, verifies float32 stereo (2 channels, 8-byte block align), and — only while a consumer heartbeat is alive — copies the rendered frames into the shared ring and zeroes the source buffer before delegating to the original method.
5. A consumer must publish a heartbeat roughly once per second. If it stops, crashes, or never attaches, the hooks pass audio through untouched, so Spotify is never left permanently silent. A watchdog restores the original vtable after a stale heartbeat, so the hook does not remain installed when the consumer exits.

The hook only touches the process's default render endpoint in shared mode, and only supports float32 stereo output. Unsupported formats stop setup with an error code instead of corrupting audio.

## Requirements

- Windows x64
- Rust 1.88+ with the MSVC toolchain; `.cargo/config.toml` statically links the CRT for the `x86_64-pc-windows-msvc` target
- Node.js 18+ only if you use the helper build script
- A consumer process that understands the shared-memory layout below

## Build

Plain Cargo build:

```powershell
cargo build --release
# -> target\release\spotify_tap.dll
```

`/target/` is gitignored.

Optional helper script:

```powershell
node scripts/build-native.mjs
```

It runs `cargo build --release --locked`, parses Cargo's JSON messages to locate the produced `.dll`, and copies it to `dist\spotify_tap.dll`. It deletes any existing copy first, because a process that has already loaded the DLL keeps the file locked; loaders should inject a private copy (for example from `%LOCALAPPDATA%`) so the build output stays replaceable. The script is optional — plain Cargo and a manual copy work just as well.

## Using the tap

### 1. Load the DLL

Load or inject `spotify_tap.dll` into the Spotify main process with any standard mechanism (`LoadLibrary`, a DLL injector, an existing loader). The tap does not install itself and has no dependencies beyond the Windows system libraries.

### 2. Start it

The exported entry point is:

```c
int32_t StartTap(void); // extern "system", idempotent
```

Call it once after the DLL is loaded. A second call returns `0` without doing anything. Setup takes a process-wide mutex named `Local\SpotifyTapHook_<pid>_v1`; if another version of the tap already owns the process, `StartTap` also returns `0` but leaves the existing hook in place.

To uninstall the hook without unloading the DLL, call:

```c
int32_t StopTap(void); // extern "system", idempotent
```

It restores the original `GetBuffer`/`ReleaseBuffer` entries, cuts the shared heartbeat, and releases the hook mutex. The DLL stays loaded but inert. If the consumer disappears without calling `StopTap`, a watchdog restores the vtable after the heartbeat goes stale.

### 3. Read the audio

Open the mapping described below, keep the heartbeat fresh, and drain the ring. The tap does not know or care what the consumer does with the samples.

## Shared memory ABI (version 1)

The mapping name is `Local\SpotifyTap_<pid>_v1`, where `<pid>` is Spotify's process ID. The total size is `64 + 4 MiB`: a 64-byte header followed by a byte-oriented ring.

The object names and header layout form the version 1 ABI. Use a different `v1` suffix for incompatible variants so they do not collide with an existing reader.

All integers are little-endian `u32`/`u64`, and the header has C-compatible alignment.

| Offset | Field          | Type   | Meaning |
|-------:|----------------|--------|---------|
| 0      | `magic`        | `u32`  | `0x50545053` (`"SPTP"`) |
| 4      | `version`      | `u32`  | `1` |
| 8      | `sample_rate`  | `u32`  | Mix-format sample rate, filled in after setup |
| 12     | `channels`     | `u32`  | Always `2` |
| 16     | `frame_bytes`  | `u32`  | Always `8` (float32 stereo) |
| 20     | `format_tag`   | `u32`  | `WAVE_FORMAT_IEEE_FLOAT` (`3`) |
| 24     | `reserved0`    | `u32`  | Zero |
| 28     | `reserved1`    | `u32`  | Zero |
| 32     | `heartbeat_ms` | `u64`  | Consumer writes wall-clock milliseconds since the Unix epoch; `0` disables the tap |
| 40     | `write_bytes`  | `u64`  | Producer counter, free-running |
| 48     | `read_bytes`   | `u64`  | Consumer counter, free-running |
| 56     | `capacity`     | `u64`  | `4 * 1024 * 1024` |
| 64     | ring           | bytes  | Raw interleaved float32 stereo PCM |

Consumer protocol:

1. Open the mapping and map `64 + capacity` bytes.
2. Verify `magic` and `version`; wait until `sample_rate` is non-zero.
3. Write `heartbeat_ms` at least every 1500 ms, using wall-clock milliseconds since the Unix epoch (the tap compares it against `SystemTime`, so monotonic tick counts will not work).
4. Treat `[read_bytes, write_bytes)` as buffered data. The next byte sits at `write_bytes % capacity`; wrap around the ring as needed.
5. Only advance `read_bytes` after copying the data out, and read `write_bytes` with acquire semantics.
6. On shutdown, stop the heartbeat (or set it to `0`). The tap falls back to pass-through within 1.5 s.

The consumer counter is only used to bound the producer: if `write_bytes - read_bytes + incoming` would exceed `capacity`, the frame is dropped rather than blocking Spotify's audio thread. A stalled consumer therefore causes dropped frames, never a hang.

## Error codes

`StartTap` returns `0` on success (including "already started" and "another tap version owns this process"). Non-zero values are diagnostics:

| Code | Cause |
|-----:|-------|
| 1    | `CreateMutexW` failed |
| 2    | `CreateFileMappingW` failed |
| 3    | `MapViewOfFile` failed |
| 4    | `CoInitializeEx` failed |
| 5    | `CoCreateInstance(MMDeviceEnumerator)` failed |
| 6    | `GetDefaultAudioEndpoint` failed |
| 7    | `Activate(IAudioClient)` failed |
| 8    | `GetMixFormat` failed |
| 9    | Unsupported mix format (needs float32 stereo) |
| 10   | `IAudioClient::Initialize` failed |
| 11   | `GetService(IAudioRenderClient)` failed |
| 12   | Render client vtable is null |
| 13   | Vtable entries are null |
| 14   | `VirtualProtect` failed |

## Logging and troubleshooting

The DLL appends diagnostics to `%TEMP%\spotify-tap.log`, including the detected mix format and the patched vtable addresses. If the tap starts but no audio appears in the ring:

- Confirm the log shows `float=true`, `tap render format=stereo`, and a `patched pid=...` line.
- Confirm the consumer is writing `heartbeat_ms` frequently enough; without it the tap deliberately passes audio through.
- Confirm the consumer is mapping `Local\SpotifyTap_<pid>_v1` for the correct PID.
- Code signing or anti-cheat software may block injection or vtable patching; `VirtualProtect` error 14 is the usual symptom.

## Limitations

- Windows x64 only.
- Hooks the default console render endpoint only; switching audio devices requires restarting the tap.
- The heartbeat is compared with wall-clock time, so a large system clock adjustment can briefly pause capture (audio passes through instead).
- Shared-mode float32 stereo output only. The endpoint mix may be 5.1/7.1, but Windows upmixes after Spotify's stereo render client; the tap uses the endpoint only to resolve the shared vtable. Exclusive mode, non-float, and multichannel render clients are rejected.
- Targets the Spotify main process; other applications are not supported.
- No code signing is performed.
- `StopTap` and the stale-heartbeat watchdog restore the render vtable, but the DLL itself stays loaded in Spotify until the Spotify process exits. A later `StartTap` can reinstall the hook.

## License

MIT - see [LICENSE](LICENSE).
