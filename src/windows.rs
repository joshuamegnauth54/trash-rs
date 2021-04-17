use std::{
    ffi::{c_void, OsStr, OsString},
    fmt::format,
    mem::MaybeUninit,
    ops::DerefMut,
    os::{
        raw::c_int,
        windows::{ffi::OsStrExt, prelude::*},
    },
    path::{Path, PathBuf},
    ptr::null_mut,
};

use scopeguard::defer;

mod bindings {
    ::windows::include_bindings!();
}

use bindings::Windows::Win32::{
    Automation::*, Com::*, Shell::*, SystemServices::*, WindowsAndMessaging::*,
    WindowsProgramming::*, WindowsPropertiesSystem::*,
};
use windows::{Guid, IUnknown, Interface, IntoParam, Param, RuntimeType, HRESULT};

struct WinNull;
impl<'a> IntoParam<'a, IBindCtx> for WinNull {
    fn into_param(self) -> Param<'a, IBindCtx> {
        Param::None
    }
}
impl<'a> IntoParam<'a, IUnknown> for WinNull {
    fn into_param(self) -> Param<'a, IUnknown> {
        Param::None
    }
}
impl<'a> IntoParam<'a, IFileOperationProgressSink> for WinNull {
    fn into_param(self) -> Param<'a, IFileOperationProgressSink> {
        Param::None
    }
}

///////////////////////////////////////////////////////////////////////////
// These don't have bindings in windows-rs for some reason
///////////////////////////////////////////////////////////////////////////
const PSGUID_DISPLACED: Guid =
    Guid::from_values(0x9b174b33, 0x40ff, 0x11d2, [0xa2, 0x7e, 0x00, 0xc0, 0x4f, 0xc3, 0x8, 0x71]);
const PID_DISPLACED_FROM: u32 = 2;
const PID_DISPLACED_DATE: u32 = 3;
const SCID_ORIGINAL_LOCATION: PROPERTYKEY =
    PROPERTYKEY { fmtid: PSGUID_DISPLACED, pid: PID_DISPLACED_FROM };
const SCID_DATE_DELETED: PROPERTYKEY =
    PROPERTYKEY { fmtid: PSGUID_DISPLACED, pid: PID_DISPLACED_DATE };

const FOF_SILENT: u32 = 0x0004;
const FOF_RENAMEONCOLLISION: u32 = 0x0008;
const FOF_NOCONFIRMATION: u32 = 0x0010;
const FOF_WANTMAPPINGHANDLE: u32 = 0x0020;
const FOF_ALLOWUNDO: u32 = 0x0040;
const FOF_FILESONLY: u32 = 0x0080;
const FOF_SIMPLEPROGRESS: u32 = 0x0100;
const FOF_NOCONFIRMMKDIR: u32 = 0x0200;
const FOF_NOERRORUI: u32 = 0x0400;
const FOF_NOCOPYSECURITYATTRIBS: u32 = 0x0800;
const FOF_NORECURSION: u32 = 0x1000;
const FOF_NO_CONNECTED_ELEMENTS: u32 = 0x2000;
const FOF_WANTNUKEWARNING: u32 = 0x4000;
const FOF_NO_UI: u32 = FOF_SILENT | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_NOCONFIRMMKDIR;
///////////////////////////////////////////////////////////////////////////

use crate::{Error, TrashItem};

macro_rules! return_err_on_fail {
    {$f_name:ident($($args:tt)*)} => ({
        let hr = $f_name($($args)*);
        if hr.is_err() {
            return Err(Error::Unknown {
                description: format!("`{}` failed with the result: {:?}", stringify!($f_name), hr)
            });
        }
        hr
    });
    {$obj:ident.$f_name:ident($($args:tt)*)} => ({
        return_err_on_fail!{($obj).$f_name($($args)*)}
    });
    {($obj:expr).$f_name:ident($($args:tt)*)} => ({
        let hr = ($obj).$f_name($($args)*);
        if hr.is_err() {
            return Err(Error::Unknown {
                description: format!("`{}` failed with the result: {:?}", stringify!($f_name), hr)
            });
        }
        hr
    })
}

/// See https://docs.microsoft.com/en-us/windows/win32/api/shellapi/ns-shellapi-_shfileopstructa
pub fn delete_all_canonicalized(full_paths: Vec<PathBuf>) -> Result<(), Error> {
    ensure_com_initialized();
    unsafe {
        let recycle_bin: IShellFolder2 = bind_to_csidl(CSIDL_BITBUCKET as c_int)?;
        // let mut pbc = MaybeUninit::<*mut IBindCtx>::uninit();
        // return_err_on_fail! { CreateBindCtx(0, pbc.as_mut_ptr()) };
        // let pbc = pbc.assume_init();
        // defer! {{ (*pbc).Release(); }}
        // (*pbc).
        let mut pfo = MaybeUninit::<IFileOperation>::uninit();
        return_err_on_fail! {
            CoCreateInstance(
                &FileOperation as *const _,
                WinNull,
                CLSCTX::CLSCTX_ALL,
                &IFileOperation::IID as *const _,
                pfo.as_mut_ptr() as *mut *mut c_void,
            )
        };
        let pfo = pfo.assume_init();
        return_err_on_fail! { pfo.SetOperationFlags(FOF_NO_UI | FOF_ALLOWUNDO | FOF_WANTNUKEWARNING) };
        for full_path in full_paths.iter() {
            let path_prefix = ['\\' as u16, '\\' as u16, '?' as u16, '\\' as u16];
            let mut wide_path_container: Vec<_> =
                full_path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
            let wide_path_slice = if wide_path_container.starts_with(&path_prefix) {
                &mut wide_path_container[path_prefix.len()..]
            } else {
                &mut wide_path_container[0..]
            };
            let mut shi = MaybeUninit::<IShellItem>::uninit();
            return_err_on_fail! {
                SHCreateItemFromParsingName(
                    PWSTR(wide_path_slice.as_mut_ptr()),
                    WinNull,
                    &IShellItem::IID as *const _,
                    shi.as_mut_ptr() as *mut *mut c_void,
                )
            };
            let shi = shi.assume_init();
            return_err_on_fail! { pfo.DeleteItem(shi, WinNull) };
        }
        return_err_on_fail! { pfo.PerformOperations() };
        Ok(())
    }
}

pub fn list() -> Result<Vec<TrashItem>, Error> {
    ensure_com_initialized();
    unsafe {
        let mut recycle_bin: IShellFolder2 = bind_to_csidl(CSIDL_BITBUCKET as c_int)?;
        let mut peidl = MaybeUninit::<Option<IEnumIDList>>::uninit();
        let flags = _SHCONTF::SHCONTF_FOLDERS.0 | _SHCONTF::SHCONTF_NONFOLDERS.0;
        let hr = return_err_on_fail! {
            recycle_bin.EnumObjects(
                HWND::NULL,
                flags as u32,
                peidl.as_mut_ptr(),
            )
        };
        // WARNING `hr.is_ok()` is DIFFERENT from `hr == S_OK`, because
        // `is_ok` returns true if the HRESULT as any of the several success codes
        // but here we want to be more strict and only accept S_OK.
        if hr != S_OK {
            return Err(Error::Unknown {
                description: format!(
                    "`EnumObjects` returned with HRESULT {:X}, but 0x0 was expected.",
                    hr.0
                ),
            });
        }
        let peidl = peidl.assume_init().ok_or_else(|| Error::Unknown {
            description: format!("`EnumObjects` set its output to None."),
        })?;
        let mut item_vec = Vec::new();
        let mut item_uninit = MaybeUninit::<*mut ITEMIDLIST>::uninit();
        while peidl.Next(1, item_uninit.as_mut_ptr(), std::ptr::null_mut()) == S_OK {
            let item = item_uninit.assume_init();
            defer! {{ CoTaskMemFree(item as *mut c_void); }}
            let id = get_display_name((&recycle_bin).into(), item, _SHGDNF::SHGDN_FORPARSING)?;
            let name = get_display_name((&recycle_bin).into(), item, _SHGDNF::SHGDN_INFOLDER)?;

            let orig_loc = get_detail(&recycle_bin, item, &SCID_ORIGINAL_LOCATION as *const _)?;
            let date_deleted = get_date_unix(&recycle_bin, item, &SCID_DATE_DELETED as *const _)?;

            item_vec.push(TrashItem {
                id,
                name: name.into_string().map_err(|original| Error::ConvertOsString { original })?,
                original_parent: PathBuf::from(orig_loc),
                time_deleted: date_deleted,
            });
        }
        return Ok(item_vec);
    }
}

pub fn purge_all<I>(items: I) -> Result<(), Error>
where
    I: IntoIterator<Item = TrashItem>,
{
    todo!()
}

pub fn restore_all<I>(items: I) -> Result<(), Error>
where
    I: IntoIterator<Item = TrashItem>,
{
    todo!();
}

unsafe fn get_display_name(
    psf: IShellFolder,
    pidl: *mut ITEMIDLIST,
    flags: _SHGDNF,
) -> Result<OsString, Error> {
    let mut sr = MaybeUninit::<STRRET>::uninit();
    return_err_on_fail! { psf.GetDisplayNameOf(pidl, flags.0 as u32, sr.as_mut_ptr()) };
    let mut sr = sr.assume_init();
    let mut name = MaybeUninit::<PWSTR>::uninit();
    return_err_on_fail! { StrRetToStrW(&mut sr as *mut _, pidl, name.as_mut_ptr()) };
    let name = name.assume_init();
    let result = wstr_to_os_string(name);
    CoTaskMemFree(name.0 as *mut c_void);
    Ok(result)
}

unsafe fn wstr_to_os_string(wstr: PWSTR) -> OsString {
    let mut len = 0;
    while *(wstr.0.offset(len)) != 0 {
        len += 1;
    }
    let wstr_slice = std::slice::from_raw_parts(wstr.0, len as usize);
    OsString::from_wide(wstr_slice)
}

unsafe fn get_detail(
    psf: &IShellFolder2,
    pidl: *mut ITEMIDLIST,
    pscid: *const PROPERTYKEY,
) -> Result<OsString, Error> {
    let mut vt = MaybeUninit::<VARIANT>::uninit();
    return_err_on_fail! { psf.GetDetailsEx(pidl, pscid, vt.as_mut_ptr()) };
    let vt = vt.assume_init();
    let mut vt = scopeguard::guard(vt, |mut vt| {
        VariantClear(&mut vt as *mut _);
    });
    return_err_on_fail! {
        VariantChangeType(vt.deref_mut() as *mut _, vt.deref_mut() as *mut _, 0, VARENUM::VT_BSTR.0 as u16)
    };
    let pstr = vt.Anonymous.Anonymous.Anonymous.bstrVal;
    let result = Ok(wstr_to_os_string(PWSTR(pstr)));
    return result;
}

unsafe fn get_date_unix(
    psf: &IShellFolder2,
    pidl: *mut ITEMIDLIST,
    pscid: *const PROPERTYKEY,
) -> Result<i64, Error> {
    let mut vt = MaybeUninit::<VARIANT>::uninit();
    return_err_on_fail! { psf.GetDetailsEx(pidl, pscid, vt.as_mut_ptr()) };
    let vt = vt.assume_init();
    let mut vt = scopeguard::guard(vt, |mut vt| {
        VariantClear(&mut vt as *mut _);
    });
    return_err_on_fail! {
        VariantChangeType(vt.deref_mut() as *mut _, vt.deref_mut() as *mut _, 0, VARENUM::VT_DATE.0 as u16)
    };
    let date = vt.Anonymous.Anonymous.Anonymous.date;
    let unix_time = variant_time_to_unix_time(date)?;
    Ok(unix_time)
}

unsafe fn variant_time_to_unix_time(from: f64) -> Result<i64, Error> {
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct LargeIntegerParts {
        low_part: u32,
        high_part: u32,
    }
    #[repr(C)]
    union LargeInteger {
        parts: LargeIntegerParts,
        whole: u64,
    }
    let mut st = MaybeUninit::<SYSTEMTIME>::uninit();
    if 0 == VariantTimeToSystemTime(from, st.as_mut_ptr()) {
        return Err(Error::Unknown {
            description: format!(
                "`VariantTimeToSystemTime` indicated failure for the parameter {:?}",
                from
            ),
        });
    }
    let st = st.assume_init();
    let mut ft = MaybeUninit::<FILETIME>::uninit();
    if SystemTimeToFileTime(&st, ft.as_mut_ptr()) == false {
        return Err(Error::Unknown {
            description: format!(
                "`SystemTimeToFileTime` failed with: {:?}",
                HRESULT::from_thread()
            ),
        });
    }
    let ft = ft.assume_init();

    let large_int = LargeInteger {
        parts: LargeIntegerParts { low_part: ft.dwLowDateTime, high_part: ft.dwHighDateTime },
    };

    // Applying assume init straight away because there's no explicit support to initialize struct
    // fields one-by-one in an `MaybeUninit` as of Rust 1.39.0
    // See: https://github.com/rust-lang/rust/blob/1.39.0/src/libcore/mem/maybe_uninit.rs#L170
    // let mut uli = MaybeUninit::<ULARGE_INTEGER>::zeroed().assume_init();
    // {
    //     let u_mut = uli.u_mut();
    //     u_mut.LowPart = ft.dwLowDateTime;
    //     u_mut.HighPart = std::mem::transmute(ft.dwHighDateTime);
    // }
    let windows_ticks: u64 = large_int.whole;
    Ok(windows_ticks_to_unix_seconds(windows_ticks))
}

fn windows_ticks_to_unix_seconds(windows_ticks: u64) -> i64 {
    // Fun fact: if my calculations are correct, then storing sucn ticks in an
    // i64 can remain valid until about 6000 years from the very first tick
    const WINDOWS_TICK: u64 = 10000000;
    const SEC_TO_UNIX_EPOCH: i64 = 11644473600;
    return (windows_ticks / WINDOWS_TICK) as i64 - SEC_TO_UNIX_EPOCH;
}

unsafe fn bind_to_csidl<T: Interface>(csidl: c_int) -> Result<T, Error> {
    let mut pidl = MaybeUninit::<*mut ITEMIDLIST>::uninit();
    return_err_on_fail! {
        SHGetSpecialFolderLocation(HWND::NULL, csidl, pidl.as_mut_ptr())
    };
    let pidl = pidl.assume_init();
    defer! {{ CoTaskMemFree(pidl as _); }};

    let mut desktop = MaybeUninit::<Option<IShellFolder>>::uninit();
    return_err_on_fail! { SHGetDesktopFolder(desktop.as_mut_ptr()) };
    let desktop = desktop.assume_init();
    let desktop = desktop.ok_or_else(|| Error::Unknown {
        description: "`SHGetDesktopFolder` set its output to `None`.".into(),
    })?;
    if (*pidl).mkid.cb != 0 {
        let iid = T::IID;
        // let bind_ctx = MaybeUninit::<Option<IBindCtx>>::uninit();
        // return_err_on_fail! { CreateBindCtx(0, bind_ctx.as_mut_ptr()) };
        // let bind_ctx = bind_ctx.assume_init().ok_or_else(|| Error::Unknown {
        //     description: "CreateBindCtx returned None".into()
        // })?;

        // WARNING The following logic relies on the fact that T has an identical memory
        // layout to a pointer, and is treated like a pointer by the `windows-rs` implementation.
        // This logic follows how the IUnknown::cast function is implemented in windows-rs 0.8
        let mut target = MaybeUninit::<T>::uninit();
        return_err_on_fail! { desktop.BindToObject(pidl, WinNull, &iid as *const _, target.as_mut_ptr() as *mut *mut c_void) };
        Ok(target.assume_init())
    } else {
        Ok(desktop.cast().map_err(into_unknown)?)
    }
}

struct CoInitializer {}
impl CoInitializer {
    fn new() -> CoInitializer {
        //let first = INITIALIZER_THREAD_COUNT.fetch_add(1, Ordering::SeqCst) == 0;
        #[cfg(all(
            not(feature = "coinit_multithreaded"),
            not(feature = "coinit_apartmentthreaded")
        ))]
        {
            0 = "THIS IS AN ERROR ON PURPOSE. Either the `coinit_multithreaded` or the `coinit_apartmentthreaded` feature must be specified";
        }
        let mut init_mode;
        #[cfg(feature = "coinit_multithreaded")]
        {
            init_mode = COINIT::COINIT_MULTITHREADED;
        }
        #[cfg(feature = "coinit_apartmentthreaded")]
        {
            init_mode = COINIT::COINIT_APARTMENTTHREADED;
        }

        // These flags can be combined with either of coinit_multithreaded or coinit_apartmentthreaded.
        if cfg!(feature = "coinit_disable_ole1dde") {
            init_mode |= COINIT::COINIT_DISABLE_OLE1DDE;
        }
        if cfg!(feature = "coinit_speed_over_memory") {
            init_mode |= COINIT::COINIT_SPEED_OVER_MEMORY;
        }
        let hr = unsafe { CoInitializeEx(std::ptr::null_mut(), init_mode) };
        if hr.is_err() {
            panic!("Call to CoInitializeEx failed. HRESULT: {:?}. Consider using `trash` with the feature `coinit_multithreaded`", hr);
        }
        CoInitializer {}
    }
}
impl Drop for CoInitializer {
    fn drop(&mut self) {
        // TODO: This does not get called because it's a global static.
        // Is there an atexit in Win32?
        unsafe {
            CoUninitialize();
        }
    }
}
thread_local! {
    static CO_INITIALIZER: CoInitializer = CoInitializer::new();
}
fn ensure_com_initialized() {
    CO_INITIALIZER.with(|_| {});
}

fn into_unknown<E: std::fmt::Display>(err: E) -> Error {
    Error::Unknown { description: format!("{}", err) }
}
