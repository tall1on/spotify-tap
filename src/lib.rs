//! An in-process Spotify audio tap.
//!
//! The DLL is injected into the Spotify main process. It replaces the shared
//! `IAudioRenderClient` `GetBuffer`/`ReleaseBuffer` vtable entries, copies the
//! rendered float32 stereo frames into a shared-memory ring, and zeroes the
//! render buffer before handing it back to Spotify. A consumer process keeps a
//! heartbeat in the shared header; when the heartbeat stops the hook passes
//! audio through again, so a crashed or stopped consumer never leaves Spotify
//! permanently silent.

use std::{
    collections::HashMap,
    ffi::c_void,
    fs::OpenOptions,
    io::Write,
    mem::size_of,
    ptr,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
        Mutex, OnceLock,
    },
    time::{SystemTime, UNIX_EPOCH},
};
use windows::{
    core::{GUID, Interface, HRESULT, PCWSTR},
    Win32::{
        Foundation::{
            CloseHandle, GetLastError, SetLastError, ERROR_ALREADY_EXISTS, HANDLE,
            INVALID_HANDLE_VALUE, WIN32_ERROR,
        },
        Media::{
            Audio::{
                eConsole, eRender, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator,
                MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
            },
            Multimedia::WAVE_FORMAT_IEEE_FLOAT,
        },
        System::{
            Com::{
                CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
            },
            Memory::{
                CreateFileMappingW, MapViewOfFile, VirtualProtect, FILE_MAP_ALL_ACCESS,
                PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS, PAGE_READWRITE,
            },
            Threading::{CreateMutexW, GetCurrentProcessId},
        },
    },
};

const MAGIC: u32 = 0x5054_5053; // "SPTP"
const VERSION: u32 = 1;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const HEADER_SIZE: usize = 64;
const RING_CAPACITY: usize = 4 * 1024 * 1024;
const HEARTBEAT_TIMEOUT_MS: u64 = 1_500;

type GetBufferFn = unsafe extern "system" fn(*mut c_void, u32, *mut *mut u8) -> HRESULT;
type ReleaseBufferFn = unsafe extern "system" fn(*mut c_void, u32, u32) -> HRESULT;

#[repr(C)]
struct Header {
    magic: u32,
    version: u32,
    sample_rate: u32,
    channels: u32,
    frame_bytes: u32,
    format_tag: u32,
    reserved0: u32,
    reserved1: u32,
    heartbeat_ms: AtomicU64,
    write_bytes: AtomicU64,
    read_bytes: AtomicU64,
    capacity: u64,
}

#[derive(Clone, Copy)]
struct BufferState {
    data: usize,
    requested_frames: u32,
}

static ORIG_GET_BUFFER: AtomicUsize = AtomicUsize::new(0);
static ORIG_RELEASE_BUFFER: AtomicUsize = AtomicUsize::new(0);
static SHARED: AtomicUsize = AtomicUsize::new(0);
static FORMAT_TAG: AtomicU32 = AtomicU32::new(0);
static CHANNELS: AtomicU32 = AtomicU32::new(0);
static SAMPLE_RATE: AtomicU32 = AtomicU32::new(0);
static FRAME_BYTES: AtomicU32 = AtomicU32::new(0);
static STARTED: AtomicBool = AtomicBool::new(false);
static HOOK_MUTEX: AtomicUsize = AtomicUsize::new(0);

fn buffers() -> &'static Mutex<HashMap<usize, BufferState>> {
    static BUFFERS: OnceLock<Mutex<HashMap<usize, BufferState>>> = OnceLock::new();
    BUFFERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn log(message: &str) {
    let path = std::env::temp_dir().join("spotify-tap.log");
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{message}");
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn header() -> *mut Header {
    SHARED.load(Ordering::Acquire) as *mut Header
}

unsafe fn reader_active() -> bool {
    let header = header();
    if header.is_null() {
        return false;
    }
    let heartbeat = (*header).heartbeat_ms.load(Ordering::Acquire);
    heartbeat != 0 && now_ms().saturating_sub(heartbeat) < HEARTBEAT_TIMEOUT_MS
}

unsafe fn write_ring(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let header = header();
    if header.is_null() {
        return;
    }
    let capacity = (*header).capacity as usize;
    if capacity == 0 || bytes.len() > capacity {
        return;
    }
    let write = (*header).write_bytes.load(Ordering::Relaxed);
    let read = (*header).read_bytes.load(Ordering::Acquire);
    if write.wrapping_sub(read) + bytes.len() as u64 > capacity as u64 {
        return;
    }
    let ring = (header as *mut u8).add(HEADER_SIZE);
    let offset = (write % capacity as u64) as usize;
    let first = bytes.len().min(capacity - offset);
    ptr::copy_nonoverlapping(bytes.as_ptr(), ring.add(offset), first);
    if first < bytes.len() {
        ptr::copy_nonoverlapping(bytes.as_ptr().add(first), ring, bytes.len() - first);
    }
    (*header)
        .write_bytes
        .store(write + bytes.len() as u64, Ordering::Release);
}

unsafe extern "system" fn get_buffer_hook(
    this: *mut c_void,
    frames: u32,
    data: *mut *mut u8,
) -> HRESULT {
    let original = ORIG_GET_BUFFER.load(Ordering::Acquire);
    if original == 0 {
        return HRESULT(0x8000_4005_u32 as i32);
    }
    let original: GetBufferFn = std::mem::transmute(original);
    let result = original(this, frames, data);
    if result.is_ok() && !data.is_null() && !(*data).is_null() {
        if let Ok(mut buffers) = buffers().lock() {
            buffers.insert(
                this as usize,
                BufferState {
                    data: *data as usize,
                    requested_frames: frames,
                },
            );
        }
    }
    result
}

unsafe extern "system" fn release_buffer_hook(
    this: *mut c_void,
    frames: u32,
    flags: u32,
) -> HRESULT {
    let state = buffers()
        .lock()
        .ok()
        .and_then(|mut buffers| buffers.remove(&(this as usize)));

    if frames > 0 && (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) == 0 {
        if let Some(state) = state {
            let frame_bytes = FRAME_BYTES.load(Ordering::Relaxed) as usize;
            let format_ok = FORMAT_TAG.load(Ordering::Relaxed) == WAVE_FORMAT_IEEE_FLOAT
                && frame_bytes > 0
                && CHANNELS.load(Ordering::Relaxed) == 2;
            if format_ok && reader_active() {
                let frames = frames.min(state.requested_frames) as usize;
                let bytes = frames * frame_bytes;
                if bytes > 0 {
                    let source = std::slice::from_raw_parts(state.data as *const u8, bytes);
                    write_ring(source);
                    ptr::write_bytes(state.data as *mut u8, 0, bytes);
                }
            }
        }
    }

    let original = ORIG_RELEASE_BUFFER.load(Ordering::Acquire);
    if original == 0 {
        return HRESULT(0x8000_4005_u32 as i32);
    }
    let original: ReleaseBufferFn = std::mem::transmute(original);
    original(this, frames, flags)
}

fn setup() -> Result<(), i32> {
    let pid = unsafe { GetCurrentProcessId() };
    let mutex_name = wide(&format!("Local\\SpotifyTapHook_{pid}_v1"));
    unsafe { SetLastError(WIN32_ERROR(0)) };
    let mutex: HANDLE = match unsafe { CreateMutexW(None, false, PCWSTR(mutex_name.as_ptr())) } {
        Ok(handle) => handle,
        Err(error) => {
            log(&format!("CreateMutexW failed: {error}"));
            return Err(1);
        }
    };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        log("another tap version already owns this process");
        let _ = unsafe { CloseHandle(mutex) };
        return Ok(());
    }
    HOOK_MUTEX.store(mutex.0 as usize, Ordering::SeqCst);

    let mapping_name = wide(&format!("Local\\SpotifyTap_{pid}_v1"));
    let mapping: HANDLE = unsafe {
        CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            None,
            PAGE_READWRITE,
            0,
            (HEADER_SIZE + RING_CAPACITY) as u32,
            PCWSTR(mapping_name.as_ptr()),
        )
    }
    .map_err(|error| {
        log(&format!("CreateFileMappingW failed: {error}"));
        2
    })?;
    let view = unsafe {
        MapViewOfFile(
            mapping,
            FILE_MAP_ALL_ACCESS,
            0,
            0,
            HEADER_SIZE + RING_CAPACITY,
        )
    };
    if view.Value.is_null() {
        log("MapViewOfFile failed");
        return Err(3);
    }
    let header = view.Value as *mut Header;
    unsafe { ptr::write_bytes(view.Value as *mut u8, 0, HEADER_SIZE) };
    unsafe {
        (*header).magic = MAGIC;
        (*header).version = VERSION;
        (*header).capacity = RING_CAPACITY as u64;
    }
    SHARED.store(view.Value as usize, Ordering::Release);

    if unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_err() {
        log("CoInitializeEx failed");
        return Err(4);
    }
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map_err(|error| {
            log(&format!("CoCreateInstance failed: {error}"));
            5
        })?;
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }.map_err(|error| {
        log(&format!("GetDefaultAudioEndpoint failed: {error}"));
        6
    })?;
    let client: IAudioClient = unsafe { device.Activate::<IAudioClient>(CLSCTX_ALL, None) }
        .map_err(|error| {
            log(&format!("Activate IAudioClient failed: {error}"));
            7
        })?;

    let mix_format = unsafe { client.GetMixFormat() }.map_err(|error| {
        log(&format!("GetMixFormat failed: {error}"));
        8
    })?;
    let format = unsafe { *mix_format };
    let format_tag = format.wFormatTag;
    let channels = format.nChannels;
    let sample_rate = format.nSamplesPerSec;
    let bits_per_sample = format.wBitsPerSample;
    let block_align = format.nBlockAlign;
    let cb_size = format.cbSize;
    let ieee_float = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
    let is_float = if format_tag as u32 == WAVE_FORMAT_IEEE_FLOAT {
        true
    } else if format_tag == WAVE_FORMAT_EXTENSIBLE && cb_size >= 22 {
        unsafe {
            ptr::read_unaligned((mix_format as *const u8).add(24) as *const GUID) == ieee_float
        }
    } else {
        false
    };
    log(&format!(
        "pid={pid} mix format tag={format_tag} channels={channels} rate={sample_rate} bits={bits_per_sample} block={block_align} float={is_float}"
    ));
    if !is_float || channels != 2 || block_align != 8 {
        unsafe { CoTaskMemFree(Some(mix_format as *const c_void)) };
        log("unsupported mix format (need float32 stereo)");
        return Err(9);
    }
    FORMAT_TAG.store(WAVE_FORMAT_IEEE_FLOAT, Ordering::SeqCst);
    CHANNELS.store(channels as u32, Ordering::SeqCst);
    SAMPLE_RATE.store(sample_rate, Ordering::SeqCst);
    FRAME_BYTES.store(block_align as u32, Ordering::SeqCst);

    let initialized = unsafe {
        client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 0, 0, mix_format, None)
    };
    unsafe { CoTaskMemFree(Some(mix_format as *const c_void)) };
    initialized.map_err(|error| {
        log(&format!("Initialize failed: {error}"));
        10
    })?;

    let render: IAudioRenderClient = unsafe { client.GetService::<IAudioRenderClient>() }.map_err(
        |error| {
            log(&format!("GetService IAudioRenderClient failed: {error}"));
            11
        },
    )?;
    let raw = render.as_raw() as *mut usize;
    let vtable = unsafe { *(raw as *mut *mut usize) };
    if vtable.is_null() {
        log("render client vtable is null");
        return Err(12);
    }
    let get_buffer = unsafe { *vtable.add(3) };
    let release_buffer = unsafe { *vtable.add(4) };
    if get_buffer == 0 || release_buffer == 0 {
        log("render client vtable entries are null");
        return Err(13);
    }
    ORIG_GET_BUFFER.store(get_buffer, Ordering::SeqCst);
    ORIG_RELEASE_BUFFER.store(release_buffer, Ordering::SeqCst);

    let mut old_protect = PAGE_PROTECTION_FLAGS(0);
    unsafe {
        VirtualProtect(
            vtable.add(3) as *const c_void,
            size_of::<usize>() * 2,
            PAGE_EXECUTE_READWRITE,
            &mut old_protect,
        )
    }
    .map_err(|error| {
        log(&format!("VirtualProtect failed: {error}"));
        14
    })?;
    unsafe {
        ptr::write(vtable.add(3), get_buffer_hook as usize);
        ptr::write(vtable.add(4), release_buffer_hook as usize);
    }

    unsafe {
        (*header).sample_rate = SAMPLE_RATE.load(Ordering::SeqCst);
        (*header).channels = CHANNELS.load(Ordering::SeqCst);
        (*header).frame_bytes = FRAME_BYTES.load(Ordering::SeqCst);
        (*header).format_tag = FORMAT_TAG.load(Ordering::SeqCst);
        (*header).magic = MAGIC;
        (*header).version = VERSION;
    }
    log(&format!(
        "patched pid={pid} vtable={vtable:p} get={get_buffer:x} release={release_buffer:x}"
    ));
    Ok(())
}

#[no_mangle]
pub extern "system" fn StartTap() -> i32 {
    if STARTED.swap(true, Ordering::SeqCst) {
        return 0;
    }
    log("StartTap");
    match setup() {
        Ok(()) => 0,
        Err(code) => {
            STARTED.store(false, Ordering::SeqCst);
            code
        }
    }
}
