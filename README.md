# clubify-spotify-tap

In-process Spotify render-buffer tap and silencer for Clubify.

The crate builds `clubify_spotify_tap.dll`, a Windows x64 `cdylib` that is injected into the Spotify main process. Once started, it copies Spotify's rendered float32 stereo frames into a shared-memory ring, then zeroes the render buffer so Spotify itself plays nothing. A reader in Clubify consumes the audio from the ring, processes it, and plays it wherever it likes.

This repository ships only the native tap. Injection, the reader process, and audio output live in the Clubify host.

## How it works

1. `StartTap()` creates a per-process mutex, a per-process file mapping, and opens the default render endpoint (`eRender` / `eConsole`) in shared mode to learn the mix format.
2. It resolves the process-wide `IAudioRenderClient` vtable and overwrites slots 3 and 4 (`GetBuffer`, `ReleaseBuffer`) with hooks. The original entries are kept and always called, so Spotify's own audio pipeline still runs normally.
3. `GetBuffer` records the buffer pointer and requested frame count for the returned `this` instance.
4. `ReleaseBuffer` looks the buffer up, checks the format (`WAVE_FORMAT_IEEE_FLOAT`, 2 channels, 8-byte block align), and — only while a reader heartbeat is alive — copies the rendered frames into the shared ring and zeroes the source buffer before delegating to the original method.
5. The reader must publish a heartbeat roughly once per second. If it stops, crashes, or never attaches, the hook passes audio through untouched, so Spotify is never left permanently silent.

The hook only touches the default render endpoint of the host process, in shared mode, with float32 stereo output. Other formats stop setup with an error code instead of corrupting audio.

## Requirements

- Windows x64
- Rust 1.88+ with the MSVC toolchain; `.cargo/config.toml` statically links the CRT for the `x86_64-pc-windows-msvc` target
- Node.js 18+ only if you use the helper build script
- A Clubify-compatible reader that understands the shared-memory layout below

## Build

Plain Cargo build:

```powershell
cargo build --release
# -> target\release\clubify_spotify_tap.dll
```

`/target/` is gitignored.

Helper script (used by the Clubify tree):

```powershell
node scripts/build-native.mjs
```

The script runs `cargo build --release --locked`, parses Cargo's JSON messages to locate the produced `.dll`, and copies it to `..\loopback-capture\build\Release\clubify_spotify_tap.dll`. It deletes any existing copy first, because a Spotify process that has already loaded the DLL keeps the file locked; the engine injects a hash-named copy from `%LOCALAPPDATA%`, leaving this source file free to replace.

## Integration

The exported entry point is:

```c
int32_t StartTap(void); // extern "system", idempotent
```

Call it once after the DLL is loaded into Spotify. A second call returns `0` without doing anything. Setup takes a process-wide mutex named `Local\ClubifySpotifyTapHook_<pid>_v1`; if another version of the tap already owns the process, `StartTap` also returns `0` but leaves the existing hook in place.

### Shared memory ABI (version 1)

The mapping name is `Local\ClubifySpotifyTap_<pid>_v1`, where `<pid>` is Spotify's process ID. Total size is `64 + 4 MiB`: a 64-byte header followed by a byte-oriented ring.

All integers are little-endian `u32`/`u64`. Multibyte fields are declared with C-compatible alignment in the Rust `Header` struct.

| Offset | Field          | Type   | Meaning |
|-------:|----------------|--------|---------|
| 0      | `magic`        | `u32`  | `0x434C4254` (`"CLBT"`) |
| 4      | `version`      | `u32`  | `1` |
| 8      | `sample_rate`  | `u32`  | Mix-format sample rate, filled in after setup |
| 12     | `channels`     | `u32`  | Always `2` |
| 16     | `frame_bytes`  | `u32`  | Always `8` (float32 stereo) |
| 20     | `format_tag`   | `u32`  | `WAVE_FORMAT_IEEE_FLOAT` (`3`) |
| 24     | `reserved0`    | `u32`  | Zero |
| 28     | `reserved1`    | `u32`  | Zero |
| 32     | `heartbeat_ms` | `u64`  | Reader writes wall-clock milliseconds since the Unix epoch; `0` disables the tap |
| 40     | `write_bytes`  | `u64`  | Producer counter, free-running |
| 48     | `read_bytes`   | `u64`  | Consumer counter, free-running |
| 56     | `capacity`     | `u64`  | `4 * 1024 * 1024` |
| 64     | ring           | bytes  | Raw interleaved float32 stereo PCM |

Reader protocol:

1. Open the mapping and map `64 + capacity` bytes.
2. Verify `magic` and `version`; wait until `sample_rate` is non-zero.
3. Write `heartbeat_ms` at least every 1500 ms, using wall-clock milliseconds since the Unix epoch (the tap compares it against `SystemTime`, so monotonic tick counts will not work).
4. Treat `[read_bytes, write_bytes)` as buffered data. The next byte sits at `write_bytes % capacity`; wrap around the ring as needed.
5. Only advance `read_bytes` after copying the data out, and read `write_bytes` with acquire semantics.
6. On shutdown, stop the heartbeat (or set it to `0`). The tap falls back to pass-through within 1.5 s.

The consumer counter is only used to bound the producer: if `write_bytes - read_bytes + incoming` would exceed `capacity`, the frame is dropped rather than blocking Spotify's audio thread. A stalled reader therefore causes dropped frames, never a hang.

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

The DLL appends diagnostics to `%TEMP%\clubify-tap.log`, including the detected mix format and the patched vtable addresses. If audio is silent and nothing is being captured:

- Confirm the log shows `float=true`, `channels=2`, `block=8`, and a `patched pid=...` line.
- Confirm the reader is writing `heartbeat_ms` frequently enough; without it the tap deliberately passes audio through.
- Confirm the reader is mapping `Local\ClubifySpotifyTap_<pid>_v1` for the correct PID.
- A code signing or anti-cheat product may block vtable patching or injection entirely; `VirtualProtect` error 14 is the usual symptom.

## Limitations

- Windows x64 only.
- Hooks the default console render endpoint only; switching audio devices requires restarting the tap.
- The heartbeat is compared with wall-clock time, so a large system clock adjustment can briefly pause capture (audio passes through instead).
- Shared-mode float32 stereo output only. Exclusive mode, non-float mix formats, and multichannel endpoints are rejected.
- Only the Spotify main process is targeted; there is no support for other players.
- No code signing is performed.

## License

MIT — see [LICENSE](LICENSE).
