// Global hotkey support — Alt+Space toggles window visibility.
// Runs in its own thread; posts WM_TOGGLE_WINDOW (WM_USER+3) to the main window.

use windows::Win32::Foundation::HWND;

pub fn register_and_listen(hwnd: HWND) {
    let hwnd_ptr = hwnd.0 as usize;
    std::thread::spawn(move || {
        let hwnd = HWND(hwnd_ptr as *mut core::ffi::c_void);
        use windows::Win32::UI::WindowsAndMessaging::{
            GetMessageW, TranslateMessage, DispatchMessageW, WM_HOTKEY, MSG, PostMessageW,
        };
        use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, HOT_KEY_MODIFIERS};
        use windows::Win32::Foundation::{WPARAM, LPARAM};

        unsafe {
            // MOD_ALT (0x0001) + VK_SPACE (0x20)
            let _ = RegisterHotKey(
                Some(HWND(std::ptr::null_mut())),
                1,
                HOT_KEY_MODIFIERS(0x0001),
                0x20,
            );

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, Some(HWND(std::ptr::null_mut())), 0, 0).as_bool() {
                if msg.message == WM_HOTKEY {
                    let _ = PostMessageW(Some(hwnd), 0x0400 + 3, WPARAM(0), LPARAM(0));
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    });
}