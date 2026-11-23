//! Decodes a looping clip, an album's motion cover or a song's Canvas, into
//! frames for the full player, through Media Foundation's source reader.
//! Media Foundation is loaded at run time: Windows N comes without it, and
//! linking it would stop Lily starting there rather than just going without.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::Duration;

use slint::{Rgba8Pixel, SharedPixelBuffer};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVDecNumWorkerThreads, IMFAttributes, IMFMediaType, IMFSample, IMFSourceReader,
    IMFSourceReaderEx, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE,
    MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_SUBTYPE, MF_SOURCE_READER_ALL_STREAMS,
    MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED, MF_SOURCE_READERF_ENDOFSTREAM,
    MF_SOURCE_READERF_ERROR, MF_VERSION, MFMediaType_Video, MFSTARTUP_LITE,
    MFT_CATEGORY_VIDEO_DECODER, MFVideoFormat_RGB32,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Variant::VT_I8;
use windows::core::{Error, GUID, HRESULT, HSTRING, Interface, PCSTR, PCWSTR, Result, s, w};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};

/// Past any clip Apple or Spotify sends, so only an absurd size is refused.
const MAX_EDGE: u32 = 4096;

pub struct Frame {
    pub pixels: SharedPixelBuffer<Rgba8Pixel>,
    pub at: Duration,
}

/// Decodes the clip at `path` on a thread of its own, over and over, at its
/// own size, handing each frame over only as the receiver takes it. `at` is
/// the frame's presentation time counted across loops, so it only grows. The
/// thread ends once the receiver is dropped, or on any error (logged at warn,
/// without the path's user folder). Pausing is the receiver not pulling: the
/// thread then blocks and costs no CPU.
pub fn play(path: PathBuf) -> Receiver<Frame> {
    // No buffer, as the receiver keeps the next frame itself.
    let (frames, receiver) = sync_channel(0);
    let spawned = std::thread::Builder::new()
        .name("motion video".into())
        .spawn(move || {
            if let Err(e) = run(&path, &frames) {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                log::warn!("Motion clip {name} stopped: {e}");
            }
        });
    if let Err(e) = spawned {
        log::warn!("Couldn't start a motion clip's thread: {e}");
    }
    receiver
}

fn run(path: &Path, frames: &SyncSender<Frame>) -> std::result::Result<(), String> {
    let mf = Mf::load()?;
    // Declared in this order so the reader goes first, then Media
    // Foundation, then COM.
    let _com = Com::init()?;
    let _started = mf.start()?;
    decode(&mf, path, frames).map_err(|e| e.to_string())
}

// Software only: the GPU decoder cuts the CPU by about two thirds but
// leaves its driver holding a hundred MB or so for the rest of the run.
fn decode(mf: &Mf, path: &Path, frames: &SyncSender<Frame>) -> Result<()> {
    let video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    // SAFETY: Media Foundation is started on this thread, every pointer
    // passed is to a live local, and the interfaces come from its calls.
    let reader = unsafe {
        let attributes = mf.attributes()?;
        // The plain converter, as the advanced processor (which can scale)
        // held about 60 MB more for a Canvas; the full player scales anyway.
        attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)?;
        let reader = mf.reader(path, &attributes)?;
        reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
        reader.SetStreamSelection(video, true)?;
        let output = mf.media_type()?;
        output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        reader.SetCurrentMediaType(video, None, &output)?;
        if let Err(e) = one_thread(&reader, video) {
            log::debug!("A motion clip decodes on every core: {e}");
        }
        reader
    };
    // SAFETY: as above.
    let mut layout = self::layout(&unsafe { reader.GetCurrentMediaType(video) }?)?;

    // Each loop counts from its own first frame, as a seek needn't land on 0.
    let mut base = Duration::ZERO;
    let mut first = None;
    let mut last = None;
    let mut end = 0;
    loop {
        let (mut flags, mut time, mut sample) = (0, 0, None);
        // SAFETY: the out pointers are to live locals.
        unsafe {
            reader.ReadSample(
                video,
                0,
                None,
                Some(&mut flags),
                Some(&mut time),
                Some(&mut sample),
            )
        }?;
        if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
            return Err(failed("the reader failed"));
        }
        if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0 {
            // SAFETY: as above.
            layout = self::layout(&unsafe { reader.GetCurrentMediaType(video) }?)?;
        }
        // The plain converter now and then repeats a frame's time.
        if let Some(sample) = sample.filter(|_| last.is_none_or(|last| time > last)) {
            last = Some(time);
            let since = time.saturating_sub(*first.get_or_insert(time));
            // SAFETY: the sample is live.
            let length = unsafe { sample.GetSampleDuration() }.unwrap_or(0);
            end = end.max(since.saturating_add(length));
            let frame = Frame {
                pixels: pixels(&sample, &layout)?,
                at: base + ticks(since),
            };
            if frames.send(frame).is_err() {
                return Ok(());
            }
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            if first.take().is_none() {
                return Err(failed("the clip has no frames"));
            }
            last = None;
            base += ticks(end);
            end = 0;
            let mut start = PROPVARIANT::default();
            // SAFETY: a zeroed PROPVARIANT is a valid empty one, made an 8-byte
            // integer of 0 here, which needs no clearing; `GUID_NULL` asks for
            // 100 ns units.
            unsafe {
                (*start.Anonymous.Anonymous).vt = VT_I8;
                reader.SetCurrentPosition(&GUID::zeroed(), &start)?;
            }
        }
    }
}

/// One decoding thread rather than one per core, as each holds frames of its
/// own: on a many-core machine a Canvas took 300 MB with them and 120 with
/// one, for the same CPU.
/// Through `ICodecAPI` the decoder refuses it once its types are set, but it
/// still reads its attributes as streaming starts.
fn one_thread(reader: &IMFSourceReader, stream: u32) -> Result<()> {
    let reader: IMFSourceReaderEx = reader.cast()?;
    let (mut category, mut decoder) = (GUID::zeroed(), None);
    // SAFETY: the out pointers are to live locals.
    unsafe { reader.GetTransformForStream(stream, 0, Some(&mut category), &mut decoder) }?;
    match decoder {
        // SAFETY: the decoder is live.
        Some(decoder) if category == MFT_CATEGORY_VIDEO_DECODER => unsafe {
            decoder
                .GetAttributes()?
                .SetUINT32(&CODECAPI_AVDecNumWorkerThreads, 1)
        },
        _ => Err(failed("no decoder")),
    }
}

fn failed(message: &str) -> Error {
    Error::new(E_FAIL, message)
}

/// Media Foundation's 100 ns units, negatives (before a clip's start) as 0.
fn ticks(time: i64) -> Duration {
    Duration::from_nanos((time.max(0) as u64).saturating_mul(100))
}

/// Where the picture sits in each frame's buffer.
struct Layout {
    width: u32,
    height: u32,
    /// The byte its top left pixel starts at.
    top: usize,
    /// Bytes from one row to the next, negative for bottom-up.
    pitch: isize,
}

fn layout(media: &IMFMediaType) -> Result<Layout> {
    // SAFETY: the media type is live.
    let packed = unsafe { media.GetUINT64(&MF_MT_FRAME_SIZE) }?;
    let (width, height) = ((packed >> 32) as u32, packed as u32);
    if width.max(height) > MAX_EDGE {
        return Err(failed("the clip is too large"));
    }
    // SAFETY: as above. The stride is signed, stored unsigned.
    let pitch = unsafe { media.GetUINT32(&MF_MT_DEFAULT_STRIDE) }
        .map_or(width as isize * 4, |p| p as i32 as isize);
    // The decoder pads frames to whole macroblocks; the aperture is the picture.
    let (x, y, cropped_width, cropped_height) = aperture(media)
        .filter(|&(x, y, w, h)| w > 0 && h > 0 && x + w <= width && y + h <= height)
        .unwrap_or((0, 0, width, height));
    Ok(Layout {
        width: cropped_width,
        height: cropped_height,
        top: offset(height, pitch, x, y),
        pitch,
    })
}

/// The byte pixel (`x`, `y`) starts at, in a frame `height` rows tall.
fn offset(height: u32, pitch: isize, x: u32, y: u32) -> usize {
    // A bottom-up frame's top row is its last in memory.
    let row = if pitch < 0 {
        height.saturating_sub(1 + y)
    } else {
        y
    };
    pitch.unsigned_abs() * row as usize + x as usize * 4
}

/// The minimum display aperture's offset and size, in whole pixels.
fn aperture(media: &IMFMediaType) -> Option<(u32, u32, u32, u32)> {
    // An `MFVideoArea`: two `MFOffset`s (a fraction, then a whole i16) and a `SIZE`.
    let mut area = [0; 16];
    let mut length = 0;
    // SAFETY: the media type is live and the pointers are to live locals.
    unsafe {
        media.GetBlob(
            &MF_MT_MINIMUM_DISPLAY_APERTURE,
            &mut area,
            Some(&mut length),
        )
    }
    .ok()?;
    if length != 16 {
        return None;
    }
    let whole = |at: usize| u32::try_from(i16::from_le_bytes([area[at], area[at + 1]])).ok();
    let size =
        |at: usize| u32::try_from(i32::from_le_bytes(area[at..at + 4].try_into().ok()?)).ok();
    Some((whole(2)?, whole(6)?, size(8)?, size(12)?))
}

fn pixels(sample: &IMFSample, layout: &Layout) -> Result<SharedPixelBuffer<Rgba8Pixel>> {
    let Layout {
        width,
        height,
        top,
        pitch,
    } = *layout;
    // SAFETY: the buffer stays locked while `bytes` is read, and Lock gives
    // the length it holds, so `bytes` covers only memory it owns.
    unsafe {
        let buffer = sample.GetBufferByIndex(0)?;
        let (mut start, mut length) = (std::ptr::null_mut(), 0);
        buffer.Lock(&mut start, None, Some(&mut length))?;
        let bytes = std::slice::from_raw_parts(start, length as usize);
        let rgba = rgba(bytes, top, pitch, width, height);
        buffer.Unlock()?;
        rgba.ok_or_else(|| failed("a frame doesn't fit its buffer"))
    }
}

/// BGRX rows from `top`, `pitch` bytes apart (negative for bottom-up), as
/// RGBA, top row first; `None` should a row run outside `bytes`.
fn rgba(
    bytes: &[u8],
    top: usize,
    pitch: isize,
    width: u32,
    height: u32,
) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    if width == 0 || height == 0 {
        return None;
    }
    let row_bytes = (width as usize).checked_mul(4)?;
    // Before allocating, so an absurd size can't abort on allocation.
    let last = top.checked_add_signed(pitch.checked_mul(height as isize - 1)?)?;
    bytes.get(last..last.checked_add(row_bytes)?)?;
    let mut out = SharedPixelBuffer::new(width, height);
    let rows = out.make_mut_slice().chunks_exact_mut(width as usize);
    for (y, row_out) in rows.enumerate() {
        let start = top.checked_add_signed(pitch.checked_mul(y as isize)?)?;
        let row = bytes.get(start..start.checked_add(row_bytes)?)?;
        for (pixel, &[b, g, r, _]) in row_out.iter_mut().zip(row.as_chunks::<4>().0) {
            *pixel = Rgba8Pixel::new(r, g, b, 255);
        }
    }
    Some(out)
}

type Startup = unsafe extern "system" fn(u32, u32) -> HRESULT;
type Shutdown = unsafe extern "system" fn() -> HRESULT;
type CreateAttributes = unsafe extern "system" fn(*mut Option<IMFAttributes>, u32) -> HRESULT;
type CreateMediaType = unsafe extern "system" fn(*mut Option<IMFMediaType>) -> HRESULT;
type CreateReader =
    unsafe extern "system" fn(PCWSTR, *mut c_void, *mut Option<IMFSourceReader>) -> HRESULT;

/// The few Media Foundation functions used, found at run time.
struct Mf {
    startup: Startup,
    shutdown: Shutdown,
    create_attributes: CreateAttributes,
    create_media_type: CreateMediaType,
    create_reader: CreateReader,
}

impl Mf {
    fn load() -> std::result::Result<Self, String> {
        let plat = library(w!("mfplat.dll"))?;
        let readwrite = library(w!("mfreadwrite.dll"))?;
        // SAFETY: each type is the function's documented signature, and the
        // libraries are never freed, so the pointers stay valid.
        unsafe {
            Ok(Self {
                startup: function(plat, s!("MFStartup"))?,
                shutdown: function(plat, s!("MFShutdown"))?,
                create_attributes: function(plat, s!("MFCreateAttributes"))?,
                create_media_type: function(plat, s!("MFCreateMediaType"))?,
                create_reader: function(readwrite, s!("MFCreateSourceReaderFromURL"))?,
            })
        }
    }

    fn start(&self) -> std::result::Result<Started, String> {
        // SAFETY: a plain call, paired with `MFShutdown` by `Started`.
        unsafe { (self.startup)(MF_VERSION, MFSTARTUP_LITE) }
            .ok()
            .map_err(|e| format!("Media Foundation didn't start: {e}"))?;
        Ok(Started(self.shutdown))
    }

    unsafe fn attributes(&self) -> Result<IMFAttributes> {
        let mut attributes = None;
        // SAFETY: the out pointer is to a live local.
        unsafe { (self.create_attributes)(&mut attributes, 1) }.ok()?;
        attributes.ok_or_else(|| failed("no attributes"))
    }

    unsafe fn media_type(&self) -> Result<IMFMediaType> {
        let mut media = None;
        // SAFETY: the out pointer is to a live local.
        unsafe { (self.create_media_type)(&mut media) }.ok()?;
        media.ok_or_else(|| failed("no media type"))
    }

    unsafe fn reader(&self, path: &Path, attributes: &IMFAttributes) -> Result<IMFSourceReader> {
        let path = HSTRING::from(path);
        let mut reader = None;
        // SAFETY: `path` is null-terminated and outlives the call, which
        // takes its own reference to the attributes.
        unsafe { (self.create_reader)(PCWSTR(path.as_ptr()), attributes.as_raw(), &mut reader) }
            .ok()?;
        reader.ok_or_else(|| failed("no reader"))
    }
}

fn library(name: PCWSTR) -> std::result::Result<*mut c_void, String> {
    // SAFETY: `name` is a null-terminated literal; only System32 is searched,
    // so nothing beside the exe can stand in.
    let module = unsafe {
        LoadLibraryExW(
            name.as_ptr(),
            std::ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    };
    if module.is_null() {
        return Err("Media Foundation isn't installed".into());
    }
    Ok(module)
}

/// # Safety
/// `F` must be the function's own signature.
unsafe fn function<F: Copy>(module: *mut c_void, name: PCSTR) -> std::result::Result<F, String> {
    // SAFETY: `name` is a null-terminated literal and `module` is loaded.
    let found = unsafe { GetProcAddress(module, name.as_ptr()) }
        .ok_or("Media Foundation lacks a function this needs")?;
    // SAFETY: the caller vouches for the signature; both are function pointers.
    Ok(unsafe { std::mem::transmute_copy(&found) })
}

/// Shuts Media Foundation down once the reader is gone.
struct Started(Shutdown);

impl Drop for Started {
    fn drop(&mut self) {
        // SAFETY: pairs the `MFStartup` that made this.
        let _ = unsafe { (self.0)() };
    }
}

struct Com;

impl Com {
    fn init() -> std::result::Result<Self, String> {
        // SAFETY: a plain call on this thread, paired with `CoUninitialize`.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(|e| format!("COM didn't start: {e}"))?;
        Ok(Com)
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        // SAFETY: pairs the `CoInitializeEx` that made this.
        unsafe { CoUninitialize() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::RecvTimeoutError;

    fn clip(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join("lily-motion-video")
            .join("clips")
            .join(name)
    }

    fn pixel(frame: &Frame, x: u32, y: u32) -> [u8; 3] {
        let p = frame.pixels.as_slice()[(y * frame.pixels.width() + x) as usize];
        [p.r, p.g, p.b]
    }

    #[test]
    fn rows_come_out_top_first_from_a_negative_pitch() {
        // Two rows of two pixels, padded to 12 bytes, stored bottom-up.
        let bottom = [1, 2, 3, 0, 4, 5, 6, 0, 9, 9, 9, 9];
        let top = [7, 8, 9, 0, 10, 11, 12, 0, 9, 9, 9, 9];
        let bytes = [bottom, top].concat();
        let rows = rgba(&bytes, 12, -12, 2, 2).unwrap();
        assert_eq!(
            rows.as_bytes(),
            [9, 8, 7, 255, 12, 11, 10, 255, 3, 2, 1, 255, 6, 5, 4, 255]
        );
        assert!(rgba(&bytes, 12, -12, 2, 3).is_none());
        assert!(rgba(&bytes, 0, 12, 7, 1).is_none());
        assert!(rgba(&bytes, 0, 12, 0, 1).is_none());
        // The right-hand pixel of the top row alone.
        let crop = rgba(&bytes, offset(2, -12, 1, 0), -12, 1, 1).unwrap();
        assert_eq!(crop.as_bytes(), [12, 11, 10, 255]);
        assert_eq!(offset(2, 12, 1, 1), 16);
    }

    /// Checks the size, the bands ffmpeg drew (red, white, the picture,
    /// black, green) at both edges and down the middle, and that `at` grows
    /// by a frame at a time, three times round the clip.
    fn plays(name: &str, size: (u32, u32)) {
        let frames = play(clip(name));
        let (width, height) = size;
        let bands = [
            (0, [255, 0, 0]),
            (height / 8 + 4, [255, 255, 255]),
            (height * 13 / 16 + 4, [0, 0, 0]),
            (height - 1, [0, 255, 0]),
        ];
        let mut last: Option<Duration> = None;
        for _ in 0..450 {
            let frame = frames.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(
                (frame.pixels.width(), frame.pixels.height()),
                size,
                "{name}"
            );
            for x in [0, width / 2, width - 1] {
                for (y, colour) in bands {
                    let found = pixel(&frame, x, y);
                    let near = found.iter().zip(colour).all(|(&a, b)| a.abs_diff(b) <= 10);
                    assert!(near, "{name}: {found:?} at {x},{y}");
                }
            }
            if let Some(last) = last {
                let step = frame.at.checked_sub(last).unwrap();
                let off = step.abs_diff(Duration::from_millis(33));
                assert!(off < Duration::from_millis(2), "{step:?}");
            }
            last = Some(frame.at);
        }
        let off = last.unwrap().abs_diff(Duration::from_secs(15));
        assert!(off < Duration::from_millis(50));
    }

    #[test]
    #[ignore = "needs the clips ffmpeg made"]
    fn plays_and_loops_plain_and_fragmented() {
        for name in ["plain_1080x1080.mp4", "frag_1080x1080.mp4"] {
            plays(name, (1080, 1080));
        }
        for name in [
            "plain_720x1280.mp4",
            "frag_720x1280.mp4",
            "odd dír #1 %20/clip #2.mp4",
        ] {
            plays(name, (720, 1280));
        }
    }

    #[test]
    #[ignore = "needs the clips ffmpeg made"]
    fn a_bad_file_just_disconnects() {
        for name in ["missing.mp4", "corrupt.mp4"] {
            let frames = play(clip(name));
            let ended = frames.recv_timeout(Duration::from_secs(5));
            assert!(
                matches!(ended, Err(RecvTimeoutError::Disconnected)),
                "{name}"
            );
        }
    }

    /// Plays real Apple and Spotify clips should another check have saved
    /// them, printing their size.
    #[test]
    #[ignore = "needs clips saved from Apple Music or Spotify"]
    fn plays_real_clips() {
        let folder = std::env::temp_dir().join("lily-motion-sample");
        for name in ["apple.mp4", "canvas.mp4"] {
            let path = folder.join(name);
            if !path.exists() {
                println!("{name}: not saved");
                continue;
            }
            let frames = play(path);
            let mut count = 0;
            let mut last = Duration::ZERO;
            while let Ok(frame) = frames.recv_timeout(Duration::from_secs(5)) {
                assert!(count == 0 || frame.at > last, "{name}");
                if count == 0 {
                    println!("{name}: {}x{}", frame.pixels.width(), frame.pixels.height());
                }
                last = frame.at;
                count += 1;
                if last > Duration::from_secs(40) {
                    break;
                }
            }
            println!("{name}: {count} frames over {last:?}");
            assert!(last > Duration::from_secs(40), "{name} stopped");
        }
    }
}
