//! Windows: WASAPI process loopback (Windows 10 2004+ / Windows 11).
//! Captures only the audio rendered by the window's process tree.

use super::{AudioCallback, Source};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;
use windows::core::{implement, Interface, Ref, HRESULT, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, WAIT_OBJECT_0};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, BLOB, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW,
    WaitForSingleObject, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::Variant::VT_BLOB;
use windows::Win32::UI::WindowsAndMessaging::*;

struct ComGuard;
impl ComGuard {
    fn new() -> Self {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        ComGuard
    }
}
impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

// ---------------------------------------------------------------- listing

fn exe_name(pid: u32) -> String {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::new();
        };
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        if ok.is_err() {
            return String::new();
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        let file = path.rsplit('\\').next().unwrap_or(&path);
        file.strip_suffix(".exe").or(file.strip_suffix(".EXE")).unwrap_or(file).to_string()
    }
}

fn parent_map() -> HashMap<u32, u32> {
    let mut map = HashMap::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return map;
        };
        let mut e = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                map.insert(e.th32ProcessID, e.th32ParentProcessID);
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    map
}

/// PIDs (and all their ancestors) that currently have an active, non-silent
/// audio session on the default output device.
fn audible_pids() -> HashSet<u32> {
    let mut out = HashSet::new();
    let _com = ComGuard::new();
    let r: windows::core::Result<()> = (|| unsafe {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let dev = en.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let mgr: IAudioSessionManager2 = dev.Activate(CLSCTX_ALL, None)?;
        let list = mgr.GetSessionEnumerator()?;
        let parents = parent_map();
        for i in 0..list.GetCount()? {
            let ctl = list.GetSession(i)?;
            if ctl.GetState()? != AudioSessionStateActive {
                continue;
            }
            let Ok(ctl2) = ctl.cast::<IAudioSessionControl2>() else { continue };
            let Ok(meter) = ctl.cast::<IAudioMeterInformation>() else { continue };
            if meter.GetPeakValue().unwrap_or(0.0) <= 0.0001 {
                continue;
            }
            let mut pid = ctl2.GetProcessId().unwrap_or(0);
            // Walk up the tree: browsers play audio from a child process.
            for _ in 0..16 {
                if pid == 0 || !out.insert(pid) {
                    break;
                }
                pid = *parents.get(&pid).unwrap_or(&0);
            }
        }
        Ok(())
    })();
    let _ = r;
    out
}

pub fn list_sources() -> Result<Vec<Source>, String> {
    struct Ctx {
        me: u32,
        out: Vec<Source>,
    }
    unsafe extern "system" fn cb(hwnd: HWND, lp: LPARAM) -> windows::core::BOOL {
        let ctx = &mut *(lp.0 as *mut Ctx);
        if !IsWindowVisible(hwnd).as_bool() {
            return true.into();
        }
        if GetWindow(hwnd, GW_OWNER).map(|h| !h.is_invalid()).unwrap_or(false) {
            return true.into();
        }
        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TOOLWINDOW.0 != 0 {
            return true.into();
        }
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut _,
            size_of::<u32>() as u32,
        );
        if cloaked != 0 {
            return true.into();
        }
        let len = GetWindowTextLengthW(hwnd);
        if len == 0 {
            return true.into();
        }
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(hwnd, &mut buf);
        let title = String::from_utf16_lossy(&buf[..n as usize]);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == ctx.me || title == "Program Manager" {
            return true.into();
        }
        ctx.out.push(Source {
            id: hwnd.0 as usize as u64,
            pid,
            app: exe_name(pid),
            title,
            audible: false,
        });
        true.into()
    }

    let mut ctx = Ctx { me: unsafe { GetCurrentProcessId() }, out: Vec::new() };
    unsafe {
        EnumWindows(Some(cb), LPARAM(&mut ctx as *mut Ctx as isize)).map_err(|e| e.to_string())?;
    }
    let audible = audible_pids();
    for s in &mut ctx.out {
        s.audible = audible.contains(&s.pid);
    }
    Ok(ctx.out)
}

// ---------------------------------------------------------------- capture

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated(mpsc::Sender<()>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(
        &self,
        _op: Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        let _ = self.0.send(());
        Ok(())
    }
}

fn activate_process_loopback(pid: u32) -> Result<IAudioClient, String> {
    let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    // ManuallyDrop: PROPVARIANT's Drop calls PropVariantClear, which would try
    // to CoTaskMemFree our stack-allocated blob.
    let pv = std::mem::ManuallyDrop::new(PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                        pBlobData: &mut params as *mut _ as *mut u8,
                    },
                },
            }),
        },
    });

    let (tx, rx) = mpsc::channel();
    let handler: IActivateAudioInterfaceCompletionHandler = Activated(tx).into();
    unsafe {
        let op = ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&*pv),
            &handler,
        )
        .map_err(|e| format!("ActivateAudioInterfaceAsync: {e}"))?;
        rx.recv_timeout(Duration::from_secs(5))
            .map_err(|_| "timed out activating process loopback".to_string())?;
        let mut hr = HRESULT(0);
        let mut unk = None;
        op.GetActivateResult(&mut hr, &mut unk).map_err(|e| e.to_string())?;
        hr.ok().map_err(|e| format!("process loopback unavailable (needs Windows 10 2004+): {e}"))?;
        unk.ok_or("no audio client")?.cast::<IAudioClient>().map_err(|e| e.to_string())
    }
}

pub fn run(
    src: &Source,
    cb: &mut AudioCallback,
    stop: &AtomicBool,
    ready: impl FnOnce(Result<(), String>),
) -> Result<(), String> {
    let _com = ComGuard::new();
    const RATE: u32 = 48_000;
    const CH: u16 = 2;
    let client = activate_process_loopback(src.pid)?;
    let fmt = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: CH,
        nSamplesPerSec: RATE,
        nAvgBytesPerSec: RATE * CH as u32 * 2,
        nBlockAlign: CH * 2,
        wBitsPerSample: 16,
        cbSize: 0,
    };
    let capture: IAudioCaptureClient;
    let event;
    unsafe {
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK
                    | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                2_000_000, // 200 ms
                0,
                &fmt,
                None,
            )
            .map_err(|e| format!("IAudioClient::Initialize: {e}"))?;
        event = CreateEventW(None, false, false, None).map_err(|e| e.to_string())?;
        client.SetEventHandle(event).map_err(|e| e.to_string())?;
        capture = client.GetService().map_err(|e| e.to_string())?;
        client.Start().map_err(|e| e.to_string())?;
    }
    ready(Ok(()));

    let mut buf: Vec<f32> = Vec::with_capacity(RATE as usize);
    let result = (|| -> windows::core::Result<()> {
        while !stop.load(Ordering::Relaxed) {
            unsafe {
                if WaitForSingleObject(event, 100) != WAIT_OBJECT_0 {
                    continue;
                }
                while capture.GetNextPacketSize()? > 0 {
                    let mut data = std::ptr::null_mut();
                    let mut frames = 0u32;
                    let mut flags = 0u32;
                    capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                    let n = frames as usize * CH as usize;
                    buf.clear();
                    if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                        buf.resize(n, 0.0);
                    } else {
                        let s = std::slice::from_raw_parts(data as *const i16, n);
                        buf.extend(s.iter().map(|&x| x as f32 / 32768.0));
                    }
                    capture.ReleaseBuffer(frames)?;
                    cb(&buf, CH as usize, RATE);
                }
            }
        }
        Ok(())
    })();
    unsafe {
        let _ = client.Stop();
        let _ = CloseHandle(event);
    }
    result.map_err(|e| format!("capture failed: {e}"))
}
