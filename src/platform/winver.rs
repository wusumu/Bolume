//! 当前是哪一代 Windows —— 只用来给「跟随系统」挑一套动画参数。
//!
//! `GetVersionExW` 在没写 manifest 的进程里会撒谎（永远报 6.2），所以走 ntdll 的
//! `RtlGetVersion`：它返回的就是驱动里那份真版本。1709 之后的 manifest 其实也让
//! `GetVersionExW` 说实话了，但少踩一个坑少一个坑。

/// 系统主版本号（`10`）与 build 号（Win10 = 10240..19045，Win11 = 22000 起）。
#[cfg(windows)]
fn version() -> (u32, u32) {
    use std::sync::OnceLock;

    #[repr(C)]
    struct OsVersionInfoExW {
        size: u32,
        major: u32,
        minor: u32,
        build: u32,
        platform: u32,
        csd: [u16; 128],
        service_pack_major: u16,
        service_pack_minor: u16,
        suite_mask: u16,
        product_type: u8,
        reserved: u8,
    }

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn RtlGetVersion(info: *mut OsVersionInfoExW) -> i32;
    }

    static CACHE: OnceLock<(u32, u32)> = OnceLock::new();
    *CACHE.get_or_init(|| {
        // 字段有 128 个 u16 的数组（`Default` 不覆盖这么长的数组），直接清零。
        let mut info: OsVersionInfoExW = unsafe { std::mem::zeroed() };
        info.size = std::mem::size_of::<OsVersionInfoExW>() as u32;
        // STATUS_SUCCESS = 0
        if unsafe { RtlGetVersion(&mut info) } == 0 {
            (info.major, info.build)
        } else {
            (10, 0)
        }
    })
}

#[cfg(not(windows))]
fn version() -> (u32, u32) {
    (10, 0)
}

/// 是不是 Windows 11（build 22000 起）。
///
/// 注意 `ProductName` 一直写着 "Windows 10"，判断世代只能看 build 号。
pub fn is_windows_11() -> bool {
    build_number() >= 22000
}

/// build 号（给设置页 / trace 里写一行用）。
pub fn build_number() -> u32 {
    version().1
}

#[cfg(test)]
mod tests {
    use super::{build_number, is_windows_11};

    #[test]
    fn build_and_generation_agree() {
        let build = build_number();
        if build == 0 {
            return; // 非 Windows：不判
        }
        assert_eq!(is_windows_11(), build >= 22000);
    }
}
