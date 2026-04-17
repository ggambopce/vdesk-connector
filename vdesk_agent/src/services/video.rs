//! 화면 캡처 + VP9 인코딩 → VideoFrame → mpsc 채널
//!
//! 캡처 상태 머신:
//!   DXGI 초기 시도 (3회, 200ms 간격)
//!     ├─ 성공 → DXGI 모드
//!     │          캡처 오류 5회 연속 → 전체 스택 해제 후 DXGI 재초기화
//!     │                              └─ 재초기화 실패 → GDI 모드 (30초마다 DXGI 복귀 시도)
//!     └─ 실패 → GDI 모드 (30초마다 DXGI 복귀 시도)
//!
//! GDI 모드: winapi GDI BitBlt — RDP/터미널 세션, 드라이버 불안정 환경에서 사용
//!   - DirtyRects 없음 → 매 프레임 전체 캡처, FNV 해시로 정적 화면 스킵
//!   - 동일 VP9/JPEG 인코딩 파이프라인

use anyhow::Result;
use hbb_common::log;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use super::{
    capture_dxgi::{self, DxgiCapture},
    capture_gdi::GdiCapture,
    vpx_enc::VpxEncoder,
    yuv::{bgra_to_i420, bgra_to_i420_rects},
};

pub const TARGET_FPS: u64 = 60;
const FRAME_INTERVAL: Duration = Duration::from_micros(1_000_000 / TARGET_FPS);

// JPEG 폴백 품질 (0-100)
const JPEG_QUALITY: u8 = 80;

// VP9 비트레이트 (kbps): 1080p 원격 데스크톱 권장값
const BITRATE_KBPS_DEFAULT: u32 = 8000;

// DXGI 연속 실패 임계값 — 이 횟수 초과 시 전체 스택 재초기화
const DXGI_FAIL_THRESHOLD: u32 = 5;

// GDI 모드에서 DXGI 복귀 시도 간격
const DXGI_RETRY_SECS: u64 = 30;

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

// ── 캡처 모드 ────────────────────────────────────────────────────────────────

enum CaptureState {
    Dxgi(DxgiCapture),
    Gdi(GdiCapture),
}

impl CaptureState {
    fn label(&self) -> &'static str {
        match self { Self::Dxgi(_) => "DXGI", Self::Gdi(_) => "GDI" }
    }
    fn size(&self) -> (u32, u32) {
        match self {
            Self::Dxgi(c) => (c.width, c.height),
            Self::Gdi(c)  => (c.width, c.height),
        }
    }
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

// ── JPEG 인코딩 ──────────────────────────────────────────────────────────────

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

// ── 캡처 루프 ────────────────────────────────────────────────────────────────

/// 화면 캡처 루프 — spawn_blocking 안에서 동기 실행
pub fn capture_loop(tx: mpsc::Sender<VideoFrame>, session_key: String) -> Result<()> {
    log::info!("[video] 캡처 시작: {}", session_key);
    timer::begin();

    // 이전 세션 고스트 Desktop Duplication 상태 강제 회수
    capture_dxgi::reclaim_output();

    // ── DXGI 초기 시도 (3회, 빠른 실패) ─────────────────────────────────────
    // session.rs의 1500ms 대기 + 올바른 COM 해제 순서로 이미 핸들이 정리된 상태.
    // RDP 세션이나 드라이버 불안정 환경은 3회 시도로 즉시 판별 후 GDI로 전환.
    let mut state = {
        let mut dxgi = None;
        for attempt in 1..=3u8 {
            match DxgiCapture::new() {
                Ok(c) => {
                    if attempt > 1 {
                        log::info!("[video] DXGI 초기화 성공 (시도 {})", attempt);
                    }
                    dxgi = Some(c);
                    break;
                }
                Err(e) => {
                    log::warn!("[video] DXGI 초기화 실패 ({}/3): {:?}", attempt, e);
                    if attempt < 3 {
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
            }
        }
        match dxgi {
            Some(c) => {
                log::info!("[video] DXGI 모드 시작 ({}x{})", c.width, c.height);
                CaptureState::Dxgi(c)
            }
            None => {
                log::warn!("[video] ★ DXGI 불가 → GDI 모드로 시작 ({}초마다 DXGI 복귀 시도)",
                    DXGI_RETRY_SECS);
                CaptureState::Gdi(GdiCapture::new()?)
            }
        }
    };

    let (w, h) = state.size();

    let bitrate_kbps = std::env::var("VDESK_VP9_BITRATE_KBPS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(BITRATE_KBPS_DEFAULT);

    let mut encoder: Option<VpxEncoder> =
        match VpxEncoder::new(w, h, bitrate_kbps, TARGET_FPS as u32) {
        Ok(enc) => {
            log::info!("[video] VP9 인코더 초기화 완료 ({}kbps, {}fps)", bitrate_kbps, TARGET_FPS);
            Some(enc)
        }
        Err(e) => {
            log::warn!("[video] VP9 인코더 초기화 실패: {:?} → JPEG 폴백", e);
            None
        }
    };

    let mut i420_buf              = Vec::<u8>::new();
    let mut last_hash: u64        = 0;
    let mut frames: u64           = 0;
    let mut drop_count: u32       = 0;
    let mut force_keyframe_on_next = false;
    let mut dxgi_fail_streak: u32 = 0;
    let mut last_dxgi_retry       = Instant::now();
    let mut last_tick             = Instant::now();

    loop {
        if tx.is_closed() { break; }

        // FPS 제한
        let elapsed = last_tick.elapsed();
        if elapsed < FRAME_INTERVAL {
            std::thread::sleep(FRAME_INTERVAL - elapsed);
        }
        last_tick = Instant::now();

        // ── GDI 모드: 주기적 DXGI 복귀 시도 ─────────────────────────────────
        // match 블록 외부에서 실행 — state borrow 없음
        if matches!(state, CaptureState::Gdi(_))
            && last_dxgi_retry.elapsed() >= Duration::from_secs(DXGI_RETRY_SECS)
        {
            last_dxgi_retry = Instant::now();
            match DxgiCapture::new() {
                Ok(c) => {
                    log::info!("[video] DXGI 복귀 성공 ({}x{}) → DXGI 모드 전환", c.width, c.height);
                    state = CaptureState::Dxgi(c);
                    dxgi_fail_streak = 0;
                    force_keyframe_on_next = true;
                    continue;
                }
                Err(e) => {
                    log::debug!("[video] DXGI 복귀 시도 실패 ({}초 후 재시도): {:?}",
                        DXGI_RETRY_SECS, e);
                }
            }
        }

        let force_key = force_keyframe_on_next || frames % (TARGET_FPS * 10) == 0;
        force_keyframe_on_next = false;

        // ── 캡처 + 인코딩 ────────────────────────────────────────────────────
        // send_result는 Vec<u8>을 소유 — match 블록 이후 state를 자유롭게 교체 가능.
        // frame.bgra / bgra 슬라이스 borrow는 각 arm 내에서만 살아있음 (NLL).
        let send_result: Option<(Vec<u8>, Codec, bool)> = match &mut state {

            CaptureState::Dxgi(cap) => match cap.capture() {
                Ok(Some(frame)) => {
                    dxgi_fail_streak = 0;

                    // 정적 화면 스킵 (dirty_rects 기반 전체 프레임인 경우만)
                    let skip = frame.is_full_frame && frame.has_dirty_rects && {
                        let hash = fnv_sample(frame.bgra);
                        let same = hash == last_hash;
                        last_hash = hash;
                        same && !force_key
                    };
                    if skip { None } else {
                        encode_frame_dxgi(frame.bgra, frame.is_full_frame, frame.dirty_rects,
                            w, h, &mut i420_buf, &mut encoder, force_key)
                    }
                }
                Ok(None) => { dxgi_fail_streak = 0; None } // 변화 없음
                Err(e) => {
                    dxgi_fail_streak += 1;
                    log::warn!("[video] DXGI 캡처 오류 ({}회 연속): {:?}", dxgi_fail_streak, e);
                    std::thread::sleep(Duration::from_millis(100));
                    None
                }
            },

            CaptureState::Gdi(cap) => match cap.capture() {
                Ok(bgra) => {
                    let hash = fnv_sample(bgra);
                    if hash == last_hash && !force_key {
                        None // 변화 없음
                    } else {
                        last_hash = hash;
                        encode_frame_gdi(bgra, w, h, &mut i420_buf, &mut encoder, force_key)
                    }
                }
                Err(e) => {
                    log::warn!("[video] GDI 캡처 오류: {:?}", e);
                    std::thread::sleep(Duration::from_millis(200));
                    None
                }
            },
        };
        // ↑ match 블록 종료 — state borrow 해제됨

        // ── DXGI 연속 실패 → 전체 스택 재초기화 ────────────────────────────
        // DXGI handles을 완전히 drop한 뒤 GPU 드라이버에 정리 시간을 주고 재시도.
        // 재시도 실패 시 GDI 모드로 전환, 이후 30초마다 복귀 시도.
        if dxgi_fail_streak >= DXGI_FAIL_THRESHOLD {
            dxgi_fail_streak = 0;
            log::warn!("[video] DXGI {}회 연속 실패 → 전체 스택 재초기화", DXGI_FAIL_THRESHOLD);

            // DXGI Drop (ClearState + Flush + 역순 COM Release)
            match GdiCapture::new() {
                Ok(gdi) => {
                    state = CaptureState::Gdi(gdi); // ← DxgiCapture Drop 트리거
                    log::info!("[video] DXGI 핸들 해제 완료 — GPU 드라이버 정리 대기 500ms");
                    std::thread::sleep(Duration::from_millis(500));

                    match DxgiCapture::new() {
                        Ok(c) => {
                            log::info!("[video] DXGI 전체 재초기화 성공 → DXGI 모드 복귀");
                            state = CaptureState::Dxgi(c);
                        }
                        Err(e2) => {
                            log::error!("[video] DXGI 재초기화 실패 → GDI 모드 유지: {:?}", e2);
                            last_dxgi_retry = Instant::now(); // 30초 후 복귀 재시도
                        }
                    }
                }
                Err(e) => {
                    log::error!("[video] GDI 초기화 실패 — 치명적 오류: {:?}", e);
                    break;
                }
            }
            force_keyframe_on_next = true;
            continue;
        }

        // ── 프레임 전송 ──────────────────────────────────────────────────────
        let Some((encoded, codec, is_key)) = send_result else { continue };

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
                force_keyframe_on_next = true;
                if drop_count % 10 == 0 {
                    log::warn!("[video] 채널 포화 드롭 {}회 (모드: {})", drop_count, state.label());
                }
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break,
        }

        frames += 1;
        if frames % 300 == 0 {
            log::debug!("[video] {}프레임 전송 (모드: {}, 코덱: {})",
                frames, state.label(), if encoder.is_some() { "VP9" } else { "JPEG" });
        }
    }

    timer::end();
    log::info!("[video] 캡처 루프 종료");
    Ok(())
}

// ── 인코딩 헬퍼 ──────────────────────────────────────────────────────────────

fn encode_frame_dxgi<'a>(
    bgra: &[u8],
    is_full_frame: bool,
    dirty_rects: &'a [capture_dxgi::DirtyRect],
    w: u32, h: u32,
    i420_buf: &mut Vec<u8>,
    encoder: &mut Option<VpxEncoder>,
    force_key: bool,
) -> Option<(Vec<u8>, Codec, bool)> {
    if let Some(enc) = encoder.as_mut() {
        if is_full_frame {
            bgra_to_i420(bgra, w as usize, h as usize, i420_buf);
        } else {
            bgra_to_i420_rects(bgra, w as usize, h as usize, i420_buf, dirty_rects);
        }
        match enc.encode(i420_buf, force_key) {
            Ok(Some((d, k))) => Some((d.to_vec(), Codec::Vp9, k)),
            Ok(None)         => None,
            Err(e) => { log::warn!("[video] VP9 인코딩 오류: {:?}", e); None }
        }
    } else {
        match encode_jpeg(bgra, w, h) {
            Ok(d) => Some((d, Codec::Jpeg, true)),
            Err(e) => { log::warn!("[video] JPEG 인코딩 오류: {:?}", e); None }
        }
    }
}

fn encode_frame_gdi(
    bgra: &[u8],
    w: u32, h: u32,
    i420_buf: &mut Vec<u8>,
    encoder: &mut Option<VpxEncoder>,
    force_key: bool,
) -> Option<(Vec<u8>, Codec, bool)> {
    if let Some(enc) = encoder.as_mut() {
        bgra_to_i420(bgra, w as usize, h as usize, i420_buf);
        match enc.encode(i420_buf, force_key) {
            Ok(Some((d, k))) => Some((d.to_vec(), Codec::Vp9, k)),
            Ok(None)         => None,
            Err(e) => { log::warn!("[video] GDI VP9 인코딩 오류: {:?}", e); None }
        }
    } else {
        match encode_jpeg(bgra, w, h) {
            Ok(d) => Some((d, Codec::Jpeg, true)),
            Err(e) => { log::warn!("[video] GDI JPEG 인코딩 오류: {:?}", e); None }
        }
    }
}
