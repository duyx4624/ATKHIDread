//! 启动提示：在屏幕右下角弹一个自绘小窗，几秒后自动消失。
//!
//! 不用 `Shell_NotifyIcon`：托盘气泡要求普通窗口句柄，message-only 窗口只会在
//! 托盘区留下一个空白图标，气泡却不出现（实测踩过）。自绘 `WS_POPUP` 窗口反而
//! 更可控，也不受通知中心 / 勿扰模式影响。
//!
//! 窗口创建后**不自带消息循环**：由调用方（托盘线程）的 `GetMessage` 分发，
//! 定时器到点发 `WM_TIMER`，在窗口过程里销毁自己。

use std::sync::Once;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect,
    SelectObject, SetBkMode, SetTextColor, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET,
    DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, FW_NORMAL, HDC,
    HGDIOBJ, OUT_TT_PRECIS, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetSystemMetrics, GetWindowLongPtrW,
    KillTimer, RegisterClassW, SetTimer, SetWindowLongPtrW, ShowWindow, GWLP_USERDATA, SM_CXSCREEN,
    SM_CYSCREEN, SW_SHOWNOACTIVATE, WM_DESTROY, WM_PAINT, WM_TIMER, WNDCLASSW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

const CLASS: &str = "hidread-toast";
const WIDTH: i32 = 300;
const HEIGHT: i32 = 72;
const PAD: i32 = 12;
static REGISTER: Once = Once::new();

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

struct Text {
    title: Vec<u16>,
    body: Vec<u16>,
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            paint(hwnd, hdc);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_TIMER => {
            let _ = KillTimer(hwnd, 1);
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Text;
            if !ptr.is_null() {
                drop(Box::from_raw(ptr));
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}

unsafe fn paint(hwnd: HWND, hdc: HDC) {
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);

    // 深色底
    let bg = CreateSolidBrush(COLORREF(0x0026_2523));
    FillRect(hdc, &rc, bg);
    let _ = DeleteObject(HGDIOBJ(bg.0));

    // 左侧一条强调色
    let accent = CreateSolidBrush(COLORREF(0x0030_8F4C));
    let bar = RECT {
        left: 0,
        top: 0,
        right: 4,
        bottom: rc.bottom,
    };
    FillRect(hdc, &bar, accent);
    let _ = DeleteObject(HGDIOBJ(accent.0));

    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Text;
    if ptr.is_null() {
        return;
    }
    let text = &*ptr;

    let _ = SetBkMode(hdc, TRANSPARENT);
    // 用微软雅黑 + ClearType，避免默认点阵字体的锯齿感
    let face = wide("Microsoft YaHei UI");
    let mut font = CreateFontW(
        -14,
        0,
        0,
        0,
        FW_NORMAL.0 as i32,
        0,
        0,
        0,
        DEFAULT_CHARSET.0 as u32,
        OUT_TT_PRECIS.0 as u32,
        CLIP_DEFAULT_PRECIS.0 as u32,
        CLEARTYPE_QUALITY.0 as u32,
        0,
        PCWSTR(face.as_ptr()),
    );
    let _ = SelectObject(hdc, HGDIOBJ(font.0));

    let mut r1 = RECT {
        left: PAD,
        top: PAD - 5,
        right: WIDTH - PAD,
        bottom: PAD + 17,
    };
    let _ = SetTextColor(hdc, COLORREF(0x00F2_F2F2));
    let mut t = text.title.clone();
    DrawTextW(hdc, &mut t, &mut r1, DT_LEFT | DT_SINGLELINE | DT_NOPREFIX);

    let mut r2 = RECT {
        left: PAD,
        top: PAD + 17,
        right: WIDTH - PAD,
        bottom: HEIGHT - PAD + 2,
    };
    let _ = SetTextColor(hdc, COLORREF(0x00B4_BDC6));
    let mut b = text.body.clone();
    DrawTextW(
        hdc,
        &mut b,
        &mut r2,
        DT_LEFT | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
    );

    let _ = DeleteObject(HGDIOBJ(font.0));
}

/// 在右下角显示一条提示，`secs` 秒后自动关闭。
///
/// 必须在将要运行消息循环的线程上调用（托盘线程）。
pub fn toast(title: &str, body: &str, secs: u32) {
    unsafe {
        let Ok(hinst) = GetModuleHandleW(None) else {
            return;
        };

        REGISTER.call_once(|| {
            let class = wide(CLASS);
            let wc = WNDCLASSW {
                hInstance: HINSTANCE(hinst.0),
                lpszClassName: PCWSTR(class.as_ptr()),
                lpfnWndProc: Some(wndproc),
                ..Default::default()
            };
            let _ = RegisterClassW(&wc);
        });

        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        // 任务栏约 48px，再留 16px 边距
        let x = sw - WIDTH - 16;
        let y = sh - HEIGHT - 56;

        let class = wide(CLASS);
        let caption = wide(title);
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            PCWSTR(class.as_ptr()),
            PCWSTR(caption.as_ptr()),
            WS_POPUP,
            x,
            y,
            WIDTH,
            HEIGHT,
            None,
            None,
            None,
            None,
        ) else {
            return;
        };

        let text = Box::new(Text {
            title: wide(title),
            body: wide(body),
        });
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(text) as isize);

        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let _ = SetTimer(hwnd, 1, secs * 1000, None);
    }
}
