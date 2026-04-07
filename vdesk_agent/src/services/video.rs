//! 화면 캡처 + JPEG 압축 → VideoFrame → mpsc 채널
//!
//! 품질 전략:
//!   - FPS 30 (기본), 채널 포화 시 점진적 감소
//!   - FNV-1a 샘플 해시로 변화 없는 프레임 전송 스킵
//!   - 채널 백프레셔 기반 적응형 JPEG 품질 (40 ~ 85)
//!   - RGB 변환 버퍼 재사용으로 heap 할당 최소화

use anyhow::Result;
use hbb_common::log;
use image::{codecs::jpeg::JpegEncoder, ColorType, ImageEncoder};
use screenshots::Screen;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

pub const TARGET_FPS: u64 = 30;
const FRAME_INTERVAL: Duration = Duration::from_millis(1000 / TARGET_FPS);

// 적응형 JPEG 품질 파라미터
const QUALITY_MAX:  u8 = 85;
const QUALITY_MIN:  u8 = 40;
const QUALITY_DOWN: u8 = 10; // 채널 포화 시 품질 감소 폭
const QUALITY_UP:   u8 = 2;  // 채널 여유 시 품질 회복 폭

pub struct VideoFrame {
    pub jpeg:   Vec<u8>,
    pub width:  u32,
    pub height: u32,
    pub fps:    u8,
}

/// 화면 캡처 루프 — spawn_blocking 안에서 동기 실행
pub fn capture_loop(tx: mpsc::Sender<VideoFrame>, session_key: String) -> Result<()> {
    log::info!("[video] 캡처 시작: {}", session_key);

    let screens = Screen::all()?;
    let screen = screens
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("디스플레이를 찾을 수 없습니다"))?;

    let di = screen.display_info;
    log::info!("[video] {}x{} @{}fps", di.width, di.height, TARGET_FPS);

    let mut quality   = QUALITY_MAX;
    let mut last_hash = 0u64;
    let mut frames    = 0u64;
    let mut last_tick = Instant::now();

    // 재사용 버퍼: RGBA → RGB 변환용 (heap 재할당 방지)
    let mut rgb_buf = Vec::<u8>::with_capacity((di.width * di.height * 3) as usize);
    // JPEG 출력 크기 힌트 (이전 프레임 크기로 초기화 → 재할당 감소)
    let mut jpeg_size_hint = (di.width * di.height / 4) as usize;

    loop {
        if tx.is_closed() {
            break;
        }

        // ── FPS 제한 ──────────────────────────────────────────────────────
        let elapsed = last_tick.elapsed();
        if elapsed < FRAME_INTERVAL {
            std::thread::sleep(FRAME_INTERVAL - elapsed);
        }
        last_tick = Instant::now();

        // ── 화면 캡처 ────────────────────────────────────────────────────
        let cap = match screen.capture() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("[video] 캡처 오류: {:?}", e);
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
        };

        let rgba = cap.as_raw();
        let w    = cap.width();
        let h    = cap.height();

        // ── 변화 감지: FNV 샘플 해시 (변화 없는 프레임 스킵) ─────────────
        let hash = fnv_sample(rgba);
        if hash == last_hash {
            continue;
        }
        last_hash = hash;

        // ── 적응형 품질 조절 (채널 용량 기반) ────────────────────────────
        //   tx.capacity() = 남은 공간 / tx.max_capacity() = 최대 용량
        let remaining = tx.capacity();
        let max_cap   = tx.max_capacity();
        if remaining == 0 {
            // 채널 꽉 참 → 품질 빠르게 낮춤
            quality = quality.saturating_sub(QUALITY_DOWN).max(QUALITY_MIN);
        } else if remaining == max_cap && quality < QUALITY_MAX {
            // 채널 완전 비어 있음 → 품질 천천히 회복
            quality = (quality + QUALITY_UP).min(QUALITY_MAX);
        }

        // ── RGBA → RGB 변환 (버퍼 재사용) ───────────────────────────────
        let needed = (w * h * 3) as usize;
        rgb_buf.clear();
        if rgb_buf.capacity() < needed {
            rgb_buf.reserve(needed - rgb_buf.capacity());
        }
        for px in rgba.chunks_exact(4) {
            rgb_buf.push(px[0]);
            rgb_buf.push(px[1]);
            rgb_buf.push(px[2]);
        }

        // ── JPEG 인코딩 (크기 힌트로 재할당 최소화) ──────────────────────
        let mut jpeg_buf = Vec::with_capacity(jpeg_size_hint);
        if let Err(e) = JpegEncoder::new_with_quality(&mut jpeg_buf, quality)
            .write_image(&rgb_buf, w, h, ColorType::Rgb8.into())
        {
            log::warn!("[video] JPEG 인코딩 오류: {:?}", e);
            continue;
        }
        jpeg_size_hint = jpeg_buf.len(); // 다음 프레임 힌트 업데이트

        if tx.blocking_send(VideoFrame {
            jpeg:   jpeg_buf,
            width:  w,
            height: h,
            fps:    TARGET_FPS as u8,
        })
        .is_err()
        {
            break;
        }

        frames += 1;
        if frames % 300 == 0 {
            log::debug!("[video] {}프레임 전송 (quality={})", frames, quality);
        }
    }

    log::info!("[video] 캡처 루프 종료");
    Ok(())
}

/// FNV-1a 기반 프레임 샘플 해시
/// 128바이트(32픽셀×4채널)마다 1바이트 샘플 → 전체 픽셀의 ~0.8% 검사
/// 의존성 없이 충분히 빠르고 충돌률이 낮음
fn fnv_sample(data: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME:  u64 = 0x100000001b3;
    let mut h = FNV_OFFSET;
    for &b in data.iter().step_by(128) {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}
