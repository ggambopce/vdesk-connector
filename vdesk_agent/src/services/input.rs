//! 입력 주입 — Windows SendInput API
//!
//! 마우스 좌표: 뷰어가 보낸 창 크기(win_w, win_h)로 실시간 스케일 계산.
//! 스크롤: MOUSEEVENTF_WHEEL(세로) / MOUSEEVENTF_HWHEEL(가로), 단위 = Windows notch(120).
//! 키보드: 글로벌 훅 경유 VK 코드 주입. 한/영(VK_HANGUL=0x15), 한자(VK_HANJA=0x19) 키는
//!         KEYEVENTF_EXTENDEDKEY 없이 주입해야 Korean IME가 토글 이벤트로 인식한다.
//!
//! 로컬 테스트 모드 (AGENT_NO_INJECT=1 또는 루프백 IP):
//!   에이전트와 뷰어가 같은 PC에서 실행될 때 입력 주입을 비활성화합니다.
//!   같은 PC에서 SendInput을 호출하면 뷰어 마우스 ↔ 주입 커서 피드백 루프가 발생하므로
//!   화면 스트리밍 테스트 시에는 주입을 끄는 것이 올바른 동작입니다.

use std::sync::atomic::{AtomicBool, Ordering};

static NO_INJECT: AtomicBool = AtomicBool::new(false);

/// 입력 주입 전역 비활성화 — 같은 PC 루프백 테스트 시 호출
pub fn set_no_inject(v: bool) {
    NO_INJECT.store(v, Ordering::Relaxed);
}

#[cfg(target_os = "windows")]
mod win {
    use hbb_common::log;
    use std::mem;
    use winapi::um::winuser::{
        GetSystemMetrics, SendInput,
        INPUT, INPUT_KEYBOARD, INPUT_MOUSE,
        KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
        MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
        MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
        MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL,
        MOUSEINPUT, SM_CXSCREEN, SM_CYSCREEN,
    };

    // MOUSEEVENTF_HWHEEL = 0x1000 (winapi 0.3.x에 없는 경우 직접 정의)
    const MOUSEEVENTF_HWHEEL: u32 = 0x1000;
    /// 가상 데스크톱 전체에 절대 좌표 매핑 (멀티 모니터)
    const MOUSEEVENTF_VIRTUALDESK: u32 = 0x4000;

    /// SendInput dwExtraInfo 마커: 에이전트가 주입한 이벤트임을 표시
    /// 뷰어가 이 마커를 보면 자신에게 돌아온 주입 이벤트로 판단해 무시 (피드백 루프 차단)
    pub const VDESK_INPUT_MARK: usize = 0x5DEC_0001;

    const SM_XVIRTUALSCREEN: i32 = 76;
    const SM_YVIRTUALSCREEN: i32 = 77;
    const SM_CXVIRTUALSCREEN: i32 = 78;
    const SM_CYVIRTUALSCREEN: i32 = 79;

    fn screen_size() -> (i32, i32) {
        unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) }
    }

    /// 뷰어 창 좌표(vx, vy)를 에이전트 화면 절대 좌표로 변환하여 주입
    /// win_w / win_h: 뷰어가 실시간으로 보내는 창 크기
    pub fn inject_mouse_move(vx: i32, vy: i32, win_w: i32, win_h: i32) {
        if super::NO_INJECT.load(super::Ordering::Relaxed) {
            return;
        }
        if win_w <= 0 || win_h <= 0 {
            return;
        }
        let (sw, sh) = screen_size();
        // 뷰어 창 → 에이전트 화면 → Windows 절대 좌표(0-65535)
        let sx = (vx * sw / win_w).clamp(0, sw - 1);
        let sy = (vy * sh / win_h).clamp(0, sh - 1);
        let abs_x = sx * 65535 / sw;
        let abs_y = sy * 65535 / sh;

        unsafe {
            let mut inp: INPUT = mem::zeroed();
            inp.type_ = INPUT_MOUSE;
            *inp.u.mi_mut() = MOUSEINPUT {
                dx: abs_x,
                dy: abs_y,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
                time: 0,
                dwExtraInfo: VDESK_INPUT_MARK,
            };
            SendInput(1, &mut inp, mem::size_of::<INPUT>() as i32);
        }
    }

    /// 뷰어가 계산한 가상 화면 기준 물리 픽셀 (inner_position + 클라이언트 좌표)
    pub fn inject_mouse_move_global(gx: i32, gy: i32) {
        if super::NO_INJECT.load(super::Ordering::Relaxed) {
            return;
        }
        let vx = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
        let vy = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
        let vw = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) }.max(1);
        let vh = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) }.max(1);

        let rx = gx.clamp(vx, vx.saturating_add(vw).saturating_sub(1));
        let ry = gy.clamp(vy, vy.saturating_add(vh).saturating_sub(1));

        let abs_x = if vw > 1 {
            (rx - vx) as i64 * 65535 / (vw - 1) as i64
        } else {
            0
        }
        .clamp(0, 65535) as i32;
        let abs_y = if vh > 1 {
            (ry - vy) as i64 * 65535 / (vh - 1) as i64
        } else {
            0
        }
        .clamp(0, 65535) as i32;

        unsafe {
            let mut inp: INPUT = mem::zeroed();
            inp.type_ = INPUT_MOUSE;
            *inp.u.mi_mut() = MOUSEINPUT {
                dx: abs_x,
                dy: abs_y,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE
                    | MOUSEEVENTF_ABSOLUTE
                    | MOUSEEVENTF_VIRTUALDESK,
                time: 0,
                dwExtraInfo: VDESK_INPUT_MARK,
            };
            SendInput(1, &mut inp, mem::size_of::<INPUT>() as i32);
        }
    }

    pub fn inject_mouse_button(button: u8, pressed: bool) {
        if super::NO_INJECT.load(super::Ordering::Relaxed) {
            return;
        }
        let flags = match (button, pressed) {
            (0, true)  => MOUSEEVENTF_LEFTDOWN,
            (0, false) => MOUSEEVENTF_LEFTUP,
            (2, true)  => MOUSEEVENTF_RIGHTDOWN,
            (2, false) => MOUSEEVENTF_RIGHTUP,
            (4, true)  => MOUSEEVENTF_MIDDLEDOWN,
            (4, false) => MOUSEEVENTF_MIDDLEUP,
            _ => return,
        };
        unsafe {
            let mut inp: INPUT = mem::zeroed();
            inp.type_ = INPUT_MOUSE;
            *inp.u.mi_mut() = MOUSEINPUT {
                dx: 0, dy: 0, mouseData: 0, dwFlags: flags, time: 0,
                dwExtraInfo: VDESK_INPUT_MARK,
            };
            SendInput(1, &mut inp, mem::size_of::<INPUT>() as i32);
        }
    }

    /// 세로/가로 스크롤 주입
    /// dy > 0 = 위로, dy < 0 = 아래로 (Windows 관례)
    pub fn inject_scroll(dx: i16, dy: i16) {
        if super::NO_INJECT.load(super::Ordering::Relaxed) {
            return;
        }
        unsafe {
            if dy != 0 {
                let mut inp: INPUT = mem::zeroed();
                inp.type_ = INPUT_MOUSE;
                *inp.u.mi_mut() = MOUSEINPUT {
                    dx: 0, dy: 0,
                    mouseData: dy as u32,
                    dwFlags: MOUSEEVENTF_WHEEL,
                    time: 0, dwExtraInfo: VDESK_INPUT_MARK,
                };
                SendInput(1, &mut inp, mem::size_of::<INPUT>() as i32);
            }
            if dx != 0 {
                let mut inp: INPUT = mem::zeroed();
                inp.type_ = INPUT_MOUSE;
                *inp.u.mi_mut() = MOUSEINPUT {
                    dx: 0, dy: 0,
                    mouseData: dx as u32,
                    dwFlags: MOUSEEVENTF_HWHEEL,
                    time: 0, dwExtraInfo: VDESK_INPUT_MARK,
                };
                SendInput(1, &mut inp, mem::size_of::<INPUT>() as i32);
            }
        }
    }

    /// VK 코드 + scan 코드로 직접 키 주입 (글로벌 훅 경유)
    pub fn inject_key_vk(vk: u32, scan: u16, pressed: bool, extended: bool) {
        if super::NO_INJECT.load(super::Ordering::Relaxed) {
            return;
        }

        // VK_HANGUL(0x15), VK_HANJA(0x19) 는 KEYEVENTF_EXTENDEDKEY 없이 주입해야
        // Windows Korean IME 가 토글 이벤트로 인식한다.
        // 한국 키보드의 한/영·한자 키는 E0 접두어 확장 스캔코드이므로 훅에서
        // extended=true 로 오지만, SendInput 에서는 플래그를 제거해야 동작한다.
        const VK_HANGUL: u32 = 0x15;
        const VK_HANJA:  u32 = 0x19;
        let is_ime_toggle = vk == VK_HANGUL || vk == VK_HANJA;

        let mut flags = if pressed { 0 } else { KEYEVENTF_KEYUP };
        if extended && !is_ime_toggle { flags |= KEYEVENTF_EXTENDEDKEY; }

        unsafe {
            let mut inp: INPUT = mem::zeroed();
            inp.type_ = INPUT_KEYBOARD;

            *inp.u.ki_mut() = KEYBDINPUT {
                wVk:         vk as u16,
                wScan:       scan,
                dwFlags:     flags,
                time:        0,
                dwExtraInfo: VDESK_INPUT_MARK,
            };

            SendInput(1, &mut inp, mem::size_of::<INPUT>() as i32);
        }
    }

    pub fn inject_key(keycode_u32: u32, pressed: bool) {
        if super::NO_INJECT.load(super::Ordering::Relaxed) {
            return;
        }
        let vk = winit_keycode_to_vk(keycode_u32);
        if vk == 0 {
            log::trace!("[input] 알 수 없는 키코드: {}", keycode_u32);
            return;
        }

        // 확장 키 여부 (화살표, Insert, Delete, Home 등)
        let extended = matches!(
            keycode_u32,
            79  // ArrowDown
            | 80  // ArrowLeft
            | 81  // ArrowRight
            | 82  // ArrowUp
            | 72  // Delete
            | 73  // End
            | 75  // Home
            | 76  // Insert
            | 77  // PageDown
            | 78  // PageUp
            | 51  // AltRight
            | 56  // ControlRight
            | 100 // NumpadDivide
            | 101 // NumpadEnter
        );

        let mut flags = if pressed { 0 } else { KEYEVENTF_KEYUP };
        if extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }

        // VK → scan code 변환 (MapVirtualKey) — scan code 없이 VK만 보내면
        // 일부 앱에서 오작동하므로 scan code도 같이 전달한다.
        let scan = unsafe {
            extern "system" { fn MapVirtualKeyW(uCode: u32, uMapType: u32) -> u32; }
            MapVirtualKeyW(vk, 0) as u16 // MAPVK_VK_TO_VSC = 0
        };

        unsafe {
            let mut inp: INPUT = mem::zeroed();
            inp.type_ = INPUT_KEYBOARD;
            *inp.u.ki_mut() = KEYBDINPUT {
                wVk:         vk as u16,
                wScan:       scan,
                dwFlags:     flags,
                time:        0,
                dwExtraInfo: VDESK_INPUT_MARK,
            };
            SendInput(1, &mut inp, mem::size_of::<INPUT>() as i32);
        }
    }

    /// winit 0.30 KeyCode discriminant → Windows Virtual Key code
    ///
    /// 열거형 순서 (winit 0.30.13 기준):
    ///   0=Backquote, 1=Backslash, 2=BracketLeft, 3=BracketRight, 4=Comma,
    ///   5-14=Digit0-9, 15=Equal, 19-44=KeyA-Z, 45=Minus, 46=Period,
    ///   47=Quote, 48=Semicolon, 49=Slash,
    ///   50=AltLeft, 51=AltRight, 52=Backspace, 53=CapsLock, 54=ContextMenu,
    ///   55=ControlLeft, 56=ControlRight, 57=Enter, 58=SuperLeft, 59=SuperRight,
    ///   60=ShiftLeft, 61=ShiftRight, 62=Space, 63=Tab,
    ///   72=Delete, 73=End, 75=Home, 76=Insert, 77=PageDown, 78=PageUp,
    ///   79=ArrowDown, 80=ArrowLeft, 81=ArrowRight, 82=ArrowUp,
    ///   83=NumLock, 84-93=Numpad0-9, 94=NumpadAdd, 99=NumpadDecimal,
    ///   100=NumpadDivide, 101=NumpadEnter, 109=NumpadMultiply, 113=NumpadSubtract,
    ///   114=Escape, 117=PrintScreen, 118=ScrollLock, 119=Pause,
    ///   159-170=F1-F12
    fn winit_keycode_to_vk(code: u32) -> u32 {
        match code {
            // ── 문자 키 ──────────────────────────────────────────────────────
            0  => 0xC0, // Backquote  `~
            1  => 0xDC, // Backslash  \|
            2  => 0xDB, // BracketLeft  [{
            3  => 0xDD, // BracketRight  ]}
            4  => 0xBC, // Comma  ,<
            // Digit0-9 → VK '0'-'9'
            5  => 0x30, 6  => 0x31, 7  => 0x32, 8  => 0x33, 9  => 0x34,
            10 => 0x35, 11 => 0x36, 12 => 0x37, 13 => 0x38, 14 => 0x39,
            15 => 0xBB, // Equal  =+
            // KeyA-Z → VK 'A'-'Z'
            19 => 0x41, 20 => 0x42, 21 => 0x43, 22 => 0x44, 23 => 0x45,
            24 => 0x46, 25 => 0x47, 26 => 0x48, 27 => 0x49, 28 => 0x4A,
            29 => 0x4B, 30 => 0x4C, 31 => 0x4D, 32 => 0x4E, 33 => 0x4F,
            34 => 0x50, 35 => 0x51, 36 => 0x52, 37 => 0x53, 38 => 0x54,
            39 => 0x55, 40 => 0x56, 41 => 0x57, 42 => 0x58, 43 => 0x59,
            44 => 0x5A,
            45 => 0xBD, // Minus  -_
            46 => 0xBE, // Period  .>
            47 => 0xDE, // Quote  '"
            48 => 0xBA, // Semicolon  ;:
            49 => 0xBF, // Slash  /?
            // ── 수정 키 ──────────────────────────────────────────────────────
            50 => 0xA4, // AltLeft    VK_LMENU
            51 => 0xA5, // AltRight   VK_RMENU
            52 => 0x08, // Backspace  VK_BACK
            53 => 0x14, // CapsLock   VK_CAPITAL
            54 => 0x5D, // ContextMenu VK_APPS
            55 => 0xA2, // ControlLeft  VK_LCONTROL
            56 => 0xA3, // ControlRight VK_RCONTROL
            57 => 0x0D, // Enter      VK_RETURN
            58 => 0x5B, // SuperLeft  VK_LWIN
            59 => 0x5C, // SuperRight VK_RWIN
            60 => 0xA0, // ShiftLeft  VK_LSHIFT
            61 => 0xA1, // ShiftRight VK_RSHIFT
            62 => 0x20, // Space      VK_SPACE
            63 => 0x09, // Tab        VK_TAB
            // ── 편집 키 ──────────────────────────────────────────────────────
            72 => 0x2E, // Delete    VK_DELETE
            73 => 0x23, // End       VK_END
            74 => 0x2F, // Help      VK_HELP
            75 => 0x24, // Home      VK_HOME
            76 => 0x2D, // Insert    VK_INSERT
            77 => 0x22, // PageDown  VK_NEXT
            78 => 0x21, // PageUp    VK_PRIOR
            // ── 방향 키 ──────────────────────────────────────────────────────
            79 => 0x28, // ArrowDown   VK_DOWN
            80 => 0x25, // ArrowLeft   VK_LEFT
            81 => 0x27, // ArrowRight  VK_RIGHT
            82 => 0x26, // ArrowUp     VK_UP
            // ── 숫자패드 ─────────────────────────────────────────────────────
            83 => 0x90, // NumLock      VK_NUMLOCK
            84 => 0x60, 85 => 0x61, 86 => 0x62, 87 => 0x63, 88 => 0x64,
            89 => 0x65, 90 => 0x66, 91 => 0x67, 92 => 0x68, 93 => 0x69,
            94  => 0x6B, // NumpadAdd        VK_ADD
            99  => 0x6E, // NumpadDecimal    VK_DECIMAL
            100 => 0x6F, // NumpadDivide     VK_DIVIDE
            101 => 0x0D, // NumpadEnter      VK_RETURN (extended)
            109 => 0x6A, // NumpadMultiply   VK_MULTIPLY
            113 => 0x6D, // NumpadSubtract   VK_SUBTRACT
            // ── 시스템 키 ────────────────────────────────────────────────────
            114 => 0x1B, // Escape       VK_ESCAPE
            117 => 0x2C, // PrintScreen  VK_SNAPSHOT
            118 => 0x91, // ScrollLock   VK_SCROLL
            119 => 0x13, // Pause        VK_PAUSE
            // ── 미디어/브라우저 키 ────────────────────────────────────────────
            120 => 0xA6, 121 => 0xAB, 122 => 0xA7, 123 => 0xAC,
            124 => 0xA8, 125 => 0xAA, 126 => 0xA9,
            128 => 0xB6, 129 => 0xB7, 130 => 0xB4, 131 => 0xB3,
            132 => 0xB5, 133 => 0xB2, 134 => 0xB0, 135 => 0xB1,
            137 => 0x5F, 138 => 0xAE, 139 => 0xAD, 140 => 0xAF,
            // ── 기능 키 F1-F24 ───────────────────────────────────────────────
            159 => 0x70, 160 => 0x71, 161 => 0x72, 162 => 0x73,
            163 => 0x74, 164 => 0x75, 165 => 0x76, 166 => 0x77,
            167 => 0x78, 168 => 0x79, 169 => 0x7A, 170 => 0x7B,
            171 => 0x7C, 172 => 0x7D, 173 => 0x7E, 174 => 0x7F,
            175 => 0x80, 176 => 0x81, 177 => 0x82, 178 => 0x83,
            179 => 0x84, 180 => 0x85, 181 => 0x86, 182 => 0x87,
            _ => 0,
        }
    }
}

// ── 플랫폼별 공개 API ─────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
pub use win::{
    inject_key, inject_key_vk, inject_mouse_button, inject_mouse_move,
    inject_mouse_move_global, inject_scroll,
};

#[cfg(not(target_os = "windows"))]
pub fn inject_mouse_move(_vx: i32, _vy: i32, _win_w: i32, _win_h: i32) {}
#[cfg(not(target_os = "windows"))]
pub fn inject_mouse_move_global(_gx: i32, _gy: i32) {}
#[cfg(not(target_os = "windows"))]
pub fn inject_mouse_button(_button: u8, _pressed: bool) {}
#[cfg(not(target_os = "windows"))]
pub fn inject_key(_key: u32, _pressed: bool) {}
#[cfg(not(target_os = "windows"))]
pub fn inject_key_vk(_vk: u32, _scan: u16, _pressed: bool, _extended: bool) {}
#[cfg(not(target_os = "windows"))]
pub fn inject_scroll(_dx: i16, _dy: i16) {}
