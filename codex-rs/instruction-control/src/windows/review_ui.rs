use super::wide;
use anyhow::Result;
use anyhow::bail;
use std::ptr;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Foundation::LPARAM;
use windows_sys::Win32::Foundation::LRESULT;
use windows_sys::Win32::Foundation::WPARAM;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

struct State {
    accepted: bool,
    edit: HWND,
    accept: HWND,
    cancel: HWND,
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = unsafe { &*(lparam as *const CREATESTRUCTW) };
        unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, create.lpCreateParams as isize) };
    }
    let state = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *mut State;
    match message {
        WM_COMMAND if !state.is_null() => {
            let id = wparam & 0xffff;
            // Only these native buttons can accept; there is no command-line approval bypass.
            if id == 1 || id == 2 {
                unsafe {
                    (*state).accepted = id == 1;
                    DestroyWindow(window);
                }
                return 0;
            }
        }
        WM_SIZE if !state.is_null() => {
            let width = (lparam as u32 & 0xffff) as i32;
            let height = ((lparam as u32 >> 16) & 0xffff) as i32;
            unsafe {
                MoveWindow(
                    (*state).edit,
                    12,
                    12,
                    (width - 24).max(1),
                    (height - 76).max(1),
                    1,
                );
                MoveWindow(
                    (*state).accept,
                    (width - 312).max(12),
                    (height - 52).max(12),
                    144,
                    34,
                    1,
                );
                MoveWindow(
                    (*state).cancel,
                    (width - 156).max(12),
                    (height - 52).max(12),
                    144,
                    34,
                    1,
                );
            }
            return 0;
        }
        WM_CLOSE => {
            unsafe { DestroyWindow(window) };
            return 0;
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            return 0;
        }
        _ => {}
    }
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

/// Runs in the elevated helper. UIPI prevents the unelevated agent from sending
/// window messages to this review window. The original byte strings remain in memory.
pub(super) fn review(title: &str, text: &str) -> Result<()> {
    if text.contains('\0') {
        bail!("review contains NUL and cannot be displayed faithfully");
    }
    let instance = unsafe { GetModuleHandleW(ptr::null()) };
    let class_name = wide("CodexStrippedProtectedPolicyReview");
    let title = wide(title);
    let mut state = State {
        accepted: false,
        edit: ptr::null_mut(),
        accept: ptr::null_mut(),
        cancel: ptr::null_mut(),
    };
    let mut class: WNDCLASSW = unsafe { std::mem::zeroed() };
    class.lpfnWndProc = Some(window_proc);
    class.hInstance = instance;
    class.lpszClassName = class_name.as_ptr();
    class.hCursor = unsafe { LoadCursorW(ptr::null_mut(), IDC_ARROW) };
    if unsafe { RegisterClassW(&class) } == 0 {
        return Err(super::last_error("registering administrator review window"));
    }
    let window = unsafe {
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1100,
            800,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            ptr::addr_of_mut!(state).cast(),
        )
    };
    if window.is_null() {
        return Err(super::last_error("creating administrator review window"));
    }
    let edit_class = wide("EDIT");
    let button_class = wide("BUTTON");
    let header = "Read the previous and candidate content below. Accept applies exactly this candidate.\r\nCancel leaves accepted instructions unchanged.\r\n\r\n";
    let display = wide(format!("{header}{text}"));
    let accept = wide("Accept this version");
    let cancel = wide("Cancel");
    state.edit = unsafe {
        CreateWindowExW(
            WS_EX_CLIENTEDGE,
            edit_class.as_ptr(),
            ptr::null(),
            WS_CHILD
                | WS_VISIBLE
                | WS_VSCROLL
                | WS_HSCROLL
                | ES_MULTILINE as u32
                | ES_AUTOVSCROLL as u32
                | ES_AUTOHSCROLL as u32
                | ES_READONLY as u32,
            12,
            12,
            1060,
            670,
            window,
            ptr::null_mut(),
            instance,
            ptr::null(),
        )
    };
    state.accept = unsafe {
        CreateWindowExW(
            0,
            button_class.as_ptr(),
            accept.as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32,
            760,
            700,
            144,
            34,
            window,
            1_usize as HMENU,
            instance,
            ptr::null(),
        )
    };
    state.cancel = unsafe {
        CreateWindowExW(
            0,
            button_class.as_ptr(),
            cancel.as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32,
            916,
            700,
            144,
            34,
            window,
            2_usize as HMENU,
            instance,
            ptr::null(),
        )
    };
    if state.edit.is_null() || state.accept.is_null() || state.cancel.is_null() {
        unsafe { DestroyWindow(window) };
        return Err(super::last_error("creating review controls"));
    }
    unsafe {
        // EM_SETLIMITTEXT: raise the default edit limit before setting exact content.
        SendMessageW(state.edit, 0x00c5, display.len(), 0);
        if SetWindowTextW(state.edit, display.as_ptr()) == 0 {
            DestroyWindow(window);
            return Err(super::last_error(
                "displaying complete candidate instructions",
            ));
        }
        if GetWindowTextLengthW(state.edit) as usize != display.len() - 1 {
            DestroyWindow(window);
            bail!("review control truncated candidate text; refusing acceptance");
        }
        ShowWindow(window, SW_SHOW);
        SetForegroundWindow(window);
    }
    let mut message: MSG = unsafe { std::mem::zeroed() };
    loop {
        let result = unsafe { GetMessageW(&mut message, ptr::null_mut(), 0, 0) };
        if result == 0 {
            break;
        }
        if result < 0 {
            return Err(super::last_error("administrator review message loop"));
        }
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    if !state.accepted {
        bail!("administrator cancelled candidate acceptance");
    }
    Ok(())
}
