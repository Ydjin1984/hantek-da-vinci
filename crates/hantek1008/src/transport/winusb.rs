//! WinUSB transport: the safe path.
//!
//! Bound with Zadig (or an INF), the scope appears as a plain USB device and
//! `winusb.dll` gives us raw pipe access. Nothing vendor-supplied runs in the
//! kernel, so a bad day costs us a process, not a bugcheck.

use super::Transport;
use crate::{Error, Result};
use std::ffi::{c_void, CString};
use std::ptr;

type Handle = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

/// `GUID_DEVINTERFACE_USB_DEVICE`, the generic USB device interface class.
const GUID_DEVINTERFACE_USB_DEVICE: Guid = Guid {
    data1: 0xA5DC_BF10,
    data2: 0x6530,
    data3: 0x11D2,
    data4: [0x90, 0x1F, 0x00, 0xC0, 0x4F, 0xB9, 0x51, 0xED],
};

const DIGCF_PRESENT: u32 = 0x02;
const DIGCF_DEVICEINTERFACE: u32 = 0x10;
const GENERIC_READ_WRITE: u32 = 0xC000_0000;
const FILE_SHARE_RW: u32 = 0x03;
const OPEN_EXISTING: u32 = 3;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

const USB_ENDPOINT_TYPE_BULK: u32 = 2;

#[repr(C)]
struct SpDeviceInterfaceData {
    cb_size: u32,
    interface_class_guid: Guid,
    flags: u32,
    reserved: usize,
}

#[repr(C)]
#[derive(Default)]
struct UsbInterfaceDescriptor {
    b_length: u8,
    b_descriptor_type: u8,
    b_interface_number: u8,
    b_alternate_setting: u8,
    b_num_endpoints: u8,
    b_interface_class: u8,
    b_interface_sub_class: u8,
    b_interface_protocol: u8,
    i_interface: u8,
}

#[repr(C)]
#[derive(Default)]
struct WinusbPipeInformation {
    pipe_type: u32,
    pipe_id: u8,
    maximum_packet_size: u16,
    interval: u8,
}

#[link(name = "setupapi")]
extern "system" {
    fn SetupDiGetClassDevsA(
        class_guid: *const Guid,
        enumerator: *const u8,
        parent: Handle,
        flags: u32,
    ) -> Handle;
    fn SetupDiEnumDeviceInterfaces(
        set: Handle,
        info: *mut c_void,
        guid: *const Guid,
        index: u32,
        data: *mut SpDeviceInterfaceData,
    ) -> i32;
    fn SetupDiGetDeviceInterfaceDetailA(
        set: Handle,
        data: *mut SpDeviceInterfaceData,
        detail: *mut u8,
        size: u32,
        needed: *mut u32,
        info: *mut c_void,
    ) -> i32;
    fn SetupDiDestroyDeviceInfoList(set: Handle) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn CreateFileA(
        path: *const u8,
        access: u32,
        share: u32,
        security: *mut c_void,
        creation: u32,
        flags: u32,
        template: Handle,
    ) -> Handle;
    fn CloseHandle(h: Handle) -> i32;
    fn GetLastError() -> u32;
}

#[link(name = "winusb")]
extern "system" {
    fn WinUsb_Initialize(device: Handle, iface: *mut Handle) -> i32;
    fn WinUsb_Free(iface: Handle) -> i32;
    fn WinUsb_QueryInterfaceSettings(
        iface: Handle,
        alt: u8,
        desc: *mut UsbInterfaceDescriptor,
    ) -> i32;
    fn WinUsb_QueryPipe(
        iface: Handle,
        alt: u8,
        index: u8,
        info: *mut WinusbPipeInformation,
    ) -> i32;
    fn WinUsb_SetPipePolicy(iface: Handle, pipe: u8, policy: u32, len: u32, value: *const c_void)
        -> i32;
    fn WinUsb_WritePipe(
        iface: Handle,
        pipe: u8,
        buf: *const u8,
        len: u32,
        transferred: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn WinUsb_ReadPipe(
        iface: Handle,
        pipe: u8,
        buf: *mut u8,
        len: u32,
        transferred: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
}

const PIPE_TRANSFER_TIMEOUT: u32 = 3;

fn last_error() -> u32 {
    unsafe { GetLastError() }
}

/// Enumerate every USB device path whose instance id mentions the scope.
///
/// WinUSB binding changes the interface class, so we cannot filter by the
/// Hantek interface GUID any more; instead we ask for all USB devices and keep
/// the ones whose device path names VID 0783 / PID 5725.
pub fn device_paths() -> Vec<String> {
    let mut out = Vec::new();
    unsafe {
        let set = SetupDiGetClassDevsA(
            &GUID_DEVINTERFACE_USB_DEVICE,
            ptr::null(),
            ptr::null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        );
        if set.is_null() || set as isize == -1 {
            return out;
        }
        let mut index = 0u32;
        loop {
            let mut iface = SpDeviceInterfaceData {
                cb_size: std::mem::size_of::<SpDeviceInterfaceData>() as u32,
                interface_class_guid: Guid { data1: 0, data2: 0, data3: 0, data4: [0; 8] },
                flags: 0,
                reserved: 0,
            };
            if SetupDiEnumDeviceInterfaces(set, ptr::null_mut(), &GUID_DEVINTERFACE_USB_DEVICE, index, &mut iface) == 0 {
                break;
            }
            index += 1;
            let mut needed = 0u32;
            let _ = SetupDiGetDeviceInterfaceDetailA(
                set,
                &mut iface,
                ptr::null_mut(),
                0,
                &mut needed,
                ptr::null_mut(),
            );
            if needed == 0 || last_error() != ERROR_INSUFFICIENT_BUFFER {
                continue;
            }
            let mut buf = vec![0u8; needed as usize + 16];
            let detail_cb = if cfg!(target_pointer_width = "64") { 8u32 } else { 5u32 };
            buf[0..4].copy_from_slice(&detail_cb.to_le_bytes());
            if SetupDiGetDeviceInterfaceDetailA(
                set,
                &mut iface,
                buf.as_mut_ptr(),
                buf.len() as u32,
                ptr::null_mut(),
                ptr::null_mut(),
            ) == 0
            {
                continue;
            }
            let s = &buf[4..];
            let end = s.iter().position(|&b| b == 0).unwrap_or(0);
            if end == 0 {
                continue;
            }
            let path = String::from_utf8_lossy(&s[..end]).into_owned();
            let lower = path.to_ascii_lowercase();
            if lower.contains("vid_0783") && lower.contains("pid_5725") {
                out.push(path);
            }
        }
        SetupDiDestroyDeviceInfoList(set);
    }
    out
}

/// Open the first scope bound to WinUSB.
pub fn open_first() -> Result<WinUsbTransport> {
    let paths = device_paths();
    if paths.is_empty() {
        return Err(Error::NoDevice);
    }
    let mut last = None;
    for p in &paths {
        match WinUsbTransport::open(p) {
            Ok(t) => return Ok(t),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or(Error::NoDevice))
}

/// Raw bulk access through `winusb.dll`.
pub struct WinUsbTransport {
    device: Handle,
    iface: Handle,
    ep_in: u8,
    ep_out: u8,
    path: String,
}

unsafe impl Send for WinUsbTransport {}

impl WinUsbTransport {
    pub fn open(path: &str) -> Result<WinUsbTransport> {
        let c = CString::new(path).map_err(|_| Error::Protocol("path contains NUL".into()))?;
        unsafe {
            // Synchronous handle: WinUsb calls block until the transfer is done,
            // which keeps the state machine simple.
            let device = CreateFileA(
                c.as_ptr() as *const u8,
                GENERIC_READ_WRITE,
                FILE_SHARE_RW,
                ptr::null_mut(),
                OPEN_EXISTING,
                0,
                ptr::null_mut(),
            );
            if device == (-1isize) as Handle {
                // 32 = sharing violation: the vendor application still owns it.
                return Err(Error::Win32 { call: "CreateFileA", code: last_error() });
            }
            let mut iface: Handle = ptr::null_mut();
            if WinUsb_Initialize(device, &mut iface) == 0 {
                let e = last_error();
                CloseHandle(device);
                return Err(Error::Win32 { call: "WinUsb_Initialize", code: e });
            }

            let mut desc = UsbInterfaceDescriptor::default();
            if WinUsb_QueryInterfaceSettings(iface, 0, &mut desc) == 0 {
                let e = last_error();
                WinUsb_Free(iface);
                CloseHandle(device);
                return Err(Error::Win32 { call: "WinUsb_QueryInterfaceSettings", code: e });
            }

            let mut ep_in = 0u8;
            let mut ep_out = 0u8;
            for i in 0..desc.b_num_endpoints {
                let mut pipe = WinusbPipeInformation::default();
                if WinUsb_QueryPipe(iface, 0, i, &mut pipe) == 0 {
                    continue;
                }
                if pipe.pipe_type == USB_ENDPOINT_TYPE_BULK {
                    if pipe.pipe_id & 0x80 != 0 && ep_in == 0 {
                        ep_in = pipe.pipe_id;
                    } else if pipe.pipe_id & 0x80 == 0 && ep_out == 0 {
                        ep_out = pipe.pipe_id;
                    }
                }
            }
            if ep_in == 0 || ep_out == 0 {
                WinUsb_Free(iface);
                CloseHandle(device);
                return Err(Error::Protocol("no bulk endpoints on interface 0".into()));
            }

            // A stuck read must fail instead of hanging the acquisition thread.
            let timeout: u32 = 1000;
            for ep in [ep_in, ep_out] {
                let _ = WinUsb_SetPipePolicy(
                    iface,
                    ep,
                    PIPE_TRANSFER_TIMEOUT,
                    std::mem::size_of::<u32>() as u32,
                    &timeout as *const u32 as *const c_void,
                );
            }

            Ok(WinUsbTransport { device, iface, ep_in, ep_out, path: path.to_string() })
        }
    }
}

impl Transport for WinUsbTransport {
    fn write(&mut self, buf: &[u8]) -> Result<()> {
        let mut done = 0u32;
        let ok = unsafe {
            WinUsb_WritePipe(
                self.iface,
                self.ep_out,
                buf.as_ptr(),
                buf.len() as u32,
                &mut done,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(Error::Win32 { call: "WinUsb_WritePipe", code: last_error() });
        }
        if done as usize != buf.len() {
            return Err(Error::Protocol(format!("short write {done}/{}", buf.len())));
        }
        Ok(())
    }

    fn read(&mut self, len: usize) -> Result<Vec<u8>> {
        let mut out = vec![0u8; len];
        let mut done = 0u32;
        let ok = unsafe {
            WinUsb_ReadPipe(
                self.iface,
                self.ep_in,
                out.as_mut_ptr(),
                out.len() as u32,
                &mut done,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(Error::Win32 { call: "WinUsb_ReadPipe", code: last_error() });
        }
        if done as usize != len {
            return Err(Error::Protocol(format!("short read {done}/{len}")));
        }
        Ok(out)
    }

    fn describe(&self) -> String {
        format!("{} (bulk out {:#04x}, in {:#04x})", self.path, self.ep_out, self.ep_in)
    }
}

impl Drop for WinUsbTransport {
    fn drop(&mut self) {
        unsafe {
            if !self.iface.is_null() {
                WinUsb_Free(self.iface);
            }
            if !self.device.is_null() && self.device != (-1isize) as Handle {
                CloseHandle(self.device);
            }
        }
    }
}
