//! 未公开的 IPolicyConfig 接口，用于切换系统默认播放/录音设备。
//!
//! Windows 没有公开 API 能做这件事，业界通用做法是调用 PolicyConfig 的 COM 组件。
//! 这里手写 vtable，避免依赖 windows-rs 未提供的接口声明。

use std::ffi::c_void;

use windows::Win32::{Media::Audio::ERole, System::Com::CoCreateInstance};
use windows::{
    Win32::System::Com::CLSCTX_ALL,
    core::{GUID, HRESULT, IUnknown, Interface},
};

const CLSID_POLICY_CONFIG_CLIENT: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
const IID_IPOLICY_CONFIG: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);

type FnQueryInterface =
    unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT;
type FnAddRef = unsafe extern "system" fn(*mut c_void) -> u32;
type FnRelease = unsafe extern "system" fn(*mut c_void) -> u32;
type FnSetDefaultEndpoint = unsafe extern "system" fn(*mut c_void, *const u16, ERole) -> HRESULT;
type FnSetEndpointVisibility = unsafe extern "system" fn(*mut c_void, *const u16, i16) -> HRESULT;

/// IPolicyConfig 的 vtable：IUnknown 三槽 + 8 个未使用的方法，
/// 随后才是 GetPropertyValue / SetPropertyValue / SetDefaultEndpoint / SetEndpointVisibility。
#[repr(C)]
struct PolicyConfigVtbl {
    query_interface: FnQueryInterface,
    add_ref: FnAddRef,
    release: FnRelease,
    unused_get_mix_format: *const c_void,
    unused_get_device_format: *const c_void,
    unused_reset_device_format: *const c_void,
    unused_set_device_format: *const c_void,
    unused_get_processing_period: *const c_void,
    unused_set_processing_period: *const c_void,
    unused_get_share_mode: *const c_void,
    unused_set_share_mode: *const c_void,
    unused_get_property_value: *const c_void,
    unused_set_property_value: *const c_void,
    set_default_endpoint: FnSetDefaultEndpoint,
    set_endpoint_visibility: FnSetEndpointVisibility,
}

pub struct PolicyConfig {
    raw: *mut c_void,
    vtbl: *const PolicyConfigVtbl,
}

// COM 对象在 MTA 中可跨线程使用，这里只在音频线程内调用。
unsafe impl Send for PolicyConfig {}

impl PolicyConfig {
    pub fn new() -> Option<Self> {
        unsafe {
            let unknown: IUnknown =
                CoCreateInstance(&CLSID_POLICY_CONFIG_CLIENT, None::<&IUnknown>, CLSCTX_ALL)
                    .ok()?;
            let source = unknown.as_raw();
            let source_vtbl = *(source as *mut *const PolicyConfigVtbl);
            let query_interface = (*source_vtbl).query_interface;

            let mut policy: *mut c_void = std::ptr::null_mut();
            let hr = query_interface(source, &IID_IPOLICY_CONFIG, &mut policy);
            if hr.is_err() || policy.is_null() {
                return None;
            }

            let vtbl = *(policy as *mut *const PolicyConfigVtbl);
            // CoCreateInstance 已持有初始引用，这里再加一次作为 PolicyConfig 自身的引用。
            ((*vtbl).add_ref)(policy);

            Some(Self { raw: policy, vtbl })
        }
    }

    pub fn set_default_endpoint(&self, device_id: &str, role: ERole) -> windows::core::Result<()> {
        let wide = super::com::wide(device_id);
        unsafe { ((*self.vtbl).set_default_endpoint)(self.raw, wide.as_ptr(), role).ok() }
    }

    pub fn set_endpoint_visibility(
        &self,
        device_id: &str,
        visible: bool,
    ) -> windows::core::Result<()> {
        let wide = super::com::wide(device_id);
        unsafe {
            ((*self.vtbl).set_endpoint_visibility)(
                self.raw,
                wide.as_ptr(),
                if visible { 1 } else { 0 },
            )
            .ok()
        }
    }
}

impl Drop for PolicyConfig {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe { ((*self.vtbl).release)(self.raw) };
            self.raw = std::ptr::null_mut();
        }
    }
}
