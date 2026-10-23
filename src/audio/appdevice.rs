//! 未公开的 `Windows.Media.Internal.AudioPolicyConfig`：给**单个进程**指定播放设备。
//!
//! 参考 EarTrumpet `DataModel/WindowsAudio/Internal/AudioPolicyConfigService.cs`：
//! `SetPersistedDefaultAudioEndpoint` 按 **进程 id** 记住「这个应用从哪台设备出声」，
//! 应用重启后仍然有效（系统按 exe 路径落盘）。这就是 Win10 1803 之后
//! 「设置 → 应用音量 → 输出设备」背后用的那一套。
//!
//! 接口是 internal WinRT 类型，没有公开声明，这里和 `policy.rs` 一样手写 vtable。
//!
//! 实测（Windows 11 + AudioSes.dll）：
//! - vtable 布局 = IUnknown 3 槽 + IInspectable 3 槽 + 19 个未使用方法，之后才是
//!   槽 25 `Set`、26 `Get`、27 `Clear`（与 EarTrumpet 里 19 个 `__incomplete__`
//!   占位一一对应）。
//! - `Set` 对虚拟设备 / 不存在的端点返回 `E_INVALIDARG`；把应用设成「当前默认设备」
//!   返回 S_OK，但系统会把持久化值清掉 —— 也就是等价于「跟随默认设备」。
//! - `Get` 在没有持久化值时返回 S_OK + 空 HSTRING，调用方按「跟随默认」处理。

use std::ffi::c_void;
use std::sync::OnceLock;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::core::{GUID, HRESULT, PCSTR, PCWSTR};

/// Win10 21H2 及以后用的 IID；微软在 21H2 换过一次 IID，老系统要用另一个。
const IID_21H2: GUID = GUID::from_u128(0xab3d4648_e242_459f_b02f_541c70306324);
const IID_DOWNLEVEL: GUID = GUID::from_u128(0x2a59116d_6c4f_45e0_a74f_707e3fef9258);

const ACTIVATABLE_CLASS: &str = "Windows.Media.Internal.AudioPolicyConfig";

/// 生成 / 解析设备 id 时用的前缀，照抄 EarTrumpet `AudioPolicyConfig`。
const MMDEVAPI_TOKEN: &str = r"\\?\SWD#MMDEVAPI#";
const DEVINTERFACE_AUDIO_RENDER: &str = "#{e6327cad-dcec-4949-ae8a-991e976a79d2}";
const DEVINTERFACE_AUDIO_CAPTURE: &str = "#{2eef81be-33fa-4800-9670-1cd474972c3f}";

/// EDataFlow::eRender —— 我们只管播放设备。
const FLOW_RENDER: i32 = 0;
/// ERole::eConsole / eMultimedia。Set 要分别写一次；Get 传二者的按位或。
const ROLE_CONSOLE: i32 = 0;
const ROLE_MULTIMEDIA: i32 = 1;

/// `SetPersistedDefaultAudioEndpoint(pid, flow, role, HSTRING deviceId)`
type FnSet = unsafe extern "system" fn(*mut c_void, u32, i32, i32, *mut c_void) -> HRESULT;
/// `GetPersistedDefaultAudioEndpoint(pid, flow, role, HSTRING* deviceId)`
type FnGet = unsafe extern "system" fn(*mut c_void, u32, i32, i32, *mut *mut c_void) -> HRESULT;

/// IAudioPolicyConfigFactory 的 vtable。
///
/// 前 25 槽全是占位（IUnknown / IInspectable / 我们不用到的那些方法），
/// 只能靠顺序对齐 —— 少一个槽就会调到别的方法上去，所以单独列出来写死。
#[repr(C)]
struct Vtbl {
    reserved: [*const c_void; 25],
    set: FnSet,
    get: FnGet,
    #[allow(dead_code)]
    clear: *const c_void,
}

// WinRT 的字符串函数和激活工厂都走 API set。这里不用 `#[link]`：新版 SDK 的
// 导入库里已经没有这几个 `.lib` 了，改成运行时解析，跟 icons.rs 取
// PrivateExtractIconsW 是同一个路数。
type FnCreateString = unsafe extern "system" fn(*const u16, u32, *mut *mut c_void) -> HRESULT;
type FnStringBuffer = unsafe extern "system" fn(*mut c_void, *mut u32) -> *const u16;
type FnDeleteString = unsafe extern "system" fn(*mut c_void) -> HRESULT;
type FnActivationFactory =
    unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT;

struct WinRt {
    create_string: FnCreateString,
    string_buffer: FnStringBuffer,
    delete_string: FnDeleteString,
    activation_factory: FnActivationFactory,
}

impl WinRt {
    fn load() -> Option<Self> {
        unsafe fn module(name: &str) -> Option<HMODULE> {
            let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) }.ok()
        }
        unsafe fn symbol<T>(module: HMODULE, name: &str) -> Option<T> {
            let name: Vec<u8> = name.bytes().chain(std::iter::once(0)).collect();
            let proc = unsafe { GetProcAddress(module, PCSTR(name.as_ptr())) }?;
            Some(unsafe { std::mem::transmute_copy(&proc) })
        }

        unsafe {
            let winrt = module("api-ms-win-core-winrt-l1-1-0.dll")?;
            let strings = module("api-ms-win-core-winrt-string-l1-1-0.dll")?;
            Some(Self {
                activation_factory: symbol(winrt, "RoGetActivationFactory")?,
                create_string: symbol(strings, "WindowsCreateString")?,
                string_buffer: symbol(strings, "WindowsGetStringRawBuffer")?,
                delete_string: symbol(strings, "WindowsDeleteString")?,
            })
        }
    }
}

/// 老系统上这些函数可能一个都拿不到，只解析一次。
fn winrt() -> Option<&'static WinRt> {
    static API: OnceLock<Option<WinRt>> = OnceLock::new();
    API.get_or_init(WinRt::load).as_ref()
}

pub struct AppDevicePolicy {
    raw: *mut c_void,
    vtbl: *const Vtbl,
    api: &'static WinRt,
}

// 只在音频线程（已初始化 MTA）里用。
unsafe impl Send for AppDevicePolicy {}

impl AppDevicePolicy {
    pub fn new() -> Option<Self> {
        let api = winrt()?;
        let class = HString::new(api, ACTIVATABLE_CLASS)?;
        // 先按 21H2 的 IID 要，拿不到再退回老 IID：靠试比读系统版本号可靠。
        for iid in [IID_21H2, IID_DOWNLEVEL] {
            let mut raw: *mut c_void = std::ptr::null_mut();
            let hr = unsafe { (api.activation_factory)(class.raw, &iid, &mut raw) };
            if hr.is_ok() && !raw.is_null() {
                let vtbl = unsafe { *(raw as *mut *const Vtbl) };
                return Some(Self { raw, vtbl, api });
            }
        }
        None
    }

    /// 这个进程现在被指定到哪台设备；`None` = 跟随系统默认设备。
    ///
    /// 返回的 id 已经剥掉 `\\?\SWD#MMDEVAPI#` 前缀和接口 GUID，
    /// 可以直接和 `DeviceInfo::id`（`IMMDevice::GetId` 的结果）比较。
    pub fn device_of(&self, pid: u32) -> Option<String> {
        if pid == 0 {
            return None;
        }
        let mut out: *mut c_void = std::ptr::null_mut();
        let hr = unsafe {
            ((*self.vtbl).get)(
                self.raw,
                pid,
                FLOW_RENDER,
                ROLE_CONSOLE | ROLE_MULTIMEDIA,
                &mut out,
            )
        };
        if hr.is_err() || out.is_null() {
            return None;
        }
        let value = unsafe { HString::take(self.api, out) };
        let id = unpack_device_id(&value);
        (!id.is_empty()).then_some(id)
    }

    /// 把这个进程指到某台设备；`device_id` 为 `None` = 恢复「跟随默认设备」。
    ///
    /// 两种角色（控制台 / 多媒体）都要写，否则某些应用只改一半。
    /// 返回是否至少写入成功一次 —— 写完还要等系统重建会话，不能当场读回。
    pub fn set_device(&self, pid: u32, device_id: Option<&str>) -> bool {
        if pid == 0 {
            return false;
        }
        // 空 HSTRING（nullptr）就是「清除」，语义与 EarTrumpet 一致。
        let device = match device_id {
            Some(id) => match HString::new(self.api, &pack_device_id(id)) {
                Some(value) => value,
                None => return false,
            },
            None => HString::empty(),
        };

        let mut applied = false;
        for role in [ROLE_MULTIMEDIA, ROLE_CONSOLE] {
            let hr = unsafe { ((*self.vtbl).set)(self.raw, pid, FLOW_RENDER, role, device.raw) };
            applied |= hr.is_ok();
        }
        applied
    }
}

impl Drop for AppDevicePolicy {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe {
                let slots = self.raw as *mut *const c_void;
                // IUnknown::Release 恒在 vtable 第 3 槽。
                let release: unsafe extern "system" fn(*mut c_void) -> u32 =
                    std::mem::transmute(*slots.add(2));
                release(self.raw);
            }
            self.raw = std::ptr::null_mut();
        }
    }
}

/// 薄薄一层 HSTRING 包装，只在音频线程里用，长度都很短。
struct HString {
    raw: *mut c_void,
    api: &'static WinRt,
}

impl HString {
    fn new(api: &'static WinRt, text: &str) -> Option<Self> {
        let wide: Vec<u16> = text.encode_utf16().collect();
        let mut raw: *mut c_void = std::ptr::null_mut();
        let hr = unsafe { (api.create_string)(wide.as_ptr(), wide.len() as u32, &mut raw) };
        (hr.is_ok() && !raw.is_null()).then_some(Self { raw, api })
    }

    /// 「清除设置」要传的空 HSTRING：直接给 nullptr，不持引用。
    fn empty() -> Self {
        Self {
            raw: std::ptr::null_mut(),
            api: winrt().expect("WinRt 已加载"),
        }
    }

    /// 接管一个 HSTRING 的所有权并读出内容。
    unsafe fn take(api: &'static WinRt, raw: *mut c_void) -> String {
        let mut len = 0u32;
        let buffer = unsafe { (api.string_buffer)(raw, &mut len) };
        let text = if buffer.is_null() || len == 0 {
            String::new()
        } else {
            unsafe { String::from_utf16_lossy(std::slice::from_raw_parts(buffer, len as usize)) }
        };
        let _ = unsafe { (api.delete_string)(raw) };
        text
    }
}

impl Drop for HString {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            let _ = unsafe { (self.api.delete_string)(self.raw) };
            self.raw = std::ptr::null_mut();
        }
    }
}

/// `{0.0.0.…}.{GUID}` → `\\?\SWD#MMDEVAPI#{…}#{接口 GUID}`。
fn pack_device_id(device_id: &str) -> String {
    if device_id.starts_with(MMDEVAPI_TOKEN) {
        return device_id.to_string();
    }
    format!("{MMDEVAPI_TOKEN}{device_id}{DEVINTERFACE_AUDIO_RENDER}")
}

/// 反向：把接口那一层的前后缀剥掉，还原成 `IMMDevice::GetId` 的形态。
pub fn unpack_device_id(value: &str) -> String {
    let mut text = value.trim();
    if let Some(rest) = text.strip_prefix(MMDEVAPI_TOKEN) {
        text = rest;
    }
    for suffix in [DEVINTERFACE_AUDIO_RENDER, DEVINTERFACE_AUDIO_CAPTURE] {
        if let Some(rest) = text.strip_suffix(suffix) {
            text = rest;
        }
    }
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::{pack_device_id, unpack_device_id};

    const ENDPOINT: &str = "{0.0.0.00000000}.{4B03AB73-69C9-4AF3-B8BA-363D13C80E07}";

    /// 打包 / 解包必须严格互逆：面板拿到的 id 和 API 读回来的 id 是同一套写法。
    #[test]
    fn device_id_round_trips_through_the_swd_token() {
        let packed = pack_device_id(ENDPOINT);
        assert_eq!(
            packed,
            "\\\\?\\SWD#MMDEVAPI#{0.0.0.00000000}.{4B03AB73-69C9-4AF3-B8BA-363D13C80E07}\
             #{e6327cad-dcec-4949-ae8a-991e976a79d2}"
        );
        assert_eq!(unpack_device_id(&packed), ENDPOINT);
        // 已经是打包形态的不要重复加前缀。
        assert_eq!(pack_device_id(&packed), packed);
        // 空串 / 脏数据不能变成奇怪的 id。
        assert_eq!(unpack_device_id(""), "");
        assert_eq!(unpack_device_id("  "), "");
    }
}
