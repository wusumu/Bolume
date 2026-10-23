//! COM 基础设施：枚举器创建、属性读取、字符串转换。

use std::ffi::c_void;

use windows::Win32::{
    Foundation::PROPERTYKEY,
    Media::Audio::{IMMDevice, IMMDeviceEnumerator},
    System::Com::{
        CLSCTX_ALL, COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED, CoCreateInstance,
        CoInitializeEx, CoTaskMemFree, CoUninitialize, STGM_READ,
    },
    UI::Shell::PropertiesSystem::IPropertyStore,
};
use windows::core::{GUID, Interface, PCWSTR, PWSTR};

/// CLSID_MMDeviceEnumerator
pub const CLSID_MM_DEVICE_ENUMERATOR: GUID =
    GUID::from_u128(0xbcde0395_e52f_467c_8e3d_c4579291692e);

/// PKEY_AudioEndpoint_FormFactor {1da5d803-d492-4edd-8c23-e0c0ffee7f0e}, 0
pub const PKEY_AUDIO_ENDPOINT_FORM_FACTOR: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x1da5d803_d492_4edd_8c23_e0c0ffee7f0e),
    pid: 0,
};

/// PKEY_AudioEndpoint_GUID {1da5d803-d492-4edd-8c23-e0c0ffee7f0e}, 4
pub const PKEY_AUDIO_ENDPOINT_GUID: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x1da5d803_d492_4edd_8c23_e0c0ffee7f0e),
    pid: 4,
};

/// PKEY_Device_FriendlyName {a45c254e-df1c-4efd-8020-67d146a850e0}, 14
pub const PKEY_DEVICE_FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
    pid: 14,
};

/// PKEY_Device_DeviceDesc {a45c254e-df1c-4efd-8020-67d146a850e0}, 2
pub const PKEY_DEVICE_DEVICE_DESC: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
    pid: 2,
};

/// PKEY_DeviceInterface_FriendlyName {026e516e-b814-414b-83cd-856d6fef4822}, 2
pub const PKEY_DEVICE_INTERFACE_FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x026e516e_b814_414b_83cd_856d6fef4822),
    pid: 2,
};

/// 音频线程入口处初始化 MTA 套间，返回的守卫在离开作用域时反初始化。
pub struct ComApartment {
    initialized: bool,
}

impl ComApartment {
    pub fn init_sta() -> Self {
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        Self {
            initialized: hr.is_ok(),
        }
    }

    pub fn init_mta() -> Self {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Self {
            initialized: hr.is_ok(),
        }
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.initialized {
            unsafe { CoUninitialize() };
        }
    }
}

/// 取 PWSTR 内容的拷贝并释放 COM 分配的内存。
pub unsafe fn take_pwstr(value: PWSTR) -> String {
    if value.is_null() {
        return String::new();
    }
    let text = unsafe { value.to_string().unwrap_or_default() };
    unsafe { CoTaskMemFree(Some(value.0 as *const c_void)) };
    text
}

pub fn create_device_enumerator() -> windows::core::Result<IMMDeviceEnumerator> {
    unsafe {
        CoCreateInstance::<_, IMMDeviceEnumerator>(
            &CLSID_MM_DEVICE_ENUMERATOR,
            None::<&windows::core::IUnknown>,
            CLSCTX_ALL,
        )
    }
}

pub unsafe fn device_id(device: &IMMDevice) -> Option<String> {
    let id = unsafe { device.GetId() }.ok()?;
    let text = unsafe { take_pwstr(id) };
    (!text.is_empty()).then_some(text)
}

unsafe fn open_store(device: &IMMDevice) -> Option<IPropertyStore> {
    unsafe { device.OpenPropertyStore(STGM_READ).ok() }
}

/// 读取字符串属性，失败返回 None。
pub unsafe fn read_string_property(device: &IMMDevice, key: &PROPERTYKEY) -> Option<String> {
    let store = unsafe { open_store(device) }?;
    let value = unsafe { store.GetValue(key) }.ok()?;
    let text = unsafe {
        windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc(&value)
            .ok()
            .map(|ptr| take_pwstr(ptr))
    }?;
    let trimmed = text.trim().to_string();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// 读取 u32 属性（用于 FormFactor）。
pub unsafe fn read_u32_property(device: &IMMDevice, key: &PROPERTYKEY) -> Option<u32> {
    let store = unsafe { open_store(device) }?;
    let value = unsafe { store.GetValue(key) }.ok()?;
    let number = unsafe { value.Anonymous.Anonymous.Anonymous.ulVal };
    Some(number)
}

pub fn pcwstr(value: &[u16]) -> PCWSTR {
    PCWSTR(value.as_ptr())
}

/// 把 Rust 字符串转成以 NUL 结尾的宽字符串。
pub fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 让 Interface trait 在本模块可见（部分调用点需要 as_raw）。
#[allow(dead_code)]
pub fn raw_of<T: Interface>(value: &T) -> *mut c_void {
    value.as_raw()
}
