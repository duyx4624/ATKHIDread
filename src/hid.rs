//! Windows HID 访问层：SetupAPI 枚举 + 双句柄读写。
//!
//! 为什么是双句柄：同一句柄上并发/阻塞的 `ReadFile` 会和 `WriteFile`、IOCTL 互锁
//! （参考实现 `my_devices.py:327-330` 明确记录了这个坑）。因此：
//! - 写句柄：同步打开，只用于 `WriteFile` 输出报告；
//! - 读句柄：`FILE_FLAG_OVERLAPPED`，靠事件 + `WaitForSingleObject` 做超时，
//!   超时后 `CancelIoEx` 撤销，绝不阻塞线程。

use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use windows::core::{HRESULT, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
};
use windows::Win32::Devices::HumanInterfaceDevice::{
    HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetProductString, HidD_GetPreparsedData,
    HidP_GetCaps, HIDD_ATTRIBUTES, HIDP_CAPS, PHIDP_PREPARSED_DATA, GUID_DEVINTERFACE_HID,
};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_OVERLAPPED,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Threading::{
    CreateEventW, ResetEvent, WaitForSingleObject, INFINITE,
};

use crate::atk;

/// 一个 HID 接口的静态信息。
#[derive(Debug, Clone)]
pub struct HidIface {
    pub path: String,
    pub vid: u16,
    pub pid: u16,
    pub usage_page: u16,
    pub usage: u16,
    pub input_len: usize,
    pub output_len: usize,
    pub feature_len: usize,
    pub product: String,
}

impl HidIface {
    pub fn vid_pid(&self) -> String {
        format!("{:04X}:{:04X}", self.vid, self.pid)
    }

    /// 用于 `list` 子命令的一行摘要。
    pub fn label(&self) -> String {
        format!(
            "{} UP={:04X} U={:04X} in={} out={} feat={}  {}",
            self.vid_pid(),
            self.usage_page,
            self.usage,
            self.input_len,
            self.output_len,
            self.feature_len,
            if self.product.is_empty() {
                "-"
            } else {
                &self.product
            }
        )
    }
}

/// 枚举当前系统中所有 HID 接口。
pub fn enumerate() -> Result<Vec<HidIface>> {
    let mut out = Vec::new();

    unsafe {
        let set = SetupDiGetClassDevsW(
            Some(&GUID_DEVINTERFACE_HID),
            None,
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
        .context("SetupDiGetClassDevsW 失败")?;

        let mut index = 0u32;
        loop {
            let mut did = SP_DEVICE_INTERFACE_DATA {
                cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                ..Default::default()
            };

            if SetupDiEnumDeviceInterfaces(set, None, &GUID_DEVINTERFACE_HID, index, &mut did)
                .is_err()
            {
                break; // ERROR_NO_MORE_ITEMS
            }
            index += 1;

            let Some(path) = interface_path(set, &did) else {
                continue;
            };
            if let Some(iface) = probe_interface(&path) {
                out.push(iface);
            }
        }

        let _ = SetupDiDestroyDeviceInfoList(set);
    }

    Ok(out)
}

/// 取设备接口路径（两次调用：先问长度，再读数据）。
unsafe fn interface_path(
    set: windows::Win32::Devices::DeviceAndDriverInstallation::HDEVINFO,
    did: &SP_DEVICE_INTERFACE_DATA,
) -> Option<String> {
    let mut required = 0u32;
    let _ = SetupDiGetDeviceInterfaceDetailW(
        set,
        did,
        None,
        0,
        Some(&mut required),
        None,
    );
    if required == 0 {
        return None;
    }

    let mut buf = vec![0u8; required as usize];
    let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
    (*detail).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;

    let mut devinfo = SP_DEVINFO_DATA {
        cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
        ..Default::default()
    };

    if SetupDiGetDeviceInterfaceDetailW(
        set,
        did,
        Some(detail),
        required,
        None,
        Some(&mut devinfo),
    )
    .is_err()
    {
        return None;
    }

    let ptr = (*detail).DevicePath.as_ptr();
    Some(read_wide(ptr))
}

/// 读取以 NUL 结尾的宽字符串。
unsafe fn read_wide(p: *const u16) -> String {
    let mut len = 0usize;
    while *p.add(len) != 0 {
        len += 1;
    }
    let slice = std::slice::from_raw_parts(p, len);
    String::from_utf16_lossy(slice)
}

/// 打开接口读属性与能力；失败则跳过（无权限/非 HID 集合等）。
fn probe_interface(path: &str) -> Option<HidIface> {
    unsafe {
        let wide = to_wide(path);
        let handle = CreateFileW(
            PCWSTR(wide.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
        .ok()?;

        let mut attrs = HIDD_ATTRIBUTES {
            Size: std::mem::size_of::<HIDD_ATTRIBUTES>() as u32,
            ..Default::default()
        };
        if !HidD_GetAttributes(handle, &mut attrs).as_bool() {
            let _ = CloseHandle(handle);
            return None;
        }

        let mut preparsed = PHIDP_PREPARSED_DATA::default();
        if !HidD_GetPreparsedData(handle, &mut preparsed).as_bool() {
            let _ = CloseHandle(handle);
            return None;
        }

        let mut caps = HIDP_CAPS::default();
        let status = HidP_GetCaps(preparsed, &mut caps);
        let _ = HidD_FreePreparsedData(preparsed);
        if status.is_err() {
            let _ = CloseHandle(handle);
            return None;
        }

        let mut name_buf = [0u16; 128];
        let product = if HidD_GetProductString(
            handle,
            name_buf.as_mut_ptr() as *mut core::ffi::c_void,
            (name_buf.len() * 2) as u32,
        )
        .as_bool()
        {
            read_wide(name_buf.as_ptr())
        } else {
            String::new()
        };

        let _ = CloseHandle(handle);

        Some(HidIface {
            path: path.to_string(),
            vid: attrs.VendorID,
            pid: attrs.ProductID,
            usage_page: caps.UsagePage,
            usage: caps.Usage,
            input_len: caps.InputReportByteLength as usize,
            output_len: caps.OutputReportByteLength as usize,
            feature_len: caps.FeatureReportByteLength as usize,
            product,
        })
    }
}

/// 打开后的设备：读句柄（overlapped）+ 写句柄（同步）+ 读事件。
pub struct Device {
    read: HANDLE,
    write: HANDLE,
    event: HANDLE,
    input_len: usize,
}

// HANDLE 内部是裸指针，默认不满足 Send；设备句柄由单个节拍线程独占使用，
// 这里显式声明可跨线程传递（不会并发访问，锁在 App 层）。
unsafe impl Send for Device {}

impl Device {
    pub fn open(iface: &HidIface) -> Result<Self> {
        let wide = to_wide(&iface.path);
        let path = PCWSTR(wide.as_ptr());

        unsafe {
            let write = CreateFileW(
                path,
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
            .with_context(|| format!("打开写句柄失败：{}", iface.path))?;

            let read = match CreateFileW(
                path,
                GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                None,
            ) {
                Ok(h) => h,
                Err(e) => {
                    let _ = CloseHandle(write);
                    return Err(e).with_context(|| format!("打开读句柄失败：{}", iface.path));
                }
            };

            let event = match CreateEventW(None, true, false, PCWSTR::null()) {
                Ok(h) => h,
                Err(e) => {
                    let _ = CloseHandle(write);
                    let _ = CloseHandle(read);
                    return Err(e).context("CreateEventW 失败");
                }
            };

            Ok(Self {
                read,
                write,
                event,
                input_len: iface.input_len.max(atk::FRAME_LEN),
            })
        }
    }

    /// 底层交换：返回一帧应答（优先返回校验通过的），排障时可直接看原始字节。
    pub fn exchange(
        &mut self,
        retries: u32,
        per_read: Duration,
        deadline: Duration,
    ) -> Result<Vec<u8>> {
        let request = atk::request();
        let mut last: Option<Vec<u8>> = None;

        for _ in 0..retries.max(1) {
            self.drain();

            if let Err(e) = self.write_report(&request) {
                return Err(e).context("写输出报告失败");
            }

            let start = Instant::now();
            while start.elapsed() < deadline {
                match self.read_report(per_read) {
                    Ok(Some(buf)) => {
                        if atk::is_valid(&buf) {
                            return Ok(buf);
                        }
                        last = Some(buf);
                    }
                    Ok(None) => continue,
                    Err(e) => return Err(e),
                }
            }
        }

        match last {
            Some(buf) => Ok(buf),
            None => Err(anyhow!(
                "设备没有给出有效回复（鼠标可能已休眠，动一下鼠标再试）"
            )),
        }
    }

    fn write_report(&self, data: &[u8]) -> Result<()> {
        let mut written = 0u32;
        unsafe {
            WriteFile(self.write, Some(data), Some(&mut written), None)
                .map_err(|e| anyhow!("WriteFile: {e}"))?;
        }
        if (written as usize) < data.len() {
            return Err(anyhow!(
                "WriteFile 只写入 {written}/{} 字节（输出报告长度不匹配？）",
                data.len()
            ));
        }
        Ok(())
    }

    /// 读一条报告，超时返回 `Ok(None)`。
    fn read_report(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
        let mut buf = vec![0u8; self.input_len.max(64)];
        let mut read = 0u32;

        unsafe {
            let mut ov = OVERLAPPED::default();
            ov.hEvent = self.event;
            let _ = ResetEvent(self.event);

            match ReadFile(self.read, Some(&mut buf), Some(&mut read), Some(&mut ov)) {
                Ok(()) => {}
                Err(e) if e.code() == HRESULT::from_win32(ERROR_IO_PENDING.0) => {
                    match WaitForSingleObject(self.event, timeout.as_millis() as u32) {
                        WAIT_OBJECT_0 => {
                            if let Err(e) =
                                GetOverlappedResult(self.read, &ov, &mut read, false)
                            {
                                return Err(anyhow!("GetOverlappedResult: {e}"));
                            }
                        }
                        WAIT_TIMEOUT => {
                            // 撤销挂起的读，并等它真正结束，避免句柄带着未完成 I/O 被复用
                            let _ = CancelIoEx(self.read, Some(&ov));
                            let _ = WaitForSingleObject(self.event, INFINITE);
                            let mut dummy = 0u32;
                            let _ = GetOverlappedResult(self.read, &ov, &mut dummy, false);
                            return Ok(None);
                        }
                        _ => return Ok(None),
                    }
                }
                Err(e) => return Err(anyhow!("ReadFile: {e}")),
            }
        }

        if read == 0 {
            return Ok(None);
        }
        buf.truncate(read as usize);
        Ok(Some(buf))
    }

    /// 丢弃队列里残留的输入报告（上一次超时后迟到的回复）。
    fn drain(&mut self) {
        for _ in 0..32 {
            match self.read_report(Duration::from_millis(0)) {
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe {
            let _ = CancelIoEx(self.read, None);
            let _ = CloseHandle(self.event);
            let _ = CloseHandle(self.read);
            let _ = CloseHandle(self.write);
        }
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
