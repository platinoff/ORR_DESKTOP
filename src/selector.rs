use crate::recorder::Rect;
use std::cell::RefCell;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, EndPaint, FillRect, InvalidateRect, UpdateWindow,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, VK_ESCAPE};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetMessageW, GetSystemMetrics, IDC_CROSS, LWA_ALPHA, LoadCursorW, PostQuitMessage,
    RegisterClassW, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    SW_SHOW, SetLayeredWindowAttributes, ShowWindow, TranslateMessage, UnregisterClassW,
    WM_DESTROY, WM_KEYDOWN,
};

use crate::capture::wgcap::enumerate_monitors;

const WS_EX_LAYERED: u32 = 0x00080000;
const WS_EX_TOPMOST: u32 = 0x00000008;
const WS_EX_TOOLWINDOW: u32 = 0x00000080;
const WS_POPUP: u32 = 0x80000000;
const WM_PAINT: u32 = 0x000F;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONUP: u32 = 0x0202;

#[derive(Debug, Clone)]
pub enum Outcome {
    Selected(Rect),
    Cancelled(String),
}

fn rgb(r: u32, g: u32, b: u32) -> u32 {
    r | (g << 8) | (b << 16)
}

#[derive(Clone)]
struct OverlayState {
    origin: Option<(i32, i32)>,
    current: (i32, i32),
    result: Option<Outcome>,
}

thread_local! {
    static STATE: RefCell<OverlayState> = const { RefCell::new(OverlayState {
        origin: None,
        current: (0, 0),
        result: None,
    }) };
}

pub fn select_region<F: FnOnce(Outcome) + Send + 'static>(callback: F) {
    std::thread::spawn(move || {
        let r = unsafe { run_selector() };
        callback(r);
    });
}

unsafe fn run_selector() -> Outcome {
    unsafe {
        let monitors = enumerate_monitors().unwrap_or_default();
        let _primary = monitors
            .iter()
            .find(|m| m.primary)
            .cloned()
            .unwrap_or_else(|| {
                monitors
                    .first()
                    .cloned()
                    .expect("at least one monitor should exist after enumerate_monitors")
            });

        let hinstance = GetModuleHandleW(null_mut());
        let class_name: Vec<u16> = "ORR_SELECT_OVERLAY\0".encode_utf16().collect();

        let wc = windows_sys::Win32::UI::WindowsAndMessaging::WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: null_mut(),
            hCursor: LoadCursorW(null_mut(), IDC_CROSS),
            hbrBackground: null_mut(),
            lpszMenuName: null_mut(),
            lpszClassName: class_name.as_ptr(),
        };
        RegisterClassW(&wc);

        let (vx, vy, vw, vh) = virtual_bounds();

        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class_name.as_ptr(),
            null_mut(),
            WS_POPUP,
            vx,
            vy,
            vw,
            vh,
            null_mut(),
            null_mut(),
            hinstance,
            null_mut(),
        );
        if hwnd.is_null() {
            return Outcome::Cancelled("failed to create overlay window".into());
        }

        SetLayeredWindowAttributes(hwnd, 0, 170, LWA_ALPHA);
        ShowWindow(hwnd, SW_SHOW);

        let mut msg: windows_sys::Win32::UI::WindowsAndMessaging::MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let out = STATE.with_borrow_mut(|s| {
            s.result
                .take()
                .unwrap_or_else(|| Outcome::Cancelled("closed".into()))
        });

        DestroyWindow(hwnd);
        UnregisterClassW(class_name.as_ptr(), hinstance);
        out
    }
}

unsafe fn virtual_bounds() -> (i32, i32, i32, i32) {
    let vx = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let vy = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let vw = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let vh = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    (vx, vy, vw, vh)
}

fn pt(lparam: LPARAM) -> (i32, i32) {
    let x = (lparam & 0xFFFF) as u16 as i16 as i32;
    let y = ((lparam >> 16) & 0xFFFF) as u16 as i16 as i32;
    (x, y)
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_LBUTTONDOWN => {
                let (x, y) = pt(lparam);
                STATE.with_borrow_mut(|s| {
                    s.origin = Some((x, y));
                    s.current = (x, y);
                });
                SetCapture(hwnd);
                force_repaint(hwnd);
                0
            }
            WM_MOUSEMOVE => {
                let dragging = STATE.with(|s| s.borrow().origin.is_some());
                if dragging {
                    let (x, y) = pt(lparam);
                    STATE.with_borrow_mut(|s| s.current = (x, y));
                    force_repaint(hwnd);
                }
                0
            }
            WM_LBUTTONUP => {
                ReleaseCapture();
                let done = STATE.with_borrow_mut(|s| match s.origin.take() {
                    Some((ox, oy)) => {
                        let (x, y) = pt(lparam);
                        let rx = ox.min(x);
                        let ry = oy.min(y);
                        let rw = (ox - x).abs().max(0) as u32;
                        let rh = (oy - y).abs().max(0) as u32;
                        if rw >= 16 && rh >= 16 {
                            s.result = Some(Outcome::Selected(Rect {
                                x: rx,
                                y: ry,
                                w: rw,
                                h: rh,
                            }));
                            true
                        } else {
                            s.result = Some(Outcome::Cancelled("selection too small".into()));
                            false
                        }
                    }
                    None => false,
                });
                if done {
                    PostQuitMessage(0);
                }
                0
            }
            WM_KEYDOWN => {
                if wparam as u16 == VK_ESCAPE {
                    STATE.with_borrow_mut(|s| {
                        s.origin = None;
                        s.result = Some(Outcome::Cancelled("cancelled".into()));
                    });
                    PostQuitMessage(0);
                }
                0
            }
            WM_PAINT => paint(hwnd),
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

unsafe fn paint(hwnd: HWND) -> LRESULT {
    unsafe {
        let mut ps: windows_sys::Win32::Graphics::Gdi::PAINTSTRUCT = std::mem::zeroed();
        let hdc = BeginPaint(hwnd, &mut ps);
        let st = STATE.with(|s| s.borrow().clone());
        FillRect(hdc, &ps.rcPaint, CreateSolidBrush(rgb(18, 18, 24)));
        if let Some((ox, oy)) = st.origin {
            let (cx, cy) = st.current;
            let rx = ox.min(cx);
            let ry = oy.min(cy);
            let rw = (ox - cx).abs();
            let rh = (oy - cy).abs();
            let brush = CreateSolidBrush(rgb(60, 232, 110));
            let t = 2;
            let bars = [
                RECT {
                    left: rx,
                    top: ry,
                    right: rx + rw,
                    bottom: ry + t,
                },
                RECT {
                    left: rx,
                    top: ry + rh - t,
                    right: rx + rw,
                    bottom: ry + rh,
                },
                RECT {
                    left: rx,
                    top: ry,
                    right: rx + t,
                    bottom: ry + rh,
                },
                RECT {
                    left: rx + rw - t,
                    top: ry,
                    right: rx + rw,
                    bottom: ry + rh,
                },
            ];
            for b in &bars {
                FillRect(hdc, b, brush);
            }
        }
        EndPaint(hwnd, &ps);
        0
    }
}

fn force_repaint(hwnd: HWND) {
    unsafe {
        InvalidateRect(hwnd, null_mut(), 0);
        UpdateWindow(hwnd);
    }
}
