use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Result};
use image::DynamicImage;

use ffmpeg_next as ffmpeg;
use ffmpeg::format::pixel::Pixel;
use ffmpeg::software::scaling::{context::Context as Scaler, flag::Flags};
use ffmpeg::util::color::Range;

pub fn init() -> Result<()> {
    ffmpeg::init().map_err(|e| anyhow::anyhow!("Failed to initialize ffmpeg: {}", e))?;
    ffmpeg::format::network::init(); 
    ffmpeg::log::set_level(ffmpeg::log::Level::Quiet);
    Ok(())
}

pub fn get_duration(path: &str) -> Result<f64> {
    let ictx = ffmpeg::format::input(&Path::new(path))?;
    let stream = ictx
        .streams()
        .best(ffmpeg::media::Type::Video)
        .ok_or_else(|| anyhow::anyhow!("No video stream found"))?;

    let duration = stream.duration() as f64;
    let time_base = f64::from(stream.time_base());

    if duration >= 0.0 {
        Ok(duration * time_base)
    } else {
        let ctx_duration = ictx.duration() as f64 / ffmpeg::ffi::AV_TIME_BASE as f64;
        Ok(ctx_duration)
    }
}

#[allow(dead_code)]
fn configure_scaler_range(scaler: &mut Scaler, decoded: &ffmpeg::frame::Video) {
    unsafe {
        let src_range = match decoded.color_range() {
            ffmpeg::color::Range::JPEG => 1,
            _ => 0,
        };

        let dst_range = 1;

        let table = ffmpeg::ffi::sws_getCoefficients(ffmpeg::ffi::SWS_CS_DEFAULT);

        ffmpeg::ffi::sws_setColorspaceDetails(
            scaler.as_mut_ptr(),
            table,
            src_range,
            table,
            dst_range,
            0,
            1 << 16,
            1 << 16,
        );
    }
}

#[allow(dead_code)]
fn configure_scaler_from_frame(
    scaler: &mut Scaler,
    decoded: &ffmpeg::frame::Video,
) -> Result<()> {
    unsafe {
        let src_range = match decoded.color_range() {
            ffmpeg::util::color::Range::JPEG => 1,
            _ => 0,
        };

        let dst_range = 1;
        let table = ffmpeg::ffi::sws_getCoefficients(ffmpeg::ffi::SWS_CS_DEFAULT);

        let ret = ffmpeg::ffi::sws_setColorspaceDetails(
            scaler.as_mut_ptr(),
            table,
            src_range,
            table,
            dst_range,
            0,
            1 << 16,
            1 << 16,
        );

        if ret < 0 {
            bail!("sws_setColorspaceDetails failed: {}", ret);
        }
    }

    Ok(())
}

fn is_probably_black(img: &DynamicImage) -> bool {
    let rgb = img.to_rgb8();
    let mut total = 0u64;
    let mut dark = 0u64;
    let mut sum = 0u64;

    for p in rgb.pixels().step_by(16) {
        let [r, g, b] = p.0;
        let y = (r as u64 * 54 + g as u64 * 183 + b as u64 * 19) / 256;
        sum += y;
        total += 1;
        if y <= 8 {
            dark += 1;
        }
    }

    total > 0 && (sum / total <= 6 || dark * 100 / total >= 98)
}

fn normalize_frame_for_rgb(decoded: &ffmpeg::frame::Video) -> ffmpeg::frame::Video {
    let mut frame = decoded.clone();

    match frame.format() {
        Pixel::YUVJ420P => {
            frame.set_format(Pixel::YUV420P);
            frame.set_color_range(Range::JPEG);
        }
        Pixel::YUVJ422P => {
            frame.set_format(Pixel::YUV422P);
            frame.set_color_range(Range::JPEG);
        }
        Pixel::YUVJ444P => {
            frame.set_format(Pixel::YUV444P);
            frame.set_color_range(Range::JPEG);
        }
        Pixel::YUVJ440P => {
            frame.set_format(Pixel::YUV440P);
            frame.set_color_range(Range::JPEG);
        }
        _ => {}
    }

    frame
}

fn rgb_frame_to_image(rgb_frame: &ffmpeg::frame::Video) -> DynamicImage {
    let width = rgb_frame.width();
    let height = rgb_frame.height();
    let data = rgb_frame.data(0);
    let stride = rgb_frame.stride(0);

    let mut img_buf = image::ImageBuffer::new(width, height);
    for y in 0..height {
        let src_start = y as usize * stride;
        for x in 0..width {
            let src_idx = src_start + x as usize * 3;
            img_buf.put_pixel(
                x,
                y,
                image::Rgb([data[src_idx], data[src_idx + 1], data[src_idx + 2]]),
            );
        }
    }

    DynamicImage::ImageRgb8(img_buf)
}

fn video_frame_to_image(decoded: &ffmpeg::frame::Video) -> Result<DynamicImage> {
    let src = normalize_frame_for_rgb(decoded);

    let mut converter = src.converter(Pixel::RGB24)?;
    let mut rgb_frame = ffmpeg::frame::Video::empty();
    converter.run(&src, &mut rgb_frame)?;

    Ok(rgb_frame_to_image(&rgb_frame))
}

pub fn extract_frame(path: &str, timestamp_sec: f64) -> Result<DynamicImage> {
    let mut ictx = ffmpeg::format::input(&Path::new(path))?;
    let stream = ictx
        .streams()
        .best(ffmpeg::media::Type::Video)
        .ok_or_else(|| anyhow::anyhow!("No video stream found"))?;

    let stream_idx = stream.index();
    let time_base_f = f64::from(stream.time_base());
    let target_pts = if time_base_f > 0.0 && time_base_f.is_finite() {
        (timestamp_sec.max(0.0) / time_base_f).round() as i64
    } else {
        0
    };

    let context = ffmpeg::codec::context::Context::from_parameters(stream.parameters())?;
    let mut decoder = context.decoder().video()?;

    if timestamp_sec >= 0.5 {
        let ts = (timestamp_sec * ffmpeg::ffi::AV_TIME_BASE as f64).round() as i64;
        let _ = ictx.seek(ts, ..);
        decoder.flush();
    }

    let mut decoded = ffmpeg::frame::Video::empty();
    let mut first_decodable: Option<DynamicImage> = None;
    let mut best_before: Option<DynamicImage> = None;
    
    let mut read_packets = 0usize;
    let mut decoded_frames = 0usize;

    for (s, packet) in ictx.packets() {
        if s.index() != stream_idx {
            continue;
        }

        read_packets += 1;
        decoder.send_packet(&packet)?;

        while decoder.receive_frame(&mut decoded).is_ok() {
            decoded_frames += 1;
            let img = video_frame_to_image(&decoded)?;

            if first_decodable.is_none() {
                first_decodable = Some(img.clone());
            }

            if timestamp_sec <= 0.5 {
                let frame_sec = decoded
                    .pts()
                    .map(|pts| pts as f64 * time_base_f)
                    .unwrap_or(0.0);

                // FIX 1: If PTS is missing, stop checking after 20 frames to prevent 100% CPU lockups
                if (frame_sec <= 1.0 && !is_probably_black(&img)) || decoded_frames > 20 {
                    return Ok(img);
                }

                best_before = Some(img);
                continue;
            }

            // FIX 2: Handle missing PTS after a physical seek
            match decoded.pts() {
                Some(frame_pts) if frame_pts >= target_pts => return Ok(img),
                None => return Ok(img), // PTS is missing. The demuxer seeked close enough, just take it!
                _ => best_before = Some(img),
            }
        }

        if read_packets > 1000 {
            break;
        }
    }

    decoder.send_eof()?;
    while decoder.receive_frame(&mut decoded).is_ok() {
        decoded_frames += 1;
        let img = video_frame_to_image(&decoded)?;

        if timestamp_sec <= 0.5 {
            let frame_sec = decoded
                .pts()
                .map(|pts| pts as f64 * time_base_f)
                .unwrap_or(0.0);

            if (frame_sec <= 1.0 && !is_probably_black(&img)) || decoded_frames > 20 {
                return Ok(img);
            }

            best_before = Some(img);
            continue;
        }

        match decoded.pts() {
            Some(frame_pts) if frame_pts >= target_pts => return Ok(img),
            None => return Ok(img),
            _ => best_before = Some(img),
        }
    }

    best_before
        .or(first_decodable)
        .ok_or_else(|| anyhow::anyhow!("ffmpeg produced no decodable video frame"))
}

pub fn spawn_frame_extractor(
    path: String,
    timestamps: Vec<f64>,
    frames: Arc<Mutex<Vec<Option<DynamicImage>>>>,
    ready: Arc<AtomicUsize>,
    done: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
) {
    let n_frames = timestamps.len();

    std::thread::spawn(move || {
        let mut ictx = match ffmpeg::format::input(&Path::new(&path)) {
            Ok(c) => c,
            Err(_) => {
                done.store(true, Ordering::Relaxed);
                return;
            }
        };

        let stream = match ictx.streams().best(ffmpeg::media::Type::Video) {
            Some(s) => s,
            None => {
                done.store(true, Ordering::Relaxed);
                return;
            }
        };

        let stream_idx = stream.index();
        let time_base_f = f64::from(stream.time_base());
        
        // Calculate a safe fallback frame duration (e.g. 1 / 25fps = 0.04s) for missing PTS
        let fps = f64::from(stream.avg_frame_rate());
        let fallback_dur = if fps > 0.0 && fps.is_finite() { 1.0 / fps } else { time_base_f };

        let context = match ffmpeg::codec::context::Context::from_parameters(stream.parameters()) {
            Ok(c) => c,
            Err(_) => {
                done.store(true, Ordering::Relaxed);
                return;
            }
        };

        let mut decoder = match context.decoder().video() {
            Ok(d) => d,
            Err(_) => {
                done.store(true, Ordering::Relaxed);
                return;
            }
        };

        let mut loaded: Vec<bool> = vec![false; n_frames];
        loaded[0] = true;
        let mut loaded_count = 1usize;

        let mut decoded = ffmpeg::frame::Video::empty();
        let mut rgb_frame = ffmpeg::frame::Video::empty();
        let mut scaler_opt: Option<Scaler> = None;
        let mut current_fmt = Pixel::None;
        let mut current_w = 0;
        let mut current_h = 0;

        let is_http = path.starts_with("http");

        if is_http {
            let mut current_idx = 1;
            let mut first_sec: Option<f64> = None;
            let mut frame_counter = 0u64; // Fallback tracker for missing PTS
            
            for (s, packet) in ictx.packets() {
                if cancel.load(Ordering::Relaxed) || current_idx >= n_frames { break; }

                if s.index() == stream_idx {
                    if decoder.send_packet(&packet).is_ok() {
                        while decoder.receive_frame(&mut decoded).is_ok() {
                            
                            // FIX 3: Calculate timeline securely, even if the file is an AVI packed bitstream
                            let frame_sec = if let Some(pts) = decoded.pts() {
                                pts as f64 * time_base_f
                            } else {
                                frame_counter as f64 * fallback_dur
                            };
                            frame_counter += 1;
                            
                            if first_sec.is_none() { first_sec = Some(frame_sec); }
                            let normalized_sec = frame_sec - first_sec.unwrap();

                            while current_idx < n_frames {
                                let target_ts = timestamps[current_idx];
                                
                                if normalized_sec >= target_ts {
                                    if current_fmt != decoded.format() || current_w != decoded.width() || current_h != decoded.height() {
                                        current_fmt = decoded.format(); current_w = decoded.width(); current_h = decoded.height();
                                        scaler_opt = Scaler::get(current_fmt, current_w, current_h, Pixel::RGB24, current_w, current_h, Flags::BILINEAR).ok();
                                    }
                                    if let Some(scaler) = scaler_opt.as_mut() {
                                        if scaler.run(&decoded, &mut rgb_frame).is_ok() {
                                            let img = rgb_frame_to_image(&rgb_frame);
                                            frames.lock().unwrap()[current_idx] = Some(img);
                                            loaded[current_idx] = true;
                                            loaded_count += 1;
                                            ready.store(loaded_count, Ordering::Relaxed);
                                        }
                                    }
                                    current_idx += 1;
                                } else {
                                    break; 
                                }
                            }
                        }
                    }
                }
            }
        } else {
            // NATIVE LOCAL MODE
            for batch in 0..10 {
                let start = if batch == 0 { 10 } else { batch };
                let mut i = start;
                while i < n_frames {
                    if cancel.load(Ordering::Relaxed) { break; }
                    if !loaded[i] {
                        let ts_sec = timestamps[i];
                        let target_pts = if time_base_f > 0.0 { (ts_sec / time_base_f) as i64 } else { 0 };
                        let ts = (ts_sec * ffmpeg::ffi::AV_TIME_BASE as f64) as i64;

                        if ictx.seek(ts, ..).is_ok() {
                            decoder.flush();
                            let mut extracted = false;
                            let mut read_packets = 0;

                            for (s, packet) in ictx.packets() {
                                if s.index() == stream_idx {
                                    read_packets += 1;
                                    if decoder.send_packet(&packet).is_ok() {
                                        while decoder.receive_frame(&mut decoded).is_ok() {
                                            // FIX 4: Handle missing PTS in local mode
                                            let should_extract = match decoded.pts() {
                                                Some(pts) => pts >= target_pts,
                                                None => true, 
                                            };

                                            if !should_extract { continue; }

                                            if current_fmt != decoded.format() || current_w != decoded.width() || current_h != decoded.height() {
                                                current_fmt = decoded.format(); current_w = decoded.width(); current_h = decoded.height();
                                                scaler_opt = Scaler::get(current_fmt, current_w, current_h, Pixel::RGB24, current_w, current_h, Flags::BILINEAR).ok();
                                            }

                                            if let Some(scaler) = scaler_opt.as_mut() {
                                                if scaler.run(&decoded, &mut rgb_frame).is_ok() {
                                                    let img = rgb_frame_to_image(&rgb_frame);
                                                    frames.lock().unwrap()[i] = Some(img);
                                                    loaded[i] = true;
                                                    loaded_count += 1;
                                                    ready.store(loaded_count, Ordering::Relaxed);
                                                    extracted = true;
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                }
                                if extracted || read_packets > 300 { break; }
                            }
                        }
                    }
                    i += 10;
                }
            }
        }
        
        // BACKFILL missing frames (handles standard container duration mismatches)
        if !cancel.load(Ordering::Relaxed) && loaded_count < n_frames {
            let mut guard = frames.lock().unwrap();
            let mut last_good = None;
            for i in 0..n_frames {
                if guard[i].is_some() {
                    last_good = guard[i].clone();
                } else if last_good.is_some() {
                    guard[i] = last_good.clone();
                }
            }
            drop(guard);
            ready.store(n_frames, Ordering::Relaxed);
        }

        done.store(true, Ordering::Relaxed);
    });
}