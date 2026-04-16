//! 화면 캡처 + VP9 인코딩 → VideoFrame → mpsc 채널
//!
//! 파이프라인:
//!   DXGI Desktop Duplication (BGRA)
//!     → BGRA→I420 변환 (순수 Rust)
//!     → VP9 인코딩 (libvpx, C 래퍼)
//!     → try_send → 세션 루프
//!
//! 폴백: DXGI 초기화 실패 시 → JPEG (screenshots 대신 winapi GDI BitBlt 사용 불가,
//!       단순히 오류 반환)

use anyhow::Result;
use hbb_common::log;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use super::{
    capture_dxgi::DxgiCapture,
    vpx_enc::VpxEncoder,
    yuv::{bgra_to_i420, bgra_to_i420_rects},
};

pub const TARGET_FPS: u64 = 60;
const FRAME_INTERVAL: Duration = Duration::from_micros(1_000_000 / TARGET_FPS);

// JPEG 폴백 품질 (0-100)
const JPEG_QUALITY: u8 = 80;

// VP9 비트레이트 (kbps): 1080p 원격 데스크톱 권장값
const BITRATE_KBPS_DEFAULT: u32 = 8000;

// ── VideoFrame ───────────────────────────────────────────────────────────────

/// 코덱 종류 (Init 메시지로 뷰어에 전달)
#[derive(Clone, Copy, Debug)]
pub enum Codec {
    Jpeg = 0,
    Vp9  = 1,
}

pub struct VideoFrame {
    pub data:      Vec<u8>,
    pub width:     u32,
    pub height:    u32,
    pub fps:       u8,
    pub codec:     Codec,
    pub is_key:    bool,
}

// ── Windows 고해상도 타이머 ───────────────────────────────────────────────────

#[cfg(target_os = "windows")]
mod timer {
    use winapi::um::timeapi::{timeBeginPeriod, timeEndPeriod};
    pub fn begin() { unsafe { timeBeginPeriod(1); } }
    pub fn end()   { unsafe { timeEndPeriod(1); } }
}
#[cfg(not(target_os = "windows"))]
mod timer {
    pub fn begin() {}
    pub fn end()   {}
}

// ── FNV 샘플 해시 (변화 없는 프레임 스킵) ──────────────────────────────────

fn fnv_sample(data: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME:  u64 = 0x100000001b3;
    let mut h = OFFSET;
    for &b in data.iter().step_by(256) {
        h ^= b as u64;
        h = h.wrapping_mul(PRIME);
    }
    h
}

// ── 캡처 루프 ────────────────────────────────────────────────────────────────

/// BGRA 슬라이스를 JPEG 바이트로 인코딩
fn encode_jpeg(bgra: &[u8], w: u32, h: u32) -> Result<Vec<u8>> {
    use image::{codecs::jpeg::JpegEncoder, ImageBuffer, Rgb};
    let rgb: Vec<u8> = bgra.chunks_exact(4)
        .flat_map(|p| [p[2], p[1], p[0]])
        .collect();
    let img = ImageBuffer::<Rgb<u8>, _>::from_raw(w, h, rgb)
        .ok_or_else(|| anyhow::anyhow!("이미지 버퍼 생성 실패"))?;
    let mut out = Vec::new();
    let mut enc = JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY);
    enc.encode_image(&img)?;
    Ok(out)
}

/// 화면 캡처 루프 — spawn_blocking 안에서 동기 실행
pub fn capture_loop(tx: mpsc::Sender<VideoFrame>, session_key: String) -> Result<()> {
    log::info!("[video] 캡처 시작: {}", session_key);

    timer::begin();

    // DXGI 캡처 초기화 (재연결 시 이전 핸들 해제 or 디스플레이 전환 대기를 위해 재시도)
    // 0x8000FFFF(E_UNEXPECTED): GPU 드라이버가 이전 세션 핸들을 아직 해제 중인 경우 발생.
    // session.rs에서 1500ms 대기 후 진입하지만 드라이버가 느릴 수 있으므로 최대 20회 재시도.
    let mut capture = {
        const MAX_ATTEMPTS: u8 = 20;
        const RETRY_INTERVAL_MS: u64 = 500;
        let mut cap = None;
        for attempt in 1..=MAX_ATTEMPTS {
            match DxgiCapture::new() {
                Ok(c) => {
                    if attempt > 1 {
                        log::info!("[video] DXGI 초기화 성공 (시도 {}/{})", attempt, MAX_ATTEMPTS);
                    }
                    cap = Some(c);
                    break;
                }
                Err(e) => {
                    if attempt < MAX_ATTEMPTS {
                        log::warn!("[video] DXGI 초기화 실패 (시도 {}/{}): {:?} — {}ms 후 재시도",
                            attempt, MAX_ATTEMPTS, e, RETRY_INTERVAL_MS);
                        std::thread::sleep(Duration::from_millis(RETRY_INTERVAL_MS));
                    } else {
                        log::error!("[video] DXGI 초기화 실패 (최종, {}회 시도): {:?}", MAX_ATTEMPTS, e);
                        return Err(e);
                    }
                }
            }
        }
        cap.unwrap()
    };

    let w = capture.width;
    let h = capture.height;
    log::info!("[video] {}x{} DXGI 캡처 초기화 완료", w, h);

    let bitrate_kbps = std::env::var("VDESK_VP9_BITRATE_KBPS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(BITRATE_KBPS_DEFAULT);

    // VP9 인코더 초기화 (실패 시 JPEG 폴백)
    let mut encoder: Option<VpxEncoder> =
        match VpxEncoder::new(w, h, bitrate_kbps, TARGET_FPS as u32) {
        Ok(enc) => {
            log::info!(
                "[video] VP9 인코더 초기화 완료 ({}kbps, {}fps)",
                bitrate_kbps,
                TARGET_FPS
            );
            Some(enc)
        }
        Err(e) => {
            log::warn!("[video] VP9 인코더 초기화 실패: {:?}", e);
            log::warn!("[video] ★ JPEG 폴백 모드로 전환 (품질: {})", JPEG_QUALITY);
            None
        }
    };

    let mut i420_buf: Vec<u8>      = Vec::new();
    let mut last_hash: u64         = 0;
    let mut frames: u64            = 0;
    let mut drop_count: u32        = 0;
    let mut force_keyframe_on_next = false; // 채널 드롭 발생 시 다음 프레임을 키프레임으로 강제
    let mut last_tick              = Instant::now();

    loop {
        if tx.is_closed() {
            break;
        }

        // FPS 제한
        let elapsed = last_tick.elapsed();
        if elapsed < FRAME_INTERVAL {
            std::thread::sleep(FRAME_INTERVAL - elapsed);
        }
        last_tick = Instant::now();

        // DXGI 캡처
        let frame = match capture.capture() {
            Ok(Some(f)) => f,
            Ok(None)    => continue, // 변화 없음
            Err(e) => {
                log::warn!("[video] DXGI 캡처 오류: {:?} — 재초기화 시도", e);
                std::thread::sleep(Duration::from_millis(200));
                match DxgiCapture::new() {
                    Ok(c) => {
                        capture = c;
                        log::info!("[video] DXGI 재초기화 성공");
                        continue;
                    }
                    Err(e2) => {
                        log::error!("[video] DXGI 재초기화 실패: {:?}", e2);
                        std::thread::sleep(Duration::from_millis(1000));
                        continue;
                    }
                }
            }
        };

        // 변화 감지 (FNV 샘플 해시)
        // - is_full_frame=false(부분 캡처): 항상 전송 (변경 영역 확실)
        // - is_full_frame=true + has_dirty_rects=false(DirtyRects 없음):
        //     LastPresentTime != 0 이므로 화면이 바뀐 것은 확실하나 위치를 모름.
        //     해시 샘플링이 작은 변경을 놓칠 수 있으므로 스킵 금지.
        // - is_full_frame=true + has_dirty_rects=true(dirty 면적 >= 50%):
        //     화면 절반 이상 변경 → 해시가 변화를 감지할 확률이 높아 스킵 적용.
        let force_key = force_keyframe_on_next || frames % (TARGET_FPS * 10) == 0;
        force_keyframe_on_next = false;
        if frame.is_full_frame {
            let hash = fnv_sample(frame.bgra);
            let is_static = hash == last_hash;
            last_hash = hash;
            if is_static && !force_key && frame.has_dirty_rects {
                continue;
            }
        }

        // ── 인코딩 ──────────────────────────────────────────────────────────
        let (encoded, codec, is_key) = if let Some(enc) = encoder.as_mut() {
            // VP9 경로 — 변경 영역만 I420 업데이트 (전체 변경 시 전체 변환)
            if frame.is_full_frame {
                bgra_to_i420(frame.bgra, w as usize, h as usize, &mut i420_buf);
            } else {
                bgra_to_i420_rects(frame.bgra, w as usize, h as usize, &mut i420_buf, frame.dirty_rects);
            }
            match enc.encode(&i420_buf, force_key) {
                Ok(Some((d, k))) => (d.to_vec(), Codec::Vp9, k),
                Ok(None)         => continue,
                Err(e) => {
                    log::warn!("[video] VP9 인코딩 오류: {:?}", e);
                    continue;
                }
            }
        } else {
            // JPEG 폴백 경로
            match encode_jpeg(frame.bgra, w, h) {
                Ok(d) => (d, Codec::Jpeg, true),
                Err(e) => {
                    log::warn!("[video] JPEG 인코딩 오류: {:?}", e);
                    continue;
                }
            }
        };

        match tx.try_send(VideoFrame {
            data: encoded,
            width: w,
            height: h,
            fps: TARGET_FPS as u8,
            codec,
            is_key,
        }) {
            Ok(_) => { drop_count = 0; }
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                drop_count += 1;
                force_keyframe_on_next = true; // 드롭 후 다음 프레임은 키프레임으로 강제
                if drop_count % 10 == 0 {
                    log::warn!("[video] 채널 포화 드롭 {}회 — 다음 프레임 키프레임 강제", drop_count);
                }
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break,
        }

        frames += 1;
        if frames % 300 == 0 {
            log::debug!("[video] {}프레임 전송 (코덱: {:?})", frames, if encoder.is_some() { "VP9" } else { "JPEG" });
        }
    }

    timer::end();
    log::info!("[video] 캡처 루프 종료");
    Ok(())
}
