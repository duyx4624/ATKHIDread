//! 开机自启：写 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`。
//!
//! 只动当前用户、只写一个值，不需要管理员权限，也不需要计划任务。
//! 注册的命令里必须带上**绝对** `--data-dir`：登录时进程的工作目录不是 exe 所在目录，
//! 用相对路径会把数据写到 System32 去。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
    REG_VALUE_TYPE,
};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "hidread";

/// 写入自启项，返回实际注册的命令行。
pub fn install(data_dir: &Path) -> Result<String> {
    let exe = std::env::current_exe().context("取可执行文件路径失败")?;
    let exe = strip_extended(exe);
    let data = std::path::absolute(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    let data = strip_extended(data);

    let cmd = format!(
        "\"{}\" serve --data-dir \"{}\" --autostart",
        exe.display(),
        data.display()
    );
    set_value(&cmd)?;
    Ok(cmd)
}

/// 删除自启项；原本不存在时返回 `false`。
pub fn uninstall() -> Result<bool> {
    let sub = wide(RUN_KEY);
    let name = wide(VALUE_NAME);

    unsafe {
        let mut hkey = HKEY::default();
        match RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(sub.as_ptr()),
            0,
            KEY_SET_VALUE,
            &mut hkey,
        )
        .ok()
        {
            Ok(()) => {}
            Err(_) => return Ok(false),
        }

        let res = RegDeleteValueW(hkey, PCWSTR(name.as_ptr()));
        let _ = RegCloseKey(hkey);

        if res == ERROR_FILE_NOT_FOUND {
            return Ok(false);
        }
        res.ok().context("删除自启项失败")?;
        Ok(true)
    }
}

/// 读取当前自启命令；未安装返回 `None`。
pub fn status() -> Result<Option<String>> {
    let sub = wide(RUN_KEY);
    let name = wide(VALUE_NAME);

    unsafe {
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(sub.as_ptr()),
            0,
            KEY_QUERY_VALUE,
            &mut hkey,
        )
        .ok()
        .is_err()
        {
            return Ok(None);
        }

        let mut kind = REG_VALUE_TYPE::default();
        let mut size = 0u32;
        let probe = RegQueryValueExW(
            hkey,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut kind),
            None,
            Some(&mut size),
        );
        if probe != windows::Win32::Foundation::ERROR_SUCCESS {
            let _ = RegCloseKey(hkey);
            return Ok(None);
        }

        let mut buf = vec![0u8; size as usize];
        let read = RegQueryValueExW(
            hkey,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut kind),
            Some(buf.as_mut_ptr()),
            Some(&mut size),
        );
        let _ = RegCloseKey(hkey);
        if read != windows::Win32::Foundation::ERROR_SUCCESS {
            return Ok(None);
        }

        let wide_str: Vec<u16> = buf
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|c| *c != 0)
            .collect();

        Ok(Some(String::from_utf16_lossy(&wide_str)))
    }
}

fn set_value(cmd: &str) -> Result<()> {
    let sub = wide(RUN_KEY);
    let name = wide(VALUE_NAME);
    let mut data: Vec<u8> = cmd
        .encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(|u| u.to_le_bytes())
        .collect();

    unsafe {
        let mut hkey = HKEY::default();
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(sub.as_ptr()),
            0,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut hkey,
            None,
        )
        .ok()
        .context("打开/创建 Run 注册表项失败")?;

        let res = RegSetValueExW(hkey, PCWSTR(name.as_ptr()), 0, REG_SZ, Some(&data));
        let _ = RegCloseKey(hkey);
        data.clear();
        res.ok().context("写入 Run 值失败")?;
        Ok(())
    }
}

/// 去掉 canonicalize 带来的 `\\?\` 前缀，注册表里更好看（CreateProcess 两种都认）。
fn strip_extended(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => p,
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
