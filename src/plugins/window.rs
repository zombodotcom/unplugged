//! A plain native window for a plugin's own editor to live in.
//!
//! On Windows this is a raw Win32 window. Its messages are dispatched by the app's
//! existing event loop (winit pumps every window on the UI thread), so no extra thread
//! or loop is needed. Closing it only sets a flag; the plugin host tears the editor down
//! on its next tick and then destroys the window.

#[cfg(windows)]
mod imp {
    use anyhow::{Result, anyhow};
    use parking_lot::Mutex;
    use std::ffi::c_void;
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    /// Windows whose close button was pressed (as integers so the list is `Send`).
    static CLOSE_REQUESTS: Mutex<Vec<isize>> = Mutex::new(Vec::new());

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_CLOSE {
            CLOSE_REQUESTS.lock().push(hwnd as isize);
            return 0;
        }
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    fn class_name() -> &'static [u16] {
        static CLASS: OnceLock<Vec<u16>> = OnceLock::new();
        CLASS.get_or_init(|| {
            let name = wide("UnpluggedPluginWindow");
            unsafe {
                let wc = WNDCLASSW {
                    style: CS_HREDRAW | CS_VREDRAW,
                    lpfnWndProc: Some(wndproc),
                    cbClsExtra: 0,
                    cbWndExtra: 0,
                    hInstance: GetModuleHandleW(std::ptr::null()),
                    hIcon: std::ptr::null_mut(),
                    hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
                    hbrBackground: std::ptr::null_mut(),
                    lpszMenuName: std::ptr::null(),
                    lpszClassName: name.as_ptr(),
                };
                RegisterClassW(&wc);
            }
            name
        })
    }

    const STYLE: u32 = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;

    pub struct EditorWindow {
        hwnd: HWND,
    }

    impl EditorWindow {
        pub fn new(title: &str, width: u32, height: u32) -> Result<Self> {
            let (w, h) = outer_size(width, height);
            let title = wide(title);
            let hwnd = unsafe {
                CreateWindowExW(
                    0,
                    class_name().as_ptr(),
                    title.as_ptr(),
                    STYLE,
                    CW_USEDEFAULT,
                    CW_USEDEFAULT,
                    w,
                    h,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    GetModuleHandleW(std::ptr::null()),
                    std::ptr::null(),
                )
            };
            if hwnd.is_null() {
                return Err(anyhow!("couldn't create a window for the plugin"));
            }
            // Tests set this so plugin windows never pop up on someone's screen.
            if std::env::var_os("UNPLUGGED_HIDDEN_PLUGIN_WINDOWS").is_none() {
                unsafe {
                    ShowWindow(hwnd, SW_SHOW);
                }
            }
            Ok(Self { hwnd })
        }

        pub fn raw(&self) -> *mut c_void {
            self.hwnd
        }

        /// True (once) after the user clicked the window's close button.
        pub fn close_requested(&self) -> bool {
            let mut list = CLOSE_REQUESTS.lock();
            let before = list.len();
            list.retain(|&h| h != self.hwnd as isize);
            list.len() != before
        }

        pub fn set_client_size(&self, width: u32, height: u32) {
            let (w, h) = outer_size(width, height);
            unsafe {
                SetWindowPos(
                    self.hwnd,
                    std::ptr::null_mut(),
                    0,
                    0,
                    w,
                    h,
                    SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
        }

        pub fn focus(&self) {
            unsafe {
                ShowWindow(self.hwnd, SW_RESTORE);
                SetForegroundWindow(self.hwnd);
            }
        }
    }

    fn outer_size(width: u32, height: u32) -> (i32, i32) {
        let mut rc = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        unsafe {
            AdjustWindowRectEx(&mut rc, STYLE, 0, 0);
        }
        (rc.right - rc.left, rc.bottom - rc.top)
    }

    impl Drop for EditorWindow {
        fn drop(&mut self) {
            unsafe {
                DestroyWindow(self.hwnd);
            }
            CLOSE_REQUESTS.lock().retain(|&h| h != self.hwnd as isize);
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use anyhow::{Result, anyhow};
    use std::ffi::c_void;

    pub struct EditorWindow;

    impl EditorWindow {
        pub fn new(_title: &str, _width: u32, _height: u32) -> Result<Self> {
            Err(anyhow!("plugin windows are Windows-only for now"))
        }
        pub fn raw(&self) -> *mut c_void {
            std::ptr::null_mut()
        }
        pub fn close_requested(&self) -> bool {
            false
        }
        pub fn set_client_size(&self, _w: u32, _h: u32) {}
        pub fn focus(&self) {}
    }
}

pub use imp::EditorWindow;
