use anyhow::{Context, Result};
use gpui_util::ResultExt;
use itertools::Itertools;
use std::sync::{Mutex, OnceLock};
use windows::Win32::{
    Foundation::HMODULE,
    Graphics::{
        Direct3D::{
            D3D_DRIVER_TYPE_UNKNOWN, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL,
            D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
        },
        Direct3D11::{
            D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_DEBUG,
            D3D11_FEATURE_D3D10_X_HARDWARE_OPTIONS, D3D11_FEATURE_DATA_D3D10_X_HARDWARE_OPTIONS,
            D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
        },
        Dxgi::{
            CreateDXGIFactory1, CreateDXGIFactory2, DXGI_CREATE_FACTORY_DEBUG,
            DXGI_CREATE_FACTORY_FLAGS, IDXGIAdapter, IDXGIAdapter1, IDXGIDevice, IDXGIDevice3,
            IDXGIFactory2,
        },
    },
};
use windows::core::Interface;

/// 进程里那一个 D3D11 设备对应的 DXGI 设备（只有 `IDXGIDevice3` 带 `Trim`）。
///
/// 常驻托盘的应用空闲时要把内存还回去：驱动在设备里缓存了一大块提交
/// （实测一个跑过几张窗口尺寸纹理的进程能退回 ~27MB），`Trim` 之后驱动下次
/// 绘制时再按需提交，设备本身不用重建。
static DXGI_DEVICE: OnceLock<Mutex<(Option<ID3D11DeviceContext>, Option<IDXGIDevice3>)>> =
    OnceLock::new();

fn remember_device(device: &ID3D11Device, context: &ID3D11DeviceContext) {
    let slot = DXGI_DEVICE.get_or_init(|| Mutex::new((None, None)));
    let Ok(mut slot) = slot.lock() else { return };
    if slot.1.is_some() {
        return;
    }
    slot.0 = Some(context.clone());
    slot.1 = device.cast::<IDXGIDevice3>().ok();
}

/// 把驱动缓存的那部分提交还给系统。空闲时调（有窗口在画的时候别调，白折腾）。
pub fn trim_graphics() {
    let Some(slot) = DXGI_DEVICE.get() else { return };
    let Ok(slot) = slot.lock() else { return };
    if let Some(context) = slot.0.as_ref() {
        unsafe { context.Flush() };
    }
    if let Some(device) = slot.1.as_ref() {
        unsafe { device.Trim() };
    }
}

pub(crate) fn try_to_recover_from_device_lost<T>(mut f: impl FnMut() -> Result<T>) -> Result<T> {
    (0..5)
        .map(|i| {
            if i > 0 {
                // Add a small delay before retrying
                std::thread::sleep(std::time::Duration::from_millis(100 + i * 10));
            }
            f()
        })
        .find_or_last(Result::is_ok)
        .unwrap()
        .context("DirectXRenderer failed to recover from lost device after multiple attempts")
}

#[derive(Clone)]
pub(crate) struct DirectXDevices {
    pub(crate) adapter: IDXGIAdapter1,
    pub(crate) dxgi_factory: IDXGIFactory2,
    pub(crate) device: ID3D11Device,
    pub(crate) device_context: ID3D11DeviceContext,
}

impl DirectXDevices {
    pub(crate) fn new() -> Result<Self> {
        let debug_layer_available = check_debug_layer_available();
        let dxgi_factory =
            get_dxgi_factory(debug_layer_available).context("Creating DXGI factory")?;
        let (adapter, device, device_context, feature_level) =
            get_adapter(&dxgi_factory, debug_layer_available).context("Getting DXGI adapter")?;
        match feature_level {
            D3D_FEATURE_LEVEL_11_1 => {
                log::info!("Created device with Direct3D 11.1 feature level.")
            }
            D3D_FEATURE_LEVEL_11_0 => {
                log::info!("Created device with Direct3D 11.0 feature level.")
            }
            D3D_FEATURE_LEVEL_10_1 => {
                log::info!("Created device with Direct3D 10.1 feature level.")
            }
            _ => unreachable!(),
        }

        Ok(Self {
            adapter,
            dxgi_factory,
            device,
            device_context,
        })
        .inspect(|devices| remember_device(&devices.device, &devices.device_context))
    }
}

#[inline]
fn check_debug_layer_available() -> bool {
    #[cfg(debug_assertions)]
    {
        use windows::Win32::Graphics::Dxgi::{DXGIGetDebugInterface1, IDXGIInfoQueue};

        unsafe { DXGIGetDebugInterface1::<IDXGIInfoQueue>(0) }
            .log_err()
            .is_some()
    }
    #[cfg(not(debug_assertions))]
    {
        false
    }
}

#[inline]
fn get_dxgi_factory(debug_layer_available: bool) -> Result<IDXGIFactory2> {
    let factory_flag = if debug_layer_available {
        DXGI_CREATE_FACTORY_DEBUG
    } else {
        #[cfg(debug_assertions)]
        log::warn!(
            "Failed to get DXGI debug interface. DirectX debugging features will be disabled."
        );
        DXGI_CREATE_FACTORY_FLAGS::default()
    };
    // Bolume patch：这里只要 `IDXGIFactory2` —— 下面用到的枚举适配器、建交换链、
    // 关 Alt+Enter 全在 DXGI 1.2 里。上游写的是 `IDXGIFactory6`，那是 Win10 1803
    // （DXGI 1.6）才有的接口：虚拟机和更老的 Win10 上 `CreateDXGIFactory2` 创建
    // 完再 QI 到 1.6 会拿到 `E_NOINTERFACE`(0x80004002)，进程启动即崩。
    // 先按原样建（降到 1.2），再退到 `CreateDXGIFactory1`（Win7 SP1 + 平台更新就有）。
    if let Ok(factory) = unsafe { CreateDXGIFactory2::<IDXGIFactory2>(factory_flag) } {
        return Ok(factory);
    }
    unsafe { CreateDXGIFactory1::<IDXGIFactory2>() }.context("Creating DXGI factory")
}

#[inline]
fn get_adapter(
    dxgi_factory: &IDXGIFactory2,
    debug_layer_available: bool,
) -> Result<(
    IDXGIAdapter1,
    ID3D11Device,
    ID3D11DeviceContext,
    D3D_FEATURE_LEVEL,
)> {
    for adapter_index in 0.. {
        let Ok(adapter) = (unsafe { dxgi_factory.EnumAdapters(adapter_index) }) else {
            break;
        };
        let adapter: IDXGIAdapter1 = adapter.cast()?;
        if let Ok(desc) = unsafe { adapter.GetDesc1() } {
            let gpu_name = String::from_utf16_lossy(&desc.Description)
                .trim_matches(char::from(0))
                .to_string();
            log::info!("Using GPU: {}", gpu_name);
        }
        // Check to see whether the adapter supports Direct3D 11 and create
        // the device if it does.
        let mut context: Option<ID3D11DeviceContext> = None;
        let mut feature_level = D3D_FEATURE_LEVEL::default();
        if let Some(device) = get_device(
            Some(&adapter),
            Some(&mut context),
            Some(&mut feature_level),
            debug_layer_available,
        )
        .log_err()
        {
            return Ok((adapter, device, context.unwrap(), feature_level));
        }
    }

    // Bolume patch：一块能建出 D3D11 设备的卡都没有时退回 WARP（软件光栅）。
    // 虚拟机里只装了「Microsoft 基本显示适配器」、或者精简过的驱动包里
    // 连 WARP 都没列进适配器列表时，硬走上面那圈就只有一句
    // 「Getting DXGI adapter」然后启动失败。
    log::warn!("No adapter could create a Direct3D 11 device; falling back to WARP.");
    let mut context: Option<ID3D11DeviceContext> = None;
    let mut feature_level = D3D_FEATURE_LEVEL::default();
    let device = get_device(
        None,
        Some(&mut context),
        Some(&mut feature_level),
        debug_layer_available,
    )
    .context("Creating a WARP device")?;
    let dxgi_device: IDXGIDevice = device.cast().context("Getting the WARP DXGI device")?;
    let adapter: IDXGIAdapter1 = unsafe { dxgi_device.GetAdapter() }
        .and_then(|adapter| adapter.cast())
        .context("Getting the WARP adapter")?;
    Ok((adapter, device, context.unwrap(), feature_level))
}

#[inline]
fn get_device(
    adapter: Option<&IDXGIAdapter1>,
    context: Option<*mut Option<ID3D11DeviceContext>>,
    feature_level: Option<*mut D3D_FEATURE_LEVEL>,
    debug_layer_available: bool,
) -> Result<ID3D11Device> {
    let mut device: Option<ID3D11Device> = None;
    let device_flags = if debug_layer_available {
        D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_DEBUG
    } else {
        D3D11_CREATE_DEVICE_BGRA_SUPPORT
    };
    // 给了适配器就走 `UNKNOWN`（由适配器决定软硬）；没给就是 WARP。
    let driver_type = if adapter.is_some() {
        D3D_DRIVER_TYPE_UNKNOWN
    } else {
        D3D_DRIVER_TYPE_WARP
    };
    // `D3D11CreateDevice` 收的是 `IDXGIAdapter`，`IDXGIAdapter1` 得转一手。
    let target: Option<IDXGIAdapter> = match adapter {
        Some(adapter) => Some(adapter.cast()?),
        None => None,
    };
    unsafe {
        D3D11CreateDevice(
            target.as_ref(),
            driver_type,
            HMODULE::default(),
            device_flags,
            // 4x MSAA is required for Direct3D Feature Level 10.1 or better
            Some(&[
                D3D_FEATURE_LEVEL_11_1,
                D3D_FEATURE_LEVEL_11_0,
                D3D_FEATURE_LEVEL_10_1,
            ]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            feature_level,
            context,
        )?;
    }
    let device = device.unwrap();
    let mut data = D3D11_FEATURE_DATA_D3D10_X_HARDWARE_OPTIONS::default();
    unsafe {
        device
            .CheckFeatureSupport(
                D3D11_FEATURE_D3D10_X_HARDWARE_OPTIONS,
                &mut data as *mut _ as _,
                std::mem::size_of::<D3D11_FEATURE_DATA_D3D10_X_HARDWARE_OPTIONS>() as u32,
            )
            .context("Checking GPU device feature support")?;
    }
    if data
        .ComputeShaders_Plus_RawAndStructuredBuffers_Via_Shader_4_x
        .as_bool()
    {
        Ok(device)
    } else {
        Err(anyhow::anyhow!(
            "Required feature StructuredBuffer is not supported by GPU/driver"
        ))
    }
}
