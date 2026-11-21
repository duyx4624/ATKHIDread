//! 启动提示：手动双击启动时，在右下角弹一个托盘气泡。
//!
//! 不引入通知库，直接用 `Shell_NotifyIcon` 的 `NIF_INFO`。气泡需要一个窗口句柄，
//! 这里创建一个 message-only 窗口（不参与消息循环，也不显示）。
//! 图标在几秒后自动移除，避免托盘区多出一个临时图标。

use std::time::Duration;

use anyhow::Result;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_INFO, NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, RegisterClassW, HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE,
    WNDCLASSW,
};

const UID: u32 = 0x4849; // 'HI'

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    DefWindowProcW(hwnd, msg, w, l)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn put(dst: &mut [u16], s: &str) {
    let w: Vec<u16> = s.encode_utf16().collect();
    let n = w.len().min(dst.len().saturating_sub(1));
    dst[..n].copy_from_slice(&w[..n]);
    dst[n] = 0;
}

/// 弹一个信息气泡。失败时静默返回 `Err`，调用方决定是否忽略。
pub fn balloon(title: &str, body: &str) -> Result<()> {
    unsafe {
        let hinst = GetModuleHandleW(None)?;
        let class = wide("hidread-notice-window");
        let wc = WNDCLASSW {
            hInstance: HINSTANCE(hinst.0),
            lpszClassName: PCWSTR(class.as_ptr()),
            lpfnWndProc: Some(wndproc),
            ..Default::default()
        };
        // 重复注册会失败，忽略即可（类已存在）
        let _ = RegisterClassW(&wc);

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR(wide("").as_ptr()),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            None,
            None,
            None,
        )?;

        let mut nid = NOTIFYICONDATAW::default();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = UID;
        nid.uFlags = NIF_INFO;
        nid.dwInfoFlags = NIIF_INFO;
        put(&mut nid.szInfoTitle, title);
        put(&mut nid.szInfo, body);

        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);

        // 气泡停留几秒后移除临时图标；窗口随进程退出回收，不在这里销毁
        // HWND 不是 Send，这里只把裸值传进线程
        let raw = hwnd.0 as isize;
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(8));
            let mut nid = NOTIFYICONDATAW::default();
            nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = HWND(raw as *mut std::ffi::c_void);
            nid.uID = UID;
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        });
    }
    Ok(())
}
