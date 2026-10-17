use anyhow::{bail, Context, Result};
use jpeg_encoder::{ColorType, Encoder, SamplingFactor};
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use wayland_client::{
    delegate_noop,
    protocol::{
        wl_buffer::WlBuffer,
        wl_output::{self, WlOutput},
        wl_registry::{self, WlRegistry},
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
    },
    Connection, Dispatch, QueueHandle, WEnum,
};
use wayland_protocols::ext::{
    image_capture_source::v1::client::{
        ext_image_capture_source_v1::ExtImageCaptureSourceV1,
        ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
    },
    image_copy_capture::v1::client::{
        ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
        ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1},
        ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
    },
};

struct OutputEntry {
    global_name: u32,
    output: WlOutput,
    name: Option<String>,
}

#[derive(Default)]
struct CaptureAppData {
    shm: Option<WlShm>,
    source_mgr: Option<ExtOutputImageCaptureSourceManagerV1>,
    copy_mgr: Option<ExtImageCopyCaptureManagerV1>,
    outputs: Vec<OutputEntry>,
    buf_width: u32,
    buf_height: u32,
    shm_format: Option<wl_shm::Format>,
    session_done: bool,
    session_stopped: bool,
    frame_ready: bool,
    frame_failed: bool,
}

impl Dispatch<WlRegistry, ()> for CaptureAppData {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_shm" => {
                    let shm = registry.bind::<WlShm, _, _>(name, version.min(1), qh, ());
                    state.shm = Some(shm);
                }
                "wl_output" => {
                    let output = registry.bind::<WlOutput, _, _>(name, version.min(4), qh, name);
                    state.outputs.push(OutputEntry {
                        global_name: name,
                        output,
                        name: None,
                    });
                }
                "ext_output_image_capture_source_manager_v1" => {
                    let mgr = registry.bind::<ExtOutputImageCaptureSourceManagerV1, _, _>(
                        name,
                        version.min(1),
                        qh,
                        (),
                    );
                    state.source_mgr = Some(mgr);
                }
                "ext_image_copy_capture_manager_v1" => {
                    let mgr = registry.bind::<ExtImageCopyCaptureManagerV1, _, _>(
                        name,
                        version.min(1),
                        qh,
                        (),
                    );
                    state.copy_mgr = Some(mgr);
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<WlOutput, u32> for CaptureAppData {
    fn event(
        state: &mut Self,
        _proxy: &WlOutput,
        event: wl_output::Event,
        global_name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            if let Some(entry) = state.outputs.iter_mut().find(|o| o.global_name == *global_name) {
                entry.name = Some(name);
            }
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for CaptureAppData {
    fn event(
        state: &mut Self,
        _proxy: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_session_v1::Event::BufferSize { width, height } => {
                state.buf_width = width;
                state.buf_height = height;
            }
            ext_image_copy_capture_session_v1::Event::ShmFormat { format } => {
                if let WEnum::Value(fmt) = format {
                    if matches!(
                        fmt,
                        wl_shm::Format::Argb8888
                            | wl_shm::Format::Xrgb8888
                            | wl_shm::Format::Abgr8888
                            | wl_shm::Format::Xbgr8888
                    ) {
                        state.shm_format = Some(fmt);
                    } else if state.shm_format.is_none() {
                        state.shm_format = Some(fmt);
                    }
                }
            }
            ext_image_copy_capture_session_v1::Event::Done => {
                state.session_done = true;
            }
            ext_image_copy_capture_session_v1::Event::Stopped => {
                state.session_stopped = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for CaptureAppData {
    fn event(
        state: &mut Self,
        _proxy: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_frame_v1::Event::Ready => {
                state.frame_ready = true;
            }
            ext_image_copy_capture_frame_v1::Event::Failed { .. } => {
                state.frame_failed = true;
            }
            _ => {}
        }
    }
}

delegate_noop!(CaptureAppData: ignore WlShm);
delegate_noop!(CaptureAppData: ignore WlShmPool);
delegate_noop!(CaptureAppData: ignore WlBuffer);
delegate_noop!(CaptureAppData: ignore ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(CaptureAppData: ignore ExtImageCaptureSourceV1);
delegate_noop!(CaptureAppData: ignore ExtImageCopyCaptureManagerV1);

#[derive(Default)]
pub struct ClientDiagnostics {
    pub debug_log: AtomicBool,
    pub acks_received: std::sync::atomic::AtomicU32,
    pub decode_ms: std::sync::atomic::AtomicU32,
    pub client_fps: std::sync::atomic::AtomicU32,
    pub queue_len: std::sync::atomic::AtomicU32,
    pub lagged_events: std::sync::atomic::AtomicU32,
    pub active_connections: std::sync::atomic::AtomicU32,
}

#[derive(Clone, Debug)]
pub struct EncodedPacket {
    /// Binary packet with 8-byte header `[0x44, 0x4D, num_rects, flags, full_w, full_h]`
    /// followed by `num_rects` chunks `[rx, ry, rw, rh, data_len] + [jpeg_bytes]`
    pub data: Arc<Vec<u8>>,
    pub is_keyframe: bool,
}

const TILE_SIZE: u32 = 64;

/// Multi-Bounding-Box scanner using a 64x64 tile grid.
/// Finds 1 to 3 disjoint changed regions, avoiding the "giant single bounding box" penalty
/// when changes occur at opposite corners of the screen (e.g. clock/terminal blinking + mouse moving).
pub fn find_dirty_rects(
    prev: &[u8],
    curr: &[u8],
    width: u32,
    height: u32,
) -> Vec<(u32, u32, u32, u32)> {
    let cols = ((width + TILE_SIZE - 1) / TILE_SIZE) as usize;
    let rows = ((height + TILE_SIZE - 1) / TILE_SIZE) as usize;
    let stride = (width * 4) as usize;

    let mut dirty_tiles = vec![false; cols * rows];
    let mut any_dirty = false;

    for ty in 0..rows {
        let y_start = ty as u32 * TILE_SIZE;
        let y_end = (y_start + TILE_SIZE).min(height);
        let band_start = y_start as usize * stride;
        let band_end = y_end as usize * stride;

        // Quick full-band equality check
        if prev[band_start..band_end] == curr[band_start..band_end] {
            continue;
        }

        for tx in 0..cols {
            let x_start = tx as u32 * TILE_SIZE;
            let x_end = (x_start + TILE_SIZE).min(width);
            let byte_start = x_start as usize * 4;
            let byte_len = (x_end - x_start) as usize * 4;

            let mut tile_dirty = false;
            for y in y_start..y_end {
                let row_offset = y as usize * stride;
                let prev_chunk = &prev[row_offset + byte_start..row_offset + byte_start + byte_len];
                let curr_chunk = &curr[row_offset + byte_start..row_offset + byte_start + byte_len];
                if prev_chunk != curr_chunk {
                    tile_dirty = true;
                    break;
                }
            }

            if tile_dirty {
                dirty_tiles[ty * cols + tx] = true;
                any_dirty = true;
            }
        }
    }

    if !any_dirty {
        return Vec::new();
    }

    // Cluster dirty tiles into connected components (8-way connectivity)
    let mut visited = vec![false; cols * rows];
    let mut clusters: Vec<(u32, u32, u32, u32)> = Vec::new(); // (min_tx, min_ty, max_tx, max_ty)

    for ty in 0..rows {
        for tx in 0..cols {
            let idx = ty * cols + tx;
            if !dirty_tiles[idx] || visited[idx] {
                continue;
            }

            let mut queue = Vec::new();
            queue.push((tx, ty));
            visited[idx] = true;

            let mut min_tx = tx;
            let mut max_tx = tx;
            let mut min_ty = ty;
            let mut max_ty = ty;

            let mut q_idx = 0;
            while q_idx < queue.len() {
                let (cx, cy) = queue[q_idx];
                q_idx += 1;

                min_tx = min_tx.min(cx);
                max_tx = max_tx.max(cx);
                min_ty = min_ty.min(cy);
                max_ty = max_ty.max(cy);

                let x_min = cx.saturating_sub(1);
                let x_max = (cx + 1).min(cols - 1);
                let y_min = cy.saturating_sub(1);
                let y_max = (cy + 1).min(rows - 1);

                for ny in y_min..=y_max {
                    for nx in x_min..=x_max {
                        let n_idx = ny * cols + nx;
                        if dirty_tiles[n_idx] && !visited[n_idx] {
                            visited[n_idx] = true;
                            queue.push((nx, ny));
                        }
                    }
                }
            }

            clusters.push((min_tx as u32, min_ty as u32, max_tx as u32, max_ty as u32));
        }
    }

    let clamp_even_rect = |min_x: u32, min_y: u32, max_x: u32, max_y: u32| -> (u32, u32, u32, u32) {
        let x0 = min_x & !1;
        let y0 = min_y & !1;
        let x1 = ((max_x + 1) & !1).min(width);
        let y1 = ((max_y + 1) & !1).min(height);
        let rw = x1.saturating_sub(x0).max(2).min(width - x0);
        let rh = y1.saturating_sub(y0).max(2).min(height - y0);
        (x0, y0, rw, rh)
    };

    let mut rects: Vec<(u32, u32, u32, u32)> = Vec::with_capacity(clusters.len());
    for (min_tx, min_ty, max_tx, max_ty) in clusters {
        let start_y = min_ty * TILE_SIZE;
        let end_y = ((max_ty + 1) * TILE_SIZE).min(height);
        let start_x = min_tx * TILE_SIZE;
        let end_x = ((max_tx + 1) * TILE_SIZE).min(width);

        let mut min_px = u32::MAX;
        let mut max_px = 0u32;
        let mut min_py = u32::MAX;
        let mut max_py = 0u32;

        for y in start_y..end_y {
            let row_offset = y as usize * stride;
            let byte_start = start_x as usize * 4;
            let byte_len = (end_x - start_x) as usize * 4;
            if prev[row_offset + byte_start..row_offset + byte_start + byte_len]
                == curr[row_offset + byte_start..row_offset + byte_start + byte_len]
            {
                continue;
            }

            min_py = min_py.min(y);
            max_py = max_py.max(y);

            let mut fx = start_x;
            while fx < end_x {
                let off = row_offset + fx as usize * 4;
                if prev[off..off + 4] != curr[off..off + 4] {
                    break;
                }
                fx += 1;
            }

            let mut lx = end_x;
            while lx > fx {
                let off = row_offset + (lx - 1) as usize * 4;
                if prev[off..off + 4] != curr[off..off + 4] {
                    break;
                }
                lx -= 1;
            }

            if fx < lx {
                min_px = min_px.min(fx);
                max_px = max_px.max(lx - 1);
            }
        }

        if min_px <= max_px && min_py <= max_py {
            let x0 = min_px.saturating_sub(2);
            let y0 = min_py.saturating_sub(2);
            let x1 = (max_px + 3).min(width);
            let y1 = (max_py + 3).min(height);
            rects.push(clamp_even_rect(x0, y0, x1, y1));
        }
    }

    // If more than 3 clusters, merge all into a single bounding box
    if rects.len() > 3 {
        let min_x = rects.iter().map(|r| r.0).min().unwrap();
        let min_y = rects.iter().map(|r| r.1).min().unwrap();
        let max_x = rects.iter().map(|r| r.0 + r.2).max().unwrap();
        let max_y = rects.iter().map(|r| r.1 + r.3).max().unwrap();
        rects = vec![clamp_even_rect(min_x, min_y, max_x, max_y)];
    } else if rects.len() > 1 {
        // If the bounding box of all clusters is close to their sum (<= 1.5x), merge them
        let sum_area: u64 = rects.iter().map(|r| r.2 as u64 * r.3 as u64).sum();
        let min_x = rects.iter().map(|r| r.0).min().unwrap();
        let min_y = rects.iter().map(|r| r.1).min().unwrap();
        let max_x = rects.iter().map(|r| r.0 + r.2).max().unwrap();
        let max_y = rects.iter().map(|r| r.1 + r.3).max().unwrap();
        let union_area = (max_x - min_x) as u64 * (max_y - min_y) as u64;

        if union_area <= sum_area + (sum_area / 2) {
            rects = vec![clamp_even_rect(min_x, min_y, max_x, max_y)];
        }
    }

    // If single rect covers > 60% of screen, promote to full frame
    if rects.len() == 1 {
        let (_, _, rw, rh) = rects[0];
        if (rw as u64 * rh as u64) * 10 > (width as u64 * height as u64) * 6 {
            rects = vec![(0, 0, width, height)];
        }
    }

    rects
}

/// Encodes 1 or more sub-rectangles into a single binary packet:
/// Header (8 bytes): `[0x44, 0x4D, num_rects, flags, full_w: u16, full_h: u16]`
/// Followed by `num_rects` entries: `[rx: u16, ry: u16, rw: u16, rh: u16, data_len: u32] + [jpeg_bytes]`
fn encode_multi_rect_packet(
    frame_buf: &[u8],
    patch_scratch: &mut Vec<u8>,
    full_w: u32,
    full_h: u32,
    rects: &[(u32, u32, u32, u32)],
    color_type: ColorType,
    jpeg_quality: u8,
) -> Option<EncodedPacket> {
    if rects.is_empty() {
        return None;
    }
    let is_keyframe = rects.len() == 1
        && rects[0].0 == 0
        && rects[0].1 == 0
        && rects[0].2 == full_w
        && rects[0].3 == full_h;

    let num_rects = rects.len() as u8;
    let flags: u8 = if is_keyframe { 1 } else { 0 };

    let mut out = Vec::with_capacity(if is_keyframe { 128 * 1024 } else { 32 * 1024 });

    // 8-byte header: [0x44, 0x4D, num_rects, flags, full_w, full_h]
    out.push(0x44);
    out.push(0x4D);
    out.push(num_rects);
    out.push(flags);
    out.extend_from_slice(&(full_w as u16).to_le_bytes());
    out.extend_from_slice(&(full_h as u16).to_le_bytes());

    let full_stride = (full_w * 4) as usize;

    for &(rx, ry, rw, rh) in rects {
        let desc_offset = out.len();
        out.extend_from_slice(&(rx as u16).to_le_bytes());
        out.extend_from_slice(&(ry as u16).to_le_bytes());
        out.extend_from_slice(&(rw as u16).to_le_bytes());
        out.extend_from_slice(&(rh as u16).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // placeholder for data_len

        let jpeg_start = out.len();
        let mut encoder = Encoder::new(&mut out, jpeg_quality);
        encoder.set_sampling_factor(SamplingFactor::F_2_2);

        let encode_res = if is_keyframe {
            encoder.encode(frame_buf, full_w as u16, full_h as u16, color_type)
        } else {
            let patch_stride = (rw * 4) as usize;
            patch_scratch.clear();
            patch_scratch.reserve(patch_stride * rh as usize);
            for row in ry..(ry + rh) {
                let start = row as usize * full_stride + (rx as usize * 4);
                patch_scratch.extend_from_slice(&frame_buf[start..start + patch_stride]);
            }
            encoder.encode(patch_scratch, rw as u16, rh as u16, color_type)
        };

        if encode_res.is_err() {
            return None;
        }

        let data_len = (out.len() - jpeg_start) as u32;
        out[desc_offset + 8..desc_offset + 12].copy_from_slice(&data_len.to_le_bytes());
    }

    Some(EncodedPacket {
        data: Arc::new(out),
        is_keyframe,
    })
}

/// Runs the Wayland screen capture loop with Dirty-Rectangle (Delta Bounding-Box) optimization!
/// Only encodes and transmits the region `(x, y, w, h)` that actually changed between frames.
/// When moving the mouse cursor, encodes a tiny ~64x64 patch (~1.5 KB) in < 0.2ms instead of the entire screen!
pub fn run_wayland_capture_loop(
    target_output_name: String,
    _target_fps: u32,
    jpeg_quality: u8,
    packet_tx: broadcast::Sender<EncodedPacket>,
    latest_keyframe: Arc<RwLock<Option<Arc<Vec<u8>>>>>,
    diagnostics: Arc<ClientDiagnostics>,
    stop_flag: Arc<AtomicBool>,
) -> Result<()> {
    let conn = connect_wayland_for_capture()?;
    let mut event_queue = conn.new_event_queue::<CaptureAppData>();
    let qh = event_queue.handle();

    let display = conn.display();
    let _registry = display.get_registry(&qh, ());

    let mut state = CaptureAppData::default();
    event_queue.roundtrip(&mut state)?;
    event_queue.roundtrip(&mut state)?;

    let shm = state
        .shm
        .clone()
        .context("Compositor Wayland não expôs wl_shm")?;
    let source_mgr = state.source_mgr.clone().context(
        "Compositor Wayland não expôs ext_output_image_capture_source_manager_v1",
    )?;
    let copy_mgr = state
        .copy_mgr
        .clone()
        .context("Compositor Wayland não expôs ext_image_copy_capture_manager_v1")?;

    let mut target_wl_output = None;
    for _ in 0..25 {
        if let Some(found) = state
            .outputs
            .iter()
            .find(|o| o.name.as_deref() == Some(target_output_name.as_str()))
        {
            target_wl_output = Some(found.output.clone());
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
        let _ = display.get_registry(&qh, ());
        event_queue.roundtrip(&mut state)?;
        event_queue.roundtrip(&mut state)?;
    }

    let wl_output = target_wl_output.with_context(|| {
        let available: Vec<_> = state
            .outputs
            .iter()
            .filter_map(|o| o.name.clone())
            .collect();
        format!(
            "Saída Wayland '{}' não encontrada. Saídas ativas: {:?}",
            target_output_name, available
        )
    })?;

    let source = source_mgr.create_source(&wl_output, &qh, ());
    let session = copy_mgr.create_session(
        &source,
        ext_image_copy_capture_manager_v1::Options::PaintCursors,
        &qh,
        (),
    );

    for _ in 0..25 {
        event_queue.roundtrip(&mut state)?;
        if state.session_done && state.buf_width > 0 && state.buf_height > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    if state.buf_width == 0 || state.buf_height == 0 {
        bail!("Sessão de captura Wayland não informou dimensões válidas do monitor.");
    }

    let width = state.buf_width;
    let height = state.buf_height;
    let stride = width * 4;
    let frame_size = (stride * height) as usize;
    let pool_size = frame_size * 2;
    let format = state.shm_format.unwrap_or(wl_shm::Format::Argb8888);

    let memfd = nix::sys::memfd::memfd_create(
        c"droidmonitor-shm",
        nix::sys::memfd::MemFdCreateFlag::MFD_CLOEXEC,
    )?;
    nix::unistd::ftruncate(&memfd, pool_size as i64)?;

    let mmap_ptr = unsafe {
        nix::sys::mman::mmap(
            None,
            std::num::NonZeroUsize::new(pool_size).unwrap(),
            nix::sys::mman::ProtFlags::PROT_READ | nix::sys::mman::ProtFlags::PROT_WRITE,
            nix::sys::mman::MapFlags::MAP_SHARED,
            &memfd,
            0,
        )?
    };

    let pool = shm.create_pool(memfd.as_fd(), pool_size as i32, &qh, ());
    let buffers = [
        pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            format,
            &qh,
            (),
        ),
        pool.create_buffer(
            frame_size as i32,
            width as i32,
            height as i32,
            stride as i32,
            format,
            &qh,
            (),
        ),
    ];

    let is_bgra = matches!(
        format,
        wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888
    );
    let color_type = if is_bgra {
        ColorType::Bgra
    } else {
        ColorType::Rgba
    };

    let mut prev_frame = vec![0u8; frame_size];
    let mut patch_scratch = Vec::with_capacity(256 * 1024);
    let mut has_prev_keyframe = false;
    let mut last_keyframe_at = Instant::now();
    let mut idle_streak = 0u32;

    let mut stat_wayland_frames = 0u32;
    let mut stat_sent_packets = 0u32;
    let mut stat_wait_us = 0u64;
    let mut stat_enc_us = 0u64;
    let mut stat_bytes = 0usize;
    let mut stat_rect_w = 0u64;
    let mut stat_rect_h = 0u64;
    let mut stat_rect_count = 0u64;
    let mut stat_timer = Instant::now();

    // Kick off the first capture on buffer 0
    let mut cur_buf_idx = 0usize;
    state.frame_ready = false;
    state.frame_failed = false;
    let mut active_frame = session.create_frame(&qh, ());
    active_frame.attach_buffer(&buffers[cur_buf_idx]);
    active_frame.damage_buffer(0, 0, width as i32, height as i32);
    active_frame.capture();
    let _ = conn.flush();
    let mut wait_start = Instant::now();

    while !stop_flag.load(Ordering::Relaxed) && !state.session_stopped {
        while !state.frame_ready && !state.frame_failed && !state.session_stopped {
            if stop_flag.load(Ordering::Relaxed) {
                break;
            }
            event_queue.dispatch_pending(&mut state)?;
            if state.frame_ready || state.frame_failed || state.session_stopped {
                break;
            }
            if let Some(guard) = conn.prepare_read() {
                use std::os::fd::AsRawFd;
                let raw_fd = guard.connection_fd().as_raw_fd();
                let mut pfd = libc_pollfd(raw_fd);
                let ret = unsafe { poll_fd(&mut pfd, 100) };
                if ret > 0 {
                    let _ = guard.read();
                } else {
                    drop(guard);
                }
            }
            event_queue.dispatch_pending(&mut state)?;
        }

        if stop_flag.load(Ordering::Relaxed) || state.session_stopped {
            break;
        }

        let got_ready = state.frame_ready;
        let wait_us = wait_start.elapsed().as_micros() as u64;
        let ready_buf_idx = cur_buf_idx;

        // Destroy the completed frame and IMMEDIATELY queue the next capture on the alternate buffer
        // BEFORE spending any CPU time on diffing or JPEG encoding!
        active_frame.destroy();
        cur_buf_idx ^= 1;
        state.frame_ready = false;
        state.frame_failed = false;
        active_frame = session.create_frame(&qh, ());
        active_frame.attach_buffer(&buffers[cur_buf_idx]);
        active_frame.damage_buffer(0, 0, width as i32, height as i32);
        active_frame.capture();
        let _ = conn.flush();
        wait_start = Instant::now();

        if got_ready {
            let enc_start = Instant::now();
            let buf_offset = ready_buf_idx * frame_size;
            let curr_slice = unsafe {
                std::slice::from_raw_parts(
                    (mmap_ptr.as_ptr() as *const u8).add(buf_offset),
                    frame_size,
                )
            };

            let force_keyframe =
                !has_prev_keyframe || last_keyframe_at.elapsed() >= Duration::from_secs(4);

            if force_keyframe {
                prev_frame.copy_from_slice(curr_slice);
                let full_rect = vec![(0, 0, width, height)];
                if let Some(pkt) = encode_multi_rect_packet(
                    &prev_frame,
                    &mut patch_scratch,
                    width,
                    height,
                    &full_rect,
                    color_type,
                    jpeg_quality,
                ) {
                    stat_bytes += pkt.data.len();
                    stat_sent_packets += 1;
                    stat_rect_w += width as u64;
                    stat_rect_h += height as u64;
                    stat_rect_count += 1;
                    if let Ok(mut kf) = latest_keyframe.write() {
                        *kf = Some(pkt.data.clone());
                    }
                    let _ = packet_tx.send(pkt);
                    has_prev_keyframe = true;
                    last_keyframe_at = Instant::now();
                }
            } else {
                let rects = find_dirty_rects(&prev_frame, curr_slice, width, height);
                if !rects.is_empty() {
                    idle_streak = 0;
                    if rects.len() == 1 && rects[0] == (0, 0, width, height) {
                        prev_frame.copy_from_slice(curr_slice);
                    } else {
                        let full_stride = (width * 4) as usize;
                        for &(rx, ry, rw, rh) in &rects {
                            let patch_stride = (rw * 4) as usize;
                            let rx_bytes = (rx * 4) as usize;
                            for row in ry..(ry + rh) {
                                let row_offset = row as usize * full_stride;
                                let start = row_offset + rx_bytes;
                                prev_frame[start..start + patch_stride]
                                    .copy_from_slice(&curr_slice[start..start + patch_stride]);
                            }
                        }
                    }

                    let dyn_quality = if rects.len() == 1 && rects[0] == (0, 0, width, height) {
                        jpeg_quality.saturating_sub(10).max(50)
                    } else {
                        jpeg_quality
                    };

                    if let Some(pkt) = encode_multi_rect_packet(
                        &prev_frame,
                        &mut patch_scratch,
                        width,
                        height,
                        &rects,
                        color_type,
                        dyn_quality,
                    ) {
                        stat_bytes += pkt.data.len();
                        stat_sent_packets += 1;
                        for r in &rects {
                            stat_rect_w += r.2 as u64;
                            stat_rect_h += r.3 as u64;
                        }
                        stat_rect_count += rects.len() as u64;
                        if pkt.is_keyframe {
                            if let Ok(mut kf) = latest_keyframe.write() {
                                *kf = Some(pkt.data.clone());
                            }
                            last_keyframe_at = Instant::now();
                        }
                        let _ = packet_tx.send(pkt);
                    }
                } else {
                    idle_streak = idle_streak.saturating_add(1);
                }
            }

            let enc_us = enc_start.elapsed().as_micros() as u64;
            stat_wayland_frames += 1;
            stat_wait_us += wait_us;
            stat_enc_us += enc_us;

            if idle_streak > 20 {
                std::thread::sleep(Duration::from_millis(16));
            } else if wait_us + enc_us < 7500 {
                // Cap active Wayland polling at ~130Hz so we never waste CPU above 2x60Hz
                std::thread::sleep(Duration::from_micros(7500 - (wait_us + enc_us)));
            }

            let elapsed_s = stat_timer.elapsed().as_secs_f64();
            if elapsed_s >= 2.0 {
                let acks = diagnostics.acks_received.swap(0, Ordering::Relaxed);
                let dec_ms = diagnostics.decode_ms.load(Ordering::Relaxed);
                let cli_fps = diagnostics.client_fps.load(Ordering::Relaxed);
                let q_len = diagnostics.queue_len.load(Ordering::Relaxed);
                let lags = diagnostics.lagged_events.swap(0, Ordering::Relaxed);

                if diagnostics.debug_log.load(Ordering::Relaxed) && stat_sent_packets > 2 {
                    let n = stat_wayland_frames.max(1) as u64;
                    let p = stat_sent_packets.max(1) as u64;
                    let rc = stat_rect_count.max(1);
                    let is_cli_connected = diagnostics.active_connections.load(Ordering::Relaxed) > 0;
                    let android_stat = if is_cli_connected {
                        format!(
                            "{}fps (ACKs {:.0}/s, decode {}ms, fila {}, lag {})",
                            cli_fps,
                            acks as f64 / elapsed_s,
                            dec_ms,
                            q_len,
                            lags
                        )
                    } else {
                        "desconectado (aguardando cliente...)".to_string()
                    };

                    eprintln!(
                        "📊 [Diagnóstico] Wayland: {:.0}fps (wait {:.1}ms) | Dirty: {:.0}pkt/s ({:.1} rects/p, avg {}x{}, {:.1}KB, enc {:.2}ms) | Android: {}",
                        stat_wayland_frames as f64 / elapsed_s,
                        (stat_wait_us / n) as f64 / 1000.0,
                        stat_sent_packets as f64 / elapsed_s,
                        stat_rect_count as f64 / p as f64,
                        stat_rect_w / rc,
                        stat_rect_h / rc,
                        (stat_bytes as f64 / p as f64) / 1024.0,
                        (stat_enc_us / n) as f64 / 1000.0,
                        android_stat
                    );
                }
                stat_wayland_frames = 0;
                stat_sent_packets = 0;
                stat_wait_us = 0;
                stat_enc_us = 0;
                stat_bytes = 0;
                stat_rect_w = 0;
                stat_rect_h = 0;
                stat_rect_count = 0;
                stat_timer = Instant::now();
            }
        }
    }

    active_frame.destroy();
    buffers[0].destroy();
    buffers[1].destroy();
    pool.destroy();
    session.destroy();
    source.destroy();
    unsafe {
        let _ = nix::sys::mman::munmap(mmap_ptr, pool_size);
    }

    Ok(())
}

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

fn libc_pollfd(fd: i32) -> PollFd {
    PollFd {
        fd,
        events: 1, // POLLIN
        revents: 0,
    }
}

unsafe fn poll_fd(pfd: &mut PollFd, timeout_ms: i32) -> i32 {
    unsafe extern "C" {
        fn poll(fds: *mut PollFd, nfds: u64, timeout: i32) -> i32;
    }
    unsafe { poll(pfd as *mut PollFd, 1, timeout_ms) }
}

fn connect_wayland_for_capture() -> Result<Connection> {
    Connection::connect_to_env()
        .context("Não foi possível conectar ao compositor Wayland (verifique WAYLAND_DISPLAY)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::list_cosmic_outputs;

    #[test]
    fn test_dirty_rect_delta() {
        let w = 1280u32;
        let h = 720u32;
        let stride = (w * 4) as usize;
        let prev = vec![0u8; stride * h as usize];
        let mut curr = prev.clone();

        // Simulate a 24x24 mouse cursor moving around (500, 300)
        for y in 300..324 {
            for x in 500..524 {
                let idx = y * stride + x * 4;
                curr[idx] = 255;
                curr[idx + 1] = 255;
                curr[idx + 2] = 255;
            }
        }

        let rects = find_dirty_rects(&prev, &curr, w, h);
        assert_eq!(rects.len(), 1, "Should detect exactly 1 cluster for single cursor");
        let rect = rects[0];
        assert!(rect.2 <= 80 && rect.3 <= 80, "Dirty rect should be small: {:?}", rect);

        let mut crop_buf = Vec::new();
        let pkt = encode_multi_rect_packet(
            &curr,
            &mut crop_buf,
            w,
            h,
            &rects,
            ColorType::Bgra,
            75,
        )
        .expect("Failed to encode multi-rect packet");
        assert!(!pkt.is_keyframe);
        assert_eq!(pkt.data[0], 0x44);
        assert_eq!(pkt.data[1], 0x4D);
        assert_eq!(pkt.data[2], 1); // 1 rect
        assert!(
            pkt.data.len() > 8 && pkt.data.len() < 4096,
            "Delta packet should be tiny: {} bytes",
            pkt.data.len()
        );
    }

    #[test]
    fn test_multi_dirty_rects_disjoint_islands() {
        let w = 1280u32;
        let h = 720u32;
        let stride = (w * 4) as usize;
        let prev = vec![0u8; stride * h as usize];
        let mut curr = prev.clone();

        // Island 1: Cursor in top-left (100, 100)
        for y in 100..124 {
            for x in 100..124 {
                let idx = y * stride + x * 4;
                curr[idx] = 255;
            }
        }

        // Island 2: Clock or blinking terminal in bottom-right (1100, 600)
        for y in 600..630 {
            for x in 1100..1150 {
                let idx = y * stride + x * 4;
                curr[idx + 1] = 255;
            }
        }

        let rects = find_dirty_rects(&prev, &curr, w, h);
        assert_eq!(rects.len(), 2, "Should detect 2 disjoint islands: {:?}", rects);

        let mut crop_buf = Vec::new();
        let pkt = encode_multi_rect_packet(
            &curr,
            &mut crop_buf,
            w,
            h,
            &rects,
            ColorType::Bgra,
            75,
        )
        .expect("Failed to encode 2-rect packet");

        assert_eq!(pkt.data[0], 0x44);
        assert_eq!(pkt.data[1], 0x4D);
        assert_eq!(pkt.data[2], 2); // 2 rects
        assert!(!pkt.is_keyframe);
        assert!(
            pkt.data.len() < 8192,
            "Two small disjoint rects should be under 8KB, got: {} bytes",
            pkt.data.len()
        );
    }

    #[test]
    fn test_live_wayland_capture_one_frame() {
        let outputs = list_cosmic_outputs().expect("Failed to list cosmic outputs");
        let active = outputs
            .into_iter()
            .find(|o| o.enabled && !o.is_droidmonitor)
            .expect("No active output found");

        let (tx, mut rx) = broadcast::channel::<EncodedPacket>(16);
        let latest_kf = Arc::new(RwLock::new(None));
        let diag = Arc::new(ClientDiagnostics::default());
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();

        let handle = std::thread::spawn(move || {
            for _ in 0..30 {
                std::thread::sleep(Duration::from_millis(30));
                if let Ok(pkt) = rx.try_recv() {
                    if pkt.data.len() > 8 {
                        stop_clone.store(true, Ordering::Relaxed);
                        return true;
                    }
                }
            }
            stop_clone.store(true, Ordering::Relaxed);
            false
        });

        let res = run_wayland_capture_loop(active.name, 60, 75, tx, latest_kf, diag, stop);
        let got_frame = handle.join().unwrap();
        assert!(res.is_ok(), "Capture failed: {:?}", res);
        assert!(got_frame, "Did not receive captured JPEG packet");
    }
}
