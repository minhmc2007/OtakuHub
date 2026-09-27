//! Hardware accelerated transcoding, built on libav through ffmpeg next. Uploading through the
//! encoder's own frames context keeps the backend out of the pipeline.

use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ffmpeg::ffi::*;
use ffmpeg::{codec, error, format, software, util::frame, util::format::Pixel, Rational};

use crate::error::{AppError, AppResult};
use crate::media::{probe, Codec};

/// The CDN refuses requests without a browser agent, and the stream host checks the
/// embed origin, so the demuxer carries both.
pub const AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

/// Segment length of the local HLS output. Long enough to be efficient, short enough
/// that the player has something to show within a few seconds.
const SEGMENT_SECS: &str = "4";

/// How often the encoder is drained, in frames. Bounded so a slow muxer cannot build up
/// an unbounded queue of packets in memory.
const DRAIN_EVERY: u64 = 10;

/// Packets taken from the encoder per drain.
const DRAIN_MAX: usize = 256;

fn ff_err(e: error::Error) -> AppError {
    AppError::internal(format!("ffmpeg: {e}"))
}

/// The same, but naming the step. libav's error messages are terse, so without this a
/// failure says "Invalid argument" and nothing else.
fn ff_at(stage: &str, e: error::Error) -> AppError {
    AppError::internal(format!("ffmpeg while {stage}: {e}"))
}

/// Same, for the steps that already carry their own message. The inner message is kept
/// verbatim, since a stage name plus a bare errno is no more useful than the errno.
fn at(stage: &str, e: AppError) -> AppError {
    AppError::internal(format!("{stage}: {e}"))
}

/// A video encoder plus the hardware device and frame pool it borrows.
pub struct VideoEncoder {
    opened: codec::encoder::video::Encoder,
    /// Kept alive because the encoder context only borrows a reference to the device.
    _device: Option<Box<AVBufferRef>>,
    /// The pool that upload surfaces are drawn from.
    frames: Option<Box<AVBufferRef>>,
    is_hardware: bool,
}

impl VideoEncoder {
    /// Build an encoder for `width` x `height`. A hardware backend gets a device context
    /// and a frames context, both created here.
    pub fn open(
        backend: probe::Encoder,
        codec_id: Codec,
        width: u32,
        height: u32,
        time_base: Rational,
        device_hint: &str,
    ) -> AppResult<Self> {
        let name = backend.encoder_name(codec_id);
        let found = codec::encoder::find_by_name(name)
            .ok_or_else(|| AppError::internal(format!("no encoder named {name}")))?;

        // `Context::new_with_codec(..).encoder().video()` is the encoder builder, which
        // is a different type from the decoder's `Codec::video()`.
        let mut builder = codec::Context::new_with_codec(found)
            .encoder()
            .video()
            .map_err(ff_err)?;
        // The setters return unit, so each one is its own statement.
        builder.set_width(width);
        builder.set_height(height);
        builder.set_time_base(time_base);
        builder.set_bit_rate(target_bitrate(width, height, codec_id));
        // Two seconds of GOP keeps seeking responsive without inflating the file much.
        builder.set_gop((time_base.denominator() * 2).max(1) as u32);

        let mut device: Option<Box<AVBufferRef>> = None;
        let mut frames: Option<Box<AVBufferRef>> = None;
        if backend.is_hardware() {
            let (dev, pool) = unsafe { device_and_frames(backend, width, height, device_hint)? };
            unsafe {
                let ctx = builder.as_mut_ptr();
                (*ctx).pix_fmt = backend.hw_pixel_format();
                (*ctx).hw_device_ctx = av_buffer_ref(&*dev as *const AVBufferRef);
                (*ctx).hw_frames_ctx = av_buffer_ref(&*pool as *const AVBufferRef);
            }
            device = Some(dev);
            frames = Some(pool);
        } else {
            builder.set_format(Pixel::YUV420P);
        }

        let opened = builder.open().map_err(ff_err)?;
        Ok(Self {
            opened,
            _device: device,
            frames,
            is_hardware: backend.is_hardware(),
        })
    }

    /// The opened encoder, so the muxer can copy the real codec parameters out of it.
    pub fn context(&self) -> &codec::Context {
        self.opened.as_ref()
    }

    pub fn is_hardware(&self) -> bool {
        self.is_hardware
    }

    /// The pixel format the encoder was opened with, which is what the scaler must produce.
    pub fn pixel_format(&self) -> Pixel {
        if self.is_hardware {
            Pixel::NV12
        } else {
            Pixel::YUV420P
        }
    }

    /// Encode one software frame, uploading first when the backend is hardware, so
    /// callers never need to know which kind of frame they are holding.
    pub fn send(&mut self, sw: &frame::Video) -> AppResult<()> {
        let Some(pool) = self.frames.as_ref() else {
            return self.opened.send_frame(sw).map_err(ff_err);
        };
        let mut hw = frame::Video::empty();
        unsafe {
            av_hwframe_get_buffer(&**pool as *const AVBufferRef as *mut AVBufferRef, hw.as_mut_ptr(), 0);
            if av_hwframe_transfer_data(hw.as_mut_ptr(), sw.as_ptr(), 0) < 0 {
                return Err(AppError::internal("could not upload a frame to the encoder"));
            }
        }
        // A transfer moves pixels only, so the timing has to be carried over by hand.
        carry_time(sw, &mut hw);
        self.opened.send_frame(&hw).map_err(ff_err)
    }

    pub fn flush_eof(&mut self) -> AppResult<()> {
        self.opened.send_eof().map_err(ff_err)
    }

    /// Take up to `max` packets the encoder has ready. Fewer than `max` simply means the
    /// encoder is still working, which is normal.
    pub fn take_packets(&mut self, max: usize) -> Vec<ffmpeg::Packet> {
        let mut out = Vec::new();
        let mut packet = ffmpeg::Packet::empty();
        while out.len() < max {
            match self.opened.receive_packet(&mut packet) {
                Ok(()) => out.push(std::mem::replace(&mut packet, ffmpeg::Packet::empty())),
                // EAGAIN and EOF both mean "nothing more right now".
                Err(_) => break,
            }
        }
        out
    }
}

/// Rough bitrate ceiling in bits per second. Anime sources are already compressed, so this is
/// a cap and not a target.
pub fn target_bitrate(width: u32, height: u32, codec_id: Codec) -> usize {
    let pixels = width as f64 * height as f64;
    let per_pixel = match codec_id {
        Codec::H264 => 0.09,
        // HEVC needs roughly half the rate for the same picture.
        Codec::H265 => 0.05,
    };
    (pixels * per_pixel).max(120_000.0) as usize
}

/// Create the device and the frame pool that surfaces are drawn from.
unsafe fn device_and_frames(
    backend: probe::Encoder,
    width: u32,
    height: u32,
    device_hint: &str,
) -> AppResult<(Box<AVBufferRef>, Box<AVBufferRef>)> {
    let device_type = backend
        .hw_device_type()
        .ok_or_else(|| AppError::internal("a hardware encoder needs a device type"))?;
    // The CString has to outlive the call, since libav reads the string during device creation.
    let hint = CString::new(device_hint).ok();
    let hint_ptr = match hint.as_ref() {
        Some(h) if !device_hint.is_empty() => h.as_ptr(),
        _ => ptr::null(),
    };
    let mut dev: *mut AVBufferRef = ptr::null_mut();
    if av_hwdevice_ctx_create(&mut dev, device_type, hint_ptr, ptr::null_mut(), 0) < 0 || dev.is_null() {
        let where_ = if device_hint.is_empty() { "any" } else { device_hint };
        return Err(AppError::internal(format!(
            "no {} device at {where_}",
            backend.label()
        )));
    }
    let device = Box::from_raw(dev);

    let pool = av_hwframe_ctx_alloc(dev);
    if pool.is_null() {
        return Err(AppError::internal("could not allocate a frames context"));
    }
    {
        let hwfc = (*pool).data as *mut AVHWFramesContext;
        (*hwfc).format = backend.hw_pixel_format();
        // Uploads always come from NV12: every backend here accepts it.
        (*hwfc).sw_format = AVPixelFormat::AV_PIX_FMT_NV12;
        (*hwfc).width = width as i32;
        (*hwfc).height = height as i32;
        (*hwfc).initial_pool_size = 8;
    }
    if av_hwframe_ctx_init(pool) < 0 {
        av_buffer_unref(&mut { pool });
        return Err(AppError::internal(format!(
            "{} rejected a {width}x{height} surface",
            backend.label()
        )));
    }
    Ok((device, Box::from_raw(pool)))
}

/// The encode side of a run: encoder, scaler, muxer and progress, kept together
/// because they advance together and have to be handed to one method at a time.
struct Pipeline {
    encoder: VideoEncoder,
    scaler: Scaler,
    out: Output,
    video_index: usize,
    time_base: Rational,
    progress: Progress,
    /// Encoded packets seen and how many of those were written. Used to notice an encoder
    /// that is producing nothing usable instead of grinding through the whole episode.
    offered: u32,
    written: u32,
}

impl Pipeline {
    /// Take every frame the decoder is holding, rescale it and encode it.
    /// Returns how many frames went through.
    fn drain_frames(
        &mut self,
        decoder: &mut codec::decoder::video::Video,
        on_progress: &mut dyn FnMut(Progress),
    ) -> AppResult<usize> {
        let mut count = 0;
        let mut decoded = frame::Video::empty();
        while decoder.receive_frame(&mut decoded).is_ok() {
            if decoded.width() == 0 || decoded.height() == 0 {
                continue;
            }
            let scaled = self.scaler.run(&decoded)?;
            self.encoder.send(&scaled)?;
            count += 1;
            self.progress.frames += 1;
            if self.progress.frames % DRAIN_EVERY == 0 {
                self.flush_encoded()?;
                on_progress(self.progress);
            }
        }
        Ok(count)
    }

    fn flush_encoded(&mut self) -> AppResult<()> {
        for mut packet in self.encoder.take_packets(DRAIN_MAX) {
            self.offered += 1;
            // A hardware encoder can hand back packets with uninitialised timestamps, on the
            // order of 1e15, which no muxer will take. The check is only worth it on hardware.
            if self.encoder.is_hardware {
                if let Some(dts) = packet.dts() {
                    if dts.abs() > 1_000_000_000_000 {
                        if self.offered > 64 && self.written == 0 {
                            return Err(AppError::internal(
                                "this hardware encoder produced no usable frames",
                            ));
                        }
                        continue;
                    }
                }
            }
            self.out
                .write_encoded(&mut packet, self.video_index, self.time_base)?;
            self.written += 1;
        }
        Ok(())
    }
}

/// Is this error libav's "try again later"? A full decoder or encoder says so, and it is a
/// normal part of the cycle.
fn is_again(e: &error::Error) -> bool {
    // libav signals its errors as negated errno values, so AVERROR(EAGAIN) is EAGAIN negated.
    const AVERROR_EAGAIN: i32 = -libc_eagain();
    matches!(e, error::Error::Other { errno } if *errno == AVERROR_EAGAIN)
}

const fn libc_eagain() -> i32 {
    11
}

/// Everything one transcode run needs.
#[derive(Debug, Clone)]
pub struct Request {
    pub input_url: String,
    pub referer: Option<String>,
    pub out_dir: PathBuf,
    pub backend: probe::Encoder,
    /// Device node or index for the hardware context. `None` lets libav pick.
    pub device: Option<PathBuf>,
    pub codec: Codec,
    pub max_height: u32,
    /// Stop after this many seconds of source and close the playlist, so a long
    /// episode can be cut to a sample. `None` runs to the end.
    pub limit_seconds: Option<u64>,
}

/// How far along a run is, for the UI to show.
#[derive(Debug, Clone, Copy, PartialEq, Default, serde::Serialize)]
pub struct Progress {
    pub frames: u64,
    pub out_height: u32,
    pub source_seconds: f64,
    pub done_seconds: f64,
}

impl Progress {
    /// Rough completion against the source duration, which the demuxer reports.
    pub fn percent(&self) -> Option<u8> {
        if self.source_seconds <= 0.0 || self.done_seconds <= 0.0 {
            return None;
        }
        Some(
            (self.done_seconds / self.source_seconds * 100.0)
                .clamp(0.0, 100.0)
                .round() as u8,
        )
    }
}

/// Run one transcode to completion. `cancel` is polled from libav interrupt callback, so a
/// stop request lands within a network round trip.
pub fn run(
    req: &Request,
    cancel: &Arc<AtomicBool>,
    on_progress: &mut dyn FnMut(Progress),
) -> AppResult<()> {
    ffmpeg::init().map_err(ff_err)?;
    std::fs::create_dir_all(&req.out_dir)?;

    let mut input = open_input(&req.input_url, req.referer.as_deref(), Arc::clone(cancel))?;

    // The muxer header is written before any frame is decoded, so the size comes from the parameters.
    let (video_in, (src_w, src_h)) = input
        .streams()
        .best(ffmpeg::media::Type::Video)
        .map(|s| unsafe { (s.index(), video_size(&s)) })
        .ok_or_else(|| AppError::upstream("stream", "the source has no video track"))?;
    if src_w == 0 || src_h == 0 {
        return Err(AppError::upstream("stream", "the source reported no video size"));
    }
    let source_rate = input
        .streams()
        .find(|s| s.index() == video_in)
        .map(|s| s.rate())
        .unwrap_or(Rational::new(24, 1));
    let fps = frame_rate(source_rate);
    let enc_tb = Rational::new(1, fps);
    let (out_w, out_h) = fit(src_w, src_h, req.max_height);

    let mut decoder = open_decoder(&mut input, video_in)?;
    let source_seconds = source_duration(&input, video_in);
    let source_tb = video_time_base(&input, video_in);

    let encoder = VideoEncoder::open(
        req.backend,
        req.codec,
        out_w,
        out_h,
        enc_tb,
        &req.backend.device_hint(&req.device),
    )
    .map_err(|e| at("opening the encoder", e))?;
    let out = Output::open(&req.out_dir, encoder.context(), enc_tb, &mut input)
        .map_err(|e| at("opening the output", e))?;
    let mut pipe = Pipeline {
        progress: Progress {
            out_height: out_h,
            source_seconds,
            ..Default::default()
        },
        video_index: out.video_index(),
        scaler: Scaler::new(out_w, out_h, encoder.pixel_format()),
        time_base: enc_tb,
        offered: 0,
        written: 0,
        encoder,
        out,
    };

    let limit = req.limit_seconds.map(|s| s as f64);
    let mut cut_early = false;

    for (_stream, mut packet) in input.packets() {
        if cancel.load(Ordering::Relaxed) {
            return Err(AppError::internal("cancelled"));
        }
        if let Some(limit) = limit {
            if let Some(ts) = packet.pts().or_else(|| packet.dts()) {
                if to_seconds(ts, source_tb) >= limit {
                    cut_early = true;
                    break;
                }
            }
        }
        if packet.stream() != video_in {
            pipe.out
                .write_copied(&mut packet)
                .map_err(|e| at("remuxing audio", e))?;
            continue;
        }

        // A decoder holding reordering buffers answers EAGAIN until it is drained.
        let mut pending = Some(packet);
        let mut attempts = 0;
        while let Some(p) = pending.take() {
            match decoder.send_packet(&p) {
                Ok(()) => {}
                Err(e) if is_again(&e) => {
                    attempts += 1;
                    if attempts > MAX_DRAIN_ATTEMPTS {
                        // Dropping the packet avoids the spin: one lost frame beats a stalled job.
                        tracing::debug!("dropping a packet the decoder would not take");
                        break;
                    }
                    if pipe.drain_frames(&mut decoder, on_progress)? == 0 {
                        break;
                    }
                    pending = Some(p);
                }
                Err(e) => return Err(ff_at("decoding", e)),
            }
        }
        pipe.drain_frames(&mut decoder, on_progress)
        .map_err(|e| at("draining frames", e))?;
        pipe.flush_encoded().map_err(|e| at("writing the tail", e))?;
    }

    // Drain the decoder so the tail of the episode is not dropped, then the encoder.
    decoder.send_eof().map_err(|e| ff_at("flushing the decoder", e))?;
    pipe.drain_frames(&mut decoder, on_progress)
        .map_err(|e| at("draining frames", e))?;
    pipe.encoder.flush_eof().map_err(|e| at("flushing the encoder", e))?;
    pipe.flush_encoded().map_err(|e| at("writing the tail", e))?;
    pipe.out.finish().map_err(|e| at("closing the playlist", e))?;

    // A truncated run is still cached, since the trailer closed the playlist.
    pipe.progress.done_seconds = limit.unwrap_or(source_seconds);
    on_progress(pipe.progress);
    tracing::info!(
        frames = pipe.progress.frames,
        height = out_h,
        cut_early,
        encoder = req.backend.encoder_name(req.codec),
        "transcode finished"
    );
    Ok(())
}

/// How many times to retry a full decoder before giving up on a packet.
const MAX_DRAIN_ATTEMPTS: u32 = 4;

/// Read the coded size off a stream. `Stream` has no width accessor, so this goes to
/// the raw parameters.
unsafe fn video_size(stream: &format::stream::Stream<'_>) -> (u32, u32) {
    let par = stream.parameters();
    let p = par.as_ptr();
    if p.is_null() {
        return (0, 0);
    }
    ((*p).width as u32, (*p).height as u32)
}

/// Container duration converted to seconds through the video time base.
fn source_duration(input: &format::context::Input, video_in: usize) -> f64 {
    to_seconds(input.duration(), video_time_base(input, video_in))
}

/// A HLS demuxer can report `0/1` for a stream it has not seen in progress yet, and a zero
/// time base makes every later write fail with EINVAL, so a degenerate one is replaced.
fn usable_time_base(tb: Rational) -> ffmpeg::ffi::AVRational {
    if tb.numerator() == 0 || tb.denominator() == 0 {
        ffmpeg::ffi::AVRational {
            num: 1,
            den: 90_000,
        }
    } else {
        tb.into()
    }
}

fn video_time_base(input: &format::context::Input, video_in: usize) -> Rational {
    input
        .streams()
        .find(|s| s.index() == video_in)
        .map(|s| s.time_base())
        .unwrap_or_else(|| Rational::new(1, 1_000_000))
}

fn to_seconds(ticks: i64, tb: Rational) -> f64 {
    ticks as f64 * tb.numerator() as f64 / tb.denominator().max(1) as f64
}

fn open_input(
    url: &str,
    referer: Option<&str>,
    cancel: Arc<AtomicBool>,
) -> AppResult<format::context::Input> {
    let mut opts = ffmpeg::Dictionary::new();
    opts.set("user_agent", AGENT);
    if let Some(headers) = http_headers(referer) {
        opts.set("headers", &headers);
    }
    format::input_with_interrupt_and_dictionary(url, move || cancel.load(Ordering::Relaxed), opts)
        .map_err(|e| AppError::upstream("stream", format!("cannot open the source: {e}")))
}

/// `Dictionary::set` panics on an interior NUL, and the referer comes from an upstream page,
/// so anything outside a printable ASCII range is dropped.
fn http_headers(referer: Option<&str>) -> Option<String> {
    let referer = referer?;
    let referer: String = referer
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .take(512)
        .collect();
    if referer.trim().is_empty() {
        return None;
    }
    // No trailing newline: libav appends its own separator, and a blank line would end
    // the header block early.
    Some(format!("Referer: {}", referer.trim()))
}

fn open_decoder(
    input: &mut format::context::Input,
    video_in: usize,
) -> AppResult<codec::decoder::video::Video> {
    let params = input
        .streams()
        .find(|s| s.index() == video_in)
        .map(|s| s.parameters())
        .ok_or_else(|| AppError::upstream("stream", "the video stream disappeared"))?;
    // `Decoder::video()` opens with the codec the parameters name and tags the context
    // as video, which a bare `open()` does not.
    codec::Context::from_parameters(params)
        .map_err(ff_err)?
        .decoder()
        .video()
        .map_err(|e| AppError::upstream("stream", format!("no decoder for this source: {e}")))
}

/// Scale `w` x `h` down so the height fits `max_height`. Both sides are rounded to even
/// numbers because every hardware encoder here requires it.
pub fn fit(w: u32, h: u32, max_height: u32) -> (u32, u32) {
    if max_height == 0 || h <= max_height {
        return (even(w), even(h));
    }
    let ratio = max_height as f64 / h as f64;
    (even((w as f64 * ratio) as u32).max(2), even(max_height))
}

fn even(v: u32) -> u32 {
    if v % 2 == 0 { v } else { v.saturating_sub(1).max(2) }
}

fn frame_rate(r: Rational) -> i32 {
    let fps = r.numerator() as f64 / r.denominator().max(1) as f64;
    fps.round().clamp(1.0, 240.0) as i32
}

/// A cached swscale context. An episode keeps one resolution, so one context is enough
/// and the conversion cost is paid once.
struct Scaler {
    ctx: software::scaling::Context,
    target: (u32, u32),
    /// Taken from the opened encoder, since a fixed NV12 here killed the software fallback on frame 0.
    target_format: Pixel,
}

impl Scaler {
    fn new(width: u32, height: u32, target_format: Pixel) -> Self {
        Self {
            ctx: software::scaling::Context::get(
                Pixel::YUV420P,
                width.max(2),
                height.max(2),
                target_format,
                width.max(2),
                height.max(2),
                software::scaling::Flags::BICUBIC,
            )
            .expect("a scaler for any two pixel formats"),
            target: (width, height),
            target_format,
        }
    }

    fn run(&mut self, src: &frame::Video) -> AppResult<frame::Video> {
        let (w, h) = self.target;
        let source = (src.width(), src.height());
        if self.ctx.input().width != source.0
            || self.ctx.input().height != source.1
            || self.ctx.input().format != src.format()
        {
            self.ctx = software::scaling::Context::get(
                src.format(),
                source.0,
                source.1,
                self.target_format,
                w,
                h,
                software::scaling::Flags::BICUBIC,
            )
            .map_err(|e| AppError::internal(format!("cannot scale the source: {e}")))?;
        }
        let mut dst = frame::Video::new(self.target_format, w, h);
        // sws_scale_frame moves pixels and nothing else, so without carrying the timing across
        // by hand the output is a single unreadable segment.
        carry_time(src, &mut dst);
        self.ctx.run(src, &mut dst).map_err(ff_err)?;
        Ok(dst)
    }
}

/// The wrapper only exposes `pts`, so `pkt_dts`, `duration` and the time base go through the
/// raw frame.
fn carry_time(from: &frame::Video, to: &mut frame::Video) {
    to.set_pts(from.pts());
    unsafe {
        let src = from.as_ptr();
        let dst = to.as_mut_ptr();
        (*dst).pkt_dts = (*src).pkt_dts;
        (*dst).duration = (*src).duration;
        (*dst).time_base = (*src).time_base;
    }
}

/// A muxer refuses a packet whose timestamp does not advance, and these transport streams
/// emit duplicates around a discontinuity, so a duplicate is nudged forward one tick.
#[derive(Default)]
struct Monotonic {
    /// None until the first packet, since a sentinel reads a stream starting at dts 0 as "nothing seen yet".
    last: Option<i64>,
}

impl Monotonic {
    /// Returns true when the packet was left as it was.
    fn accept(&mut self, packet: &mut ffmpeg::Packet) -> bool {
        let Some(dts) = packet.dts() else {
            return true;
        };
        match self.last {
            None => {
                self.last = Some(dts);
                true
            }
            Some(last) if dts > last => {
                self.last = Some(dts);
                true
            }
            Some(last) => {
                // Shift both equally, so pts and dts keep their distance apart.
                let bumped = last + 1;
                if let Some(pts) = packet.pts() {
                    packet.set_pts(Some(pts + (bumped - dts)));
                }
                packet.set_dts(Some(bumped));
                self.last = Some(bumped);
                false
            }
        }
    }
}

/// The local HLS output: one video stream we encode into, plus the audio track copied
/// across untouched. Wrapped in a struct so the muxer options live next to the writer.
struct Output {
    ctx: format::context::Output,
    video_index: usize,
    time_base: Rational,
    seen: Monotonic,
    audio_seen: Monotonic,
    /// Input index to output index. The source carries a timed ID3 track as well as audio,
    /// and routing that into the audio output makes the muxer refuse the packet.
    audio: Option<(usize, usize)>,
    dir: PathBuf,
}

/// The line that tells a player a playlist is complete.
const ENDLIST_TAG: &str = "#EXT-X-ENDLIST";

impl Output {
    /// Declare both output streams, copy the real codec parameters out of the opened
    /// encoder, then write the container header.
    fn open(
        dir: &Path,
        encoder: &codec::Context,
        time_base: Rational,
        input: &mut format::context::Input,
    ) -> AppResult<Self> {
        let dir = dir.to_path_buf();
        let playlist = dir.join("index.m3u8");
        let mut ctx = format::output_as(&playlist, "hls").map_err(ff_err)?;

        // The video stream carries the opened encoder's own parameters, so the muxer
        // knows the real codec, dimensions and frame rate.
        let video_index = {
            let found = encoder.codec().ok_or_else(|| {
                AppError::internal("the encoder did not report a codec for the muxer")
            })?;
            let mut stream = ctx.add_stream(found).map_err(ff_err)?;
            // `set_parameters` converts from a codec context by value, so the parameters are
            // copied out of the encoder first.
            let mut copied = codec::Parameters::new();
            unsafe {
                avcodec_parameters_from_context(copied.as_mut_ptr(), encoder.as_ptr());
            }
            stream.set_parameters(copied);
            stream.set_time_base(time_base);
            stream.index()
        };

        // Audio is remuxed, never re encoded: source AAC plays everywhere already.
        let audio = match input.streams().best(ffmpeg::media::Type::Audio) {
            Some(source) => {
                let params = source.parameters();
                let in_tb = usable_time_base(source.time_base());
                // A stream created with no codec is how libav spells "copy this one".
                let mut stream = ctx.add_stream(None::<ffmpeg::Codec>).map_err(ff_err)?;
                stream.set_parameters(params);
                stream.set_time_base(Rational::new(in_tb.num, in_tb.den));
                // A copied codec tag can name a container the muxer will not write.
                unsafe {
                    (*stream.parameters().as_mut_ptr()).codec_tag = 0;
                }
                Some((source.index(), stream.index()))
            }
            None => None,
        };

        let mut opts = ffmpeg::Dictionary::new();
        opts.set("hls_segment_filename", &segment_pattern(&dir));
        // An unbounded list, so a finished job replays from segment zero.
        opts.set("hls_list_size", "0");
        opts.set("hls_allow_cache", "1");
        opts.set("start_number", "0");
        opts.set("hls_time", SEGMENT_SECS);
        // No ENDLIST until the job finishes, which is how the player knows to keep polling.
        opts.set("hls_flags", "omit_endlist");
        ctx.write_header_with(opts).map_err(ff_err)?;
        Ok(Self {
            ctx,
            video_index,
            time_base,
            audio,
            dir,
            seen: Monotonic::default(),
            audio_seen: Monotonic::default(),
        })
    }

    /// Write everything the encoder has ready.
    fn write_encoded(&mut self, packet: &mut ffmpeg::Packet, index: usize, from: Rational) -> AppResult<()> {
        if packet.size() == 0 || (packet.pts().is_none() && packet.dts().is_none()) {
            tracing::debug!(bytes = packet.size(), "dropping an empty encoded packet");
            return Ok(());
        }
        packet.set_stream(index);
        packet.rescale_ts(from, self.time_base);
        packet.set_position(-1);
        self.seen.accept(packet);
        // Snapshotted before the write: `write_interleaved` consumes the packet, so
        // reading it afterwards reports zeros and hides the real values.
        let seen = (
            packet.stream(),
            packet.dts().unwrap_or(i64::MIN),
            packet.pts().unwrap_or(i64::MIN),
            packet.duration(),
            packet.size(),
        );
        packet
            .write_interleaved(&mut self.ctx)
            .map_err(|e| {
                tracing::error!(
                    stream = seen.0,
                    dts = seen.1,
                    pts = seen.2,
                    duration = seen.3,
                    bytes = seen.4,
                    "the muxer refused an encoded packet"
                );
                ff_at("muxing a packet", e)
            })?;
        Ok(())
    }

    /// Copy a source packet straight through, if it belongs to the audio track we mapped.
    fn write_copied(&mut self, packet: &mut ffmpeg::Packet) -> AppResult<()> {
        // Mutated in place, since a clone of these packets resolves to no payload.
        let Some((input_index, output_index)) = self.audio else {
            return Ok(());
        };
        if packet.stream() != input_index {
            // A data or subtitle track, which has no place in this output.
            return Ok(());
        }
        // A zero length packet is refused with EINVAL, and `Packet::is_empty` only looks at the
        // data pointer, so the size is checked. A packet with no timestamp cannot be placed.
        if packet.size() == 0 {
            return Ok(());
        }
        if packet.pts().is_none() && packet.dts().is_none() {
            tracing::debug!(bytes = packet.size(), "dropping an audio packet with no timestamp");
            return Ok(());
        }
        packet.set_stream(output_index);
        packet.set_position(-1);
        self.audio_seen.accept(packet);
        let seen = (
            packet.stream(),
            packet.dts().unwrap_or(i64::MIN),
            packet.pts().unwrap_or(i64::MIN),
            packet.duration(),
            packet.size(),
        );
        packet
            .write_interleaved(&mut self.ctx)
            .map_err(|e| {
                tracing::error!(
                    stream = seen.0,
                    dts = seen.1,
                    pts = seen.2,
                    duration = seen.3,
                    bytes = seen.4,
                    "the muxer refused an audio packet"
                );
                ff_at("remuxing an audio packet", e)
            })
    }

    /// The output stream the encoded packets belong to.
    fn video_index(&self) -> usize {
        self.video_index
    }

    /// `omit_endlist` is read once at open, so the suppressed line is written here after the trailer.
    /// Without it a finished job looks live and is never cached.
    fn mark_complete(&self) -> std::io::Result<()> {
        let path = self.playlist();
        let mut body = std::fs::read_to_string(&path)?;
        if !body.contains(ENDLIST_TAG) {
            if !body.ends_with('\n') {
                body.push('\n');
            }
            body.push_str(ENDLIST_TAG);
            body.push('\n');
            // Written beside the playlist and renamed, so a reader never sees a half
            // written one.
            let tmp = path.with_extension("m3u8.part");
            std::fs::write(&tmp, body)?;
            std::fs::rename(&tmp, &path)?;
        }
        Ok(())
    }

    fn playlist(&self) -> PathBuf {
        self.dir.join("index.m3u8")
    }

    /// Close the playlist, then add the endlist tag the muxer suppressed.
    fn finish(&mut self) -> AppResult<()> {
        self.ctx.write_trailer().map_err(|e| ff_at("writing the trailer", e))?;
        self.mark_complete()?;
        Ok(())
    }
}

fn segment_pattern(dir: &Path) -> String {
    dir.join("seg%05d.ts").to_string_lossy().into_owned()
}

/// Height and width of a source, used to show the resolution before a transcode starts.
pub fn probe_size(url: &str, referer: Option<&str>) -> Option<(u32, u32)> {
    ffmpeg::init().ok()?;
    let mut opts = ffmpeg::Dictionary::new();
    opts.set("user_agent", AGENT);
    if let Some(headers) = http_headers(referer) {
        opts.set("headers", &headers);
    }
    let input = format::input_with_dictionary(url, opts).ok()?;
    let stream = input.streams().best(ffmpeg::media::Type::Video)?;
    let size = unsafe { video_size(&stream) };
    (size.0 > 0).then_some(size)
}

/// A flat grey frame in the requested format, stamped like a real one. A frame with no pts
/// makes the packet come back with no pts, which reads as an encoder that cannot timestamp.
pub fn synthetic_frame(w: u32, h: u32, format: Pixel) -> frame::Video {
    let mut f = frame::Video::new(format, w, h);
    for plane in 0..f.planes() {
        for b in f.data_mut(plane) {
            *b = 128;
        }
    }
    f.set_pts(Some(0));
    unsafe {
        let p = f.as_mut_ptr();
        (*p).pkt_dts = 0;
        (*p).duration = 1;
    }
    f
}

#[cfg(test)]
mod tests {
    #[test]
    fn scaling_carries_the_timestamps_across() {
        // Guards the case where the scaler drops the pts, which made the whole conversion
        // produce one unreadable segment.
        use super::Scaler;
        let mut scaler = Scaler::new(64, 64, ffmpeg::format::Pixel::YUV420P);

        let mut src = ffmpeg::frame::video::Video::new(
            ffmpeg::format::Pixel::YUV420P,
            64,
            64,
        );
        src.set_pts(Some(9001));
        unsafe {
            let p = src.as_mut_ptr();
            (*p).pkt_dts = 8999;
            (*p).duration = 512;
        }

        let out = scaler.run(&src).expect("scaling must work");
        assert_eq!(out.pts(), Some(9001), "the pts was lost in the scaler");
        unsafe {
            let p = out.as_ptr();
            assert_eq!((*p).pkt_dts, 8999, "the dts was lost in the scaler");
            assert_eq!((*p).duration, 512, "the duration was lost");
        }
    }

    #[test]
    fn the_scaler_targets_the_encoders_pixel_format() {
        // A fixed NV12 output against a YUV420P software encoder is what made the fallback
        // die on frame 0 with AVERROR_EXTERNAL.
        use super::Scaler;
        for (format, name) in [
            (ffmpeg::format::Pixel::YUV420P, "software"),
            (ffmpeg::format::Pixel::NV12, "hardware"),
        ] {
            let mut scaler = Scaler::new(32, 32, format);
            let mut src = ffmpeg::frame::video::Video::new(format, 32, 32);
            src.set_pts(Some(0));
            let out = scaler.run(&src).expect("scaling must work");
            assert_eq!(out.format(), format, "{name} scaler changed the format");
        }
    }

    #[test]
    fn a_stream_starting_at_zero_stays_strictly_increasing() {
        use super::Monotonic;
        let mut seen = Monotonic::default();
        // A `last == 0` sentinel read the first packet as "nothing seen yet", so every
        // later 0 was accepted and the muxer refused the batch with EINVAL.
        for dts in [0, 0, 0, 512, 512, 1024] {
            let mut p = ffmpeg::Packet::empty();
            p.set_dts(Some(dts));
            p.set_pts(Some(dts));
            seen.accept(&mut p);
        }
        // Replay the sequence and assert the invariant the muxer actually cares about.
        let mut seen = Monotonic::default();
        let mut last = i64::MIN;
        for dts in [0, 0, 0, 512, 512, 1024] {
            let mut p = ffmpeg::Packet::empty();
            p.set_dts(Some(dts));
            p.set_pts(Some(dts));
            seen.accept(&mut p);
            let out = p.dts().unwrap();
            assert!(out > last, "{out} did not advance past {last}");
            last = out;
        }
    }

    #[test]
    fn nudging_a_duplicate_keeps_pts_and_dts_the_same_distance_apart() {
        use super::Monotonic;
        let mut seen = Monotonic::default();
        let mut first = ffmpeg::Packet::empty();
        first.set_dts(Some(1000));
        first.set_pts(Some(1040));
        seen.accept(&mut first);

        let mut dup = ffmpeg::Packet::empty();
        dup.set_dts(Some(1000));
        dup.set_pts(Some(1040));
        assert!(!seen.accept(&mut dup), "a duplicate must be reported as changed");
        let gap = dup.pts().unwrap() - dup.dts().unwrap();
        assert_eq!(gap, 40, "the pts to dts distance was not preserved");
    }

    #[test]
    fn a_real_gap_is_left_alone() {
        use super::Monotonic;
        let mut seen = Monotonic::default();
        let mut a = ffmpeg::Packet::empty();
        a.set_dts(Some(0));
        a.set_pts(Some(0));
        assert!(seen.accept(&mut a));

        let mut b = ffmpeg::Packet::empty();
        b.set_dts(Some(90_000));
        b.set_pts(Some(90_000));
        assert!(seen.accept(&mut b), "a forward jump is not a duplicate");
        assert_eq!(b.dts(), Some(90_000));
    }

    use super::*;

    #[test]
    fn fit_only_ever_shrinks() {
        assert_eq!(fit(1920, 1080, 0), (1920, 1080));
        assert_eq!(fit(1920, 1080, 1080), (1920, 1080));
        assert_eq!(fit(1920, 1080, 720), (1280, 720));
    }

    #[test]
    fn fit_returns_even_numbers() {
        for target in [240, 360, 480, 720, 1080, 1440, 2160] {
            let (w, h) = fit(1921, 1081, target);
            assert_eq!(w % 2, 0, "width must be even for {target}");
            assert_eq!(h % 2, 0, "height must be even for {target}");
            assert!(w >= 2 && h >= 2);
        }
    }

    #[test]
    fn a_small_source_is_never_stretched() {
        assert_eq!(fit(854, 480, 1080), (854, 480));
    }

    #[test]
    fn a_very_short_source_keeps_a_usable_width() {
        let (w, h) = fit(4, 2, 720);
        assert!(w >= 2 && h >= 2);
    }

    #[test]
    fn bitrate_ceiling_tracks_pixels_and_codec() {
        assert!(target_bitrate(1920, 1080, Codec::H265) < target_bitrate(1920, 1080, Codec::H264));
        assert!(target_bitrate(320, 180, Codec::H264) > 0, "never below a floor");
    }

    #[test]
    fn frame_rate_rounding_clamps() {
        assert_eq!(frame_rate(Rational::new(24, 1)), 24);
        assert_eq!(frame_rate(Rational::new(24000, 1001)), 24);
        assert_eq!(frame_rate(Rational::new(0, 1)), 1);
        assert_eq!(frame_rate(Rational::new(1000, 1)), 240);
    }

    #[test]
    fn progress_percent_is_clamped_and_optional() {
        let p = Progress {
            frames: 10,
            out_height: 720,
            source_seconds: 100.0,
            done_seconds: 50.0,
        };
        assert_eq!(p.percent(), Some(50));
        assert_eq!(Progress { done_seconds: 500.0, source_seconds: 100.0, ..p }.percent(), Some(100));
        assert_eq!(Progress::default().percent(), None);
    }

    #[test]
    fn referer_headers_are_sanitised() {
        assert_eq!(
            http_headers(Some("https://zokoanime.video/")).as_deref(),
            Some("Referer: https://zokoanime.video/")
        );
        // An interior NUL would panic inside ffmpeg's dictionary, so it is dropped.
        assert_eq!(http_headers(Some("https://a.test/\0evil")).as_deref(), Some("Referer: https://a.test/evil"));
        assert_eq!(http_headers(Some("   ")), None);
        assert_eq!(http_headers(None), None);
    }

    #[test]
    fn segment_pattern_uses_the_output_directory() {
        assert!(segment_pattern(Path::new("/tmp/cache/abc")).ends_with("/tmp/cache/abc/seg%05d.ts"));
    }
}
