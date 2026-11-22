//! 系统托盘：双击启动后不显示控制台，只在托盘留一个图标。
//!
//! Windows 上 `tray-icon` 依赖窗口消息循环，所以托盘必须建在主线程，
//! HTTP 服务与节拍线程各自跑在后台线程。

use anyhow::{Context, Result};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};
use windows::core::PCWSTR;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, TranslateMessage, MSG, SW_SHOWNORMAL,
};

pub fn run(url: String, notify: bool) -> Result<()> {
    let menu = Menu::new();
    let open = MenuItem::new("打开设置页", true, None);
    let sep = PredefinedMenuItem::separator();
    let restart = MenuItem::new("重启服务", true, None);
    let quit = MenuItem::new("退出", true, None);
    menu.append_items(&[&open, &sep, &restart, &quit])
        .context("构建托盘菜单失败")?;

    let open_id = open.id().clone();
    let restart_id = restart.id().clone();
    let quit_id = quit.id().clone();

    let _tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false) // 左键留给双击打开页面，右键仍出菜单
        .with_tooltip("hidread — ATK 鼠标电量")
        .with_icon(battery_icon()?)
        .build()
        .context("创建托盘图标失败（explorer 未运行？）")?;

    // 双击图标直接打开设置页
    let url_open = url.clone();
    std::thread::spawn(move || {
        let rx = tray_icon::TrayIconEvent::receiver();
        while let Ok(ev) = rx.recv() {
            if matches!(ev, tray_icon::TrayIconEvent::DoubleClick { .. }) {
                open_url(&url_open);
            }
        }
    });

    // 只有手动双击启动才提示；开机自启靠 --autostart 跳过。
    // 必须在本线程创建（消息循环随后在下方启动），不能另开线程。
    if notify {
        crate::notice::toast("hidread 已启动", &format!("设置页 {url}"), 4);
    }

    // 菜单事件单独一个线程，避免阻塞消息循环
    let url_cl = url.clone();
    std::thread::spawn(move || {
        let rx = MenuEvent::receiver();
        while let Ok(ev) = rx.recv() {
            if ev.id == open_id {
                open_url(&url_cl);
            } else if ev.id == restart_id {
                if let Err(e) = crate::restart() {
                    eprintln!("手动重启失败：{e:#}");
                }
            } else if ev.id == quit_id {
                std::process::exit(0);
            }
        }
    });

    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

/// 用默认浏览器打开本地状态页。
fn open_url(url: &str) {
    let verb = to_wide("open");
    let target = to_wide(url);
    unsafe {
        let h = ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(target.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
        // ShellExecuteW 返回值 > 32 表示成功
        let _ = h;
    }
}

/// 画一个 32×32 的电池图标（无外部资源文件）。
fn battery_icon() -> Result<Icon> {
    const S: usize = 32;
    let mut rgba = vec![0u8; S * S * 4];

    let mut put = |x: usize, y: usize, c: [u8; 4]| {
        let i = (y * S + x) * 4;
        rgba[i..i + 4].copy_from_slice(&c);
    };

    let ink = [24u8, 68, 124, 255];
    let bar = [55u8, 138, 221, 255];

    // 外壳（2px 边）
    for y in 8..24 {
        for x in 3..25 {
            let edge = y <= 9 || y >= 22 || x <= 4 || x >= 23;
            put(x, y, if edge { ink } else { [255, 255, 255, 255] });
        }
    }
    // 正极
    for y in 13..19 {
        for x in 25..28 {
            put(x, y, ink);
        }
    }
    // 电量条（约 75%）
    for y in 11..21 {
        for x in 7..20 {
            put(x, y, bar);
        }
    }

    Icon::from_rgba(rgba, S as u32, S as u32).context("图标数据无效")
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
