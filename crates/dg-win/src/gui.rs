//! Safe Win32 host for the DriverGuard window (plan §2.1, §6).
//!
//! All `unsafe` window plumbing lives here, behind a small safe API:
//! [`run_window`] creates the window (a left navigation pane, a
//! report-style list view, one context-sensitive action button, a
//! status line), runs the message loop, and delivers [`GuiEvent`]s to
//! the app's handler. Worker threads report back through
//! [`GuiHandle::post`], which queues a message and wakes the UI thread
//! — the list is only ever touched on the UI thread.
//!
//! `dg-ui` builds on this API and keeps its `#![forbid(unsafe_code)]`.

use crate::Result;
use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

// Columns are tracked through shared interior mutability so the count
// stays consistent across Gui<M> instances that are created per event.
type ColumnCount = Arc<Cell<usize>>;

/// Static description of the window to create.
#[derive(Debug, Clone)]
pub struct WindowSpec {
    pub title: String,
    /// (header, width in pixels) per list column for the initial view.
    pub columns: Vec<(String, i32)>,
    /// Label of the single action button (can be relabeled per view).
    pub backup_label: String,
    /// Names of the entries in the left navigation pane, in order.
    pub view_names: Vec<String>,
    pub width: i32,
    pub height: i32,
}

/// Events delivered to the app handler, always on the UI thread.
pub enum GuiEvent<M> {
    /// The window and its controls exist; initial data load goes here.
    Started,
    /// The user selected a different entry in the left navigation.
    ViewSelected(usize),
    /// The action button was clicked.
    BackupClicked,
    /// The user activated (double-clicked / Enter) a list row. Index is
    /// the 0-based row index in the current view.
    RowActivated(usize),
    /// A worker thread posted a message.
    Worker(M),
    /// The window is closing; the message loop is about to end.
    Closed,
}

/// Control-handle bundle for the Windows implementation.
#[derive(Clone, Copy, Default)]
struct Handles {
    main: isize,
    nav: isize,
    list: isize,
    button: isize,
    status: isize,
}

/// The app's safe handle to the running window, valid inside the event
/// handler.
pub struct Gui<M> {
    queue: Arc<Mutex<VecDeque<M>>>,
    /// Number of columns currently installed in the list view; shared
    /// with `State` via `Arc` so the count persists across per-event
    /// `Gui` instances.
    columns: ColumnCount,
    handles: Handles,
}

impl<M> Gui<M> {
    /// A clonable, `Send` handle for worker threads to post results.
    pub fn handle(&self) -> GuiHandle<M> {
        GuiHandle {
            queue: self.queue.clone(),
            hwnd: self.handles.main,
        }
    }

    /// Replace the status line text.
    pub fn set_status(&self, text: &str) {
        unsafe_set_text(self.handles.status, text);
    }

    /// Enable or disable the action button (e.g. while a backup runs).
    pub fn set_backup_enabled(&self, enabled: bool) {
        unsafe_set_enabled(self.handles.button, enabled);
    }

    /// Change the action-button label (per-view primary action, §6.2).
    pub fn set_backup_label(&self, text: &str) {
        unsafe_set_text(self.handles.button, text);
    }

    /// Replace the list-view columns and clear its contents.
    pub fn set_columns(&self, columns: &[(String, i32)]) {
        unsafe_rebuild_columns(self.handles.list, columns);
        self.columns.set(columns.len());
    }

    /// Replace all list rows.
    pub fn set_rows(&self, rows: &[Vec<String>]) {
        unsafe_set_rows(self.handles.list, self.columns.get(), rows);
    }
}

// Note: Gui<M> is deliberately !Send. It is only ever borrowed on the
// UI thread inside the event callback; worker threads use the Send
// GuiHandle<M> instead. Keeping Gui !Send avoids needing an unsafe
// Send impl and matches the architecture.

/// A `Send` handle a worker thread uses to deliver a message to the UI.
#[derive(Clone)]
pub struct GuiHandle<M> {
    queue: Arc<Mutex<VecDeque<M>>>,
    hwnd: isize,
}

impl<M> GuiHandle<M> {
    /// Queue `msg` for the UI thread and wake it.
    pub fn post(&self, msg: M) {
        if let Ok(mut q) = self.queue.lock() {
            q.push_back(msg);
        }
        unsafe_post_worker(self.hwnd);
    }
}

/// Create the window and run the message loop until it closes.
/// On non-Windows platforms this returns [`crate::WinError::NotImplemented`].
pub fn run_window<M, F>(spec: WindowSpec, handler: F) -> Result<()>
where
    M: Send + 'static,
    F: FnMut(GuiEvent<M>, &Gui<M>) + 'static,
{
    #[cfg(target_os = "windows")]
    {
        win_run(spec, handler)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (spec, handler);
        Err(crate::WinError::NotImplemented)
    }
}

/// Open a folder in the shell (Explorer on Windows). Best-effort.
pub fn open_folder_in_shell(path: &std::path::Path) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        unsafe_open_folder(path)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = path;
        Err(crate::WinError::NotImplemented)
    }
}

/// Modal-result code from [`show_message`]. Returned as a plain integer
/// so `dg-ui` can compare without importing Win32 types: the values match
/// Win32 IDOK/IDCANCEL/IDYES/IDNO (1/2/6/7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgResult {
    Ok = 1,
    Cancel = 2,
    Yes = 6,
    No = 7,
}

/// Buttons shown by [`show_message`]; mirrors the four MB_* flag sets we
/// need (OK, OKCancel, YesNo, YesNoCancel).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgButtons {
    Ok,
    OkCancel,
    YesNo,
}

/// Which icon to show next to the message (mirrors the MB_ICON* flags).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgIcon {
    Info,
    Warning,
    Error,
    Question,
}

/// Show a modal message box on the current thread's desktop. Safe
/// wrapper around `MessageBoxW` for the small set of dialogs the UI
/// needs (confirmations, completion notices, error reporting). On
/// non-Windows platforms this is a no-op that returns Ok with the
/// cancel-equivalent, so callers default to the safe path.
pub fn show_message(title: &str, text: &str, buttons: MsgButtons, icon: MsgIcon) -> MsgResult {
    #[cfg(target_os = "windows")]
    {
        unsafe_show_message(title, text, buttons, icon)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (title, text, buttons, icon);
        MsgResult::Cancel
    }
}

// ---------------------------------------------------------------------------
// Windows implementation (all unsafe lives here).
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
fn unsafe_set_text(addr: isize, text: &str) {
    use windows::core::PCWSTR;
    use windows::Win32::UI::WindowsAndMessaging::{SetWindowTextW, HWND};
    use std::ffi::c_void;
    let t: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    // Invariant: addr is a live control created by this module; the
    // wide string outlives the synchronous SetWindowTextW call.
    unsafe {
        let _ = SetWindowTextW(HWND(addr as *mut c_void), PCWSTR(t.as_ptr()));
    }
}

#[cfg(not(target_os = "windows"))]
fn unsafe_set_text(_addr: isize, _text: &str) {}

#[cfg(target_os = "windows")]
fn unsafe_set_enabled(addr: isize, enabled: bool) {
    use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
    use windows::Win32::UI::WindowsAndMessaging::HWND;
    use std::ffi::c_void;
    // Invariant: addr is the button handle created by this module.
    unsafe {
        let _ = EnableWindow(HWND(addr as *mut c_void), enabled);
    }
}

#[cfg(not(target_os = "windows"))]
fn unsafe_set_enabled(_addr: isize, _enabled: bool) {}

#[cfg(target_os = "windows")]
fn unsafe_set_rows(list_addr: isize, columns: usize, rows: &[Vec<String>]) {
    use windows::core::PWSTR;
    use windows::Win32::UI::Controls::{LVIF_TEXT, LVITEMW, LVM_DELETEALLITEMS, LVM_INSERTITEMW, LVM_SETITEMTEXTW};
    use windows::Win32::UI::WindowsAndMessaging::{HWND, LPARAM, SendMessageW, WPARAM};
    use std::ffi::c_void;
    let list = HWND(list_addr as *mut c_void);
    // Invariant: list is our list-view; LVITEMW pointers are valid for
    // each synchronous SendMessageW call, which is all the control needs.
    unsafe {
        SendMessageW(list, LVM_DELETEALLITEMS, Some(WPARAM(0)), Some(LPARAM(0)));
        for (i, row) in rows.iter().enumerate() {
            for (j, cell) in row.iter().take(columns).enumerate() {
                let mut text: Vec<u16> = cell.encode_utf16().chain(std::iter::once(0)).collect();
                let item = LVITEMW {
                    mask: LVIF_TEXT,
                    iItem: i as i32,
                    iSubItem: j as i32,
                    pszText: PWSTR(text.as_mut_ptr()),
                    ..Default::default()
                };
                if j == 0 {
                    SendMessageW(
                        list,
                        LVM_INSERTITEMW,
                        Some(WPARAM(0)),
                        Some(LPARAM(&item as *const LVITEMW as isize)),
                    );
                } else {
                    SendMessageW(
                        list,
                        LVM_SETITEMTEXTW,
                        Some(WPARAM(i)),
                        Some(LPARAM(&item as *const LVITEMW as isize)),
                    );
                }
            }
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn unsafe_set_rows(_list: isize, _columns: usize, _rows: &[Vec<String>]) {}

#[cfg(target_os = "windows")]
fn unsafe_rebuild_columns(list_addr: isize, columns: &[(String, i32)]) {
    use windows::core::PWSTR;
    use windows::Win32::UI::Controls::{
        LVCF_SUBITEM, LVCF_TEXT, LVCF_WIDTH, LVCOLUMNW, LVM_DELETEALLITEMS, LVM_DELETECOLUMN,
        LVM_GETCOLUMNCOUNT, LVM_INSERTCOLUMNW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{HWND, LPARAM, SendMessageW, WPARAM};
    use std::ffi::c_void;
    let list = HWND(list_addr as *mut c_void);
    // Invariant: list is our list-view. Clearing items and rebuilding
    // columns is a supported sequence for LVS_REPORT controls.
    unsafe {
        SendMessageW(list, LVM_DELETEALLITEMS, Some(WPARAM(0)), Some(LPARAM(0)));
        let existing = SendMessageW(list, LVM_GETCOLUMNCOUNT, Some(WPARAM(0)), Some(LPARAM(0)));
        let existing = existing.0 as i32;
        for idx in (0..existing).rev() {
            SendMessageW(list, LVM_DELETECOLUMN, Some(WPARAM(idx as usize)), Some(LPARAM(0)));
        }
        for (i, (title, width)) in columns.iter().enumerate() {
            let mut t: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
            let col = LVCOLUMNW {
                mask: LVCF_WIDTH | LVCF_TEXT | LVCF_SUBITEM,
                cx: *width,
                pszText: PWSTR(t.as_mut_ptr()),
                iSubItem: i as i32,
                ..Default::default()
            };
            SendMessageW(
                list,
                LVM_INSERTCOLUMNW,
                Some(WPARAM(i)),
                Some(LPARAM(&col as *const LVCOLUMNW as isize)),
            );
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn unsafe_rebuild_columns(_list: isize, _columns: &[(String, i32)]) {}

#[cfg(target_os = "windows")]
fn unsafe_post_worker(hwnd_addr: isize) {
    use windows::Win32::Foundation::LPARAM;
    use windows::Win32::UI::WindowsAndMessaging::{HWND, PostMessageW, WPARAM};
    use std::ffi::c_void;
    const WM_WORKER: u32 = 0x8001;
    // Invariant: PostMessageW is thread-safe; if the window is gone the
    // call simply fails.
    unsafe {
        let _ = PostMessageW(Some(HWND(hwnd_addr as *mut c_void)), WM_WORKER, WPARAM(0), LPARAM(0));
    }
}

#[cfg(not(target_os = "windows"))]
fn unsafe_post_worker(_hwnd: isize) {}

#[cfg(target_os = "windows")]
fn unsafe_open_folder(path: &std::path::Path) -> Result<()> {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use std::os::windows::ffi::OsStrExt;
    let file: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let verb: Vec<u16> = "explore".encode_utf16().chain(std::iter::once(0)).collect();
    // Best-effort affordance. ShellExecuteW returns a Result with a
    // fake HINSTANCE on success; we don't inspect it and we map any
    // error to Ok(()) anyway — if opening fails, the Settings view
    // still shows the path as text.
    unsafe {
        let _ = ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn unsafe_open_folder(_path: &std::path::Path) -> Result<()> {
    Err(crate::WinError::NotImplemented)
}

#[cfg(target_os = "windows")]
fn unsafe_show_message(title: &str, text: &str, buttons: MsgButtons, icon: MsgIcon) -> MsgResult {
    use windows::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONQUESTION, MB_ICONWARNING, MB_OK,
        MB_OKCANCEL, MB_YESNO, MESSAGEBOX_RESULT,
    };
    let flags = match buttons {
        MsgButtons::Ok => MB_OK,
        MsgButtons::OkCancel => MB_OKCANCEL,
        MsgButtons::YesNo => MB_YESNO,
    } | match icon {
        MsgIcon::Info => MB_ICONINFORMATION,
        MsgIcon::Warning => MB_ICONWARNING,
        MsgIcon::Error => MB_ICONERROR,
        MsgIcon::Question => MB_ICONQUESTION,
    };
    let t: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let m: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    use windows::core::PCWSTR;
    // SAFETY: MessageBoxW with a null HWND owner shows a task-modal
    // dialog on the current desktop; our wide strings are valid for
    // the duration of the synchronous call. No input state is mutated.
    let res: MESSAGEBOX_RESULT = unsafe {
        MessageBoxW(None, PCWSTR(m.as_ptr()), PCWSTR(t.as_ptr()), flags)
    };
    match res.0 as i32 {
        1 => MsgResult::Ok,
        2 => MsgResult::Cancel,
        6 => MsgResult::Yes,
        7 => MsgResult::No,
        _ => MsgResult::Cancel,
    }
}

#[cfg(not(target_os = "windows"))]
fn unsafe_show_message(_title: &str, _text: &str, _buttons: MsgButtons, _icon: MsgIcon) -> MsgResult {
    MsgResult::Cancel
}

#[cfg(target_os = "windows")]
fn win_run<M, F>(spec: WindowSpec, handler: F) -> Result<()>
where
    M: Send + 'static,
    F: FnMut(GuiEvent<M>, &Gui<M>) + 'static,
{
    use std::collections::VecDeque;
    use std::ffi::c_void;
    use std::sync::{Arc, Mutex};
    use windows::core::{w, PCWSTR, PWSTR};
    use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        GetStockObject, GetSysColorBrush, UpdateWindow, COLOR_WINDOW, DEFAULT_GUI_FONT,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Controls::{
        InitCommonControlsEx, ICC_LISTVIEW_CLASSES, INITCOMMONCONTROLSEX, LVCF_SUBITEM, LVCF_TEXT,
        LVCF_WIDTH, LVCOLUMNW, LVM_INSERTCOLUMNW, LVM_SETEXTENDEDLISTVIEWSTYLE,
        LVS_EX_FULLROWSELECT, LVS_EX_GRIDLINES, WC_LISTVIEWW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, GetWindowLongPtrW,
        LoadCursorW, MoveWindow, PostQuitMessage, RegisterClassW, SendMessageW, SetWindowLongPtrW,
        ShowWindow, TranslateMessage, BS_PUSHBUTTON, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW,
        CW_USEDEFAULT, GWLP_USERDATA, HMENU, IDC_ARROW, LBN_SELCHANGE, LBS_NOTIFY, LB_ADDSTRING,
        LB_GETCURSEL, LB_SETCURSEL, MSG, NM_DBLCLK, SW_SHOW, WINDOW_EX_STYLE, WINDOW_STYLE,
        WM_COMMAND, WM_CREATE, WM_DESTROY, WM_NCCREATE, WM_NOTIFY, WM_SETFONT, WM_SIZE, WNDCLASSW,
        WS_BORDER, WS_CHILD, WS_OVERLAPPEDWINDOW, WS_VSCROLL, WS_VISIBLE,
    };

    const WM_WORKER: u32 = 0x8001;
    const ID_BACKUP: usize = 1001;
    const ID_NAV: usize = 1002;
    const ID_LIST: usize = 1003;
    const NAV_WIDTH: i32 = 160;
    const GUTTER: i32 = 10;
    const BUTTON_H: i32 = 32;
    const STATUS_H: i32 = 20;
    const BUTTON_W: i32 = 200;

    type Handler<M> = Box<dyn FnMut(GuiEvent<M>, &Gui<M>) + 'static>;

    struct State<M> {
        handler: Handler<M>,
        queue: Arc<Mutex<VecDeque<M>>>,
        spec: WindowSpec,
        handles: Handles,
        hinstance: isize,
        columns: ColumnCount,
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn hwnd(addr: isize) -> HWND {
        HWND(addr as *mut c_void)
    }

    fn api(e: windows::core::Error) -> crate::WinError {
        crate::WinError::Api {
            code: e.code().0 as u32,
            message: e.message(),
        }
    }

    fn make_gui<M>(state: &State<M>) -> Gui<M> {
        Gui {
            queue: state.queue.clone(),
            columns: state.columns.clone(),
            handles: state.handles,
        }
    }

    fn fire<M>(state: &mut State<M>, event: GuiEvent<M>) {
        let gui = make_gui(state);
        (state.handler)(event, &gui);
    }

    fn layout(state: &State<M>, width: i32, height: i32) {
        let h = state.handles;
        let content_x = NAV_WIDTH + GUTTER;
        let content_w = (width - content_x - GUTTER).max(0);
        let list_y = GUTTER + BUTTON_H + GUTTER;
        let list_h = (height - list_y - STATUS_H - GUTTER).max(0);
        // Invariant: all handles are live children of this window; rects
        // fit inside the client area for any width/height ≥ 0.
        unsafe {
            let _ = MoveWindow(
                hwnd(h.nav),
                GUTTER,
                GUTTER,
                NAV_WIDTH,
                (height - 2 * GUTTER).max(0),
                true,
            );
            let _ = MoveWindow(
                hwnd(h.button),
                content_x,
                GUTTER,
                BUTTON_W.min(content_w),
                BUTTON_H,
                true,
            );
            let _ = MoveWindow(hwnd(h.list), content_x, list_y, content_w, list_h, true);
            let _ = MoveWindow(
                hwnd(h.status),
                content_x,
                (height - STATUS_H - GUTTER).max(0),
                content_w,
                STATUS_H,
                true,
            );
        }
    }

    unsafe extern "system" fn wnd_proc<M>(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT
    where
        M: Send + 'static,
    {
        if msg == WM_NCCREATE {
            let cs = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            }
        }
        let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
        if ptr == 0 {
            return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
        }
        let state = unsafe { &mut *(ptr as *mut State<M>) };

        match msg {
            WM_CREATE => {
                state.handles.main = hwnd.0 as isize;
                create_children(state, hwnd);
                state.columns.set(state.spec.columns.len());
                layout(state, state.spec.width, state.spec.height);
                unsafe {
                    let _ = SendMessageW(
                        hwnd(state.handles.nav),
                        LB_SETCURSEL,
                        Some(WPARAM(0)),
                        Some(LPARAM(0)),
                    );
                }
                fire(state, GuiEvent::Started);
                LRESULT(0)
            }
            WM_SIZE => {
                let width = (lparam.0 & 0xFFFF) as i32;
                let height = ((lparam.0 >> 16) & 0xFFFF) as i32;
                layout(state, width, height);
                LRESULT(0)
            }
            WM_COMMAND => {
                let id = wparam.0 & 0xFFFF;
                let notify = (wparam.0 >> 16) & 0xFFFF;
                if id == ID_BACKUP && notify == 0 {
                    fire(state, GuiEvent::BackupClicked);
                } else if id == ID_NAV && notify as u32 == LBN_SELCHANGE as u32 {
                    let lres = unsafe {
                        SendMessageW(
                            hwnd(state.handles.nav),
                            LB_GETCURSEL,
                            None,
                            Some(LPARAM(0)),
                        )
                    };
                    // LB_GETCURSEL returns the 0-based index, or LB_ERR (-1).
                    let idx = lres.0 as i32;
                    if idx >= 0 && (idx as usize) < state.spec.view_names.len() {
                        fire(state, GuiEvent::ViewSelected(idx as usize));
                    }
                }
                LRESULT(0)
            }
            WM_NOTIFY => {
                // List-view double-click / Enter activation.
                // WM_NOTIFY's lparam points to an NMHDR; for list-view
                // item activation it's actually an NMITEMACTIVE whose
                // first fields overlap NMHDR. We just need the idFrom
                // (control id) and code (notification) from the header,
                // followed by the iItem index at offset 12 on 32-bit /
                // 24 on 64-bit. To stay architecture-safe we read iItem
                // via offset_of on the windows-rs NMITEMACTIVE when
                // available, but the simple "read at the offset of the
                // first i32 after the hdr" approach is what every SDK
                // sample uses. We use the publicly-recommended offset:
                // NMHDR is { hwnd: HWND(8), idFrom: usize(8), code: u32(4) }
                // → padded to 24 bytes on 64-bit before iItem.
                use windows::Win32::UI::Controls::LVN_ITEMACTIVATE;
                #[repr(C)]
                struct Nmhdr { hwnd: isize, id: usize, code: u32 }
                if !lparam.0.is_null() {
                    let hdr = unsafe { *(lparam.0 as *const Nmhdr) };
                    if hdr.id == ID_LIST && hdr.code == NM_DBLCLK as u32 {
                        // Read selected index via LVM_GETNEXTITEM.
                        let sel = unsafe {
                            SendMessageW(
                                hwnd(state.handles.list),
                                0x100B, // LVM_GETNEXTITEM
                                Some(WPARAM((-1i32) as usize)),
                                Some(LPARAM(2)), // LVNI_SELECTED
                            )
                        };
                        let idx = sel.0 as i32;
                        if idx >= 0 {
                            fire(state, GuiEvent::RowActivated(idx as usize));
                        }
                    }
                    // LVN_ITEMACTIVATE is also generated by keyboard.
                    if hdr.id == ID_LIST && hdr.code == LVN_ITEMACTIVATE as u32 {
                        let offset = if std::mem::size_of::<usize>() == 8 { 24 } else { 12 };
                        let item_ptr = (lparam.0 as *const u8).wrapping_add(offset) as *const i32;
                        let idx = unsafe { *item_ptr };
                        if idx >= 0 {
                            fire(state, GuiEvent::RowActivated(idx as usize));
                        }
                    }
                }
                LRESULT(0)
            }
            WM_WORKER => {
                let drained: Vec<M> = match state.queue.lock() {
                    Ok(mut q) => q.drain(..).collect(),
                    Err(_) => Vec::new(),
                };
                for m in drained {
                    fire(state, GuiEvent::Worker(m));
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                fire(state, GuiEvent::Closed);
                unsafe {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    drop(Box::from_raw(ptr as *mut State<M>));
                    PostQuitMessage(0);
                }
                LRESULT(0)
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    fn create_children<M>(state: &mut State<M>, parent: HWND) {
        let hinstance = state.hinstance;
        // Invariant: standard child-control creation on the UI thread
        // during WM_CREATE; wide strings outlive each call.
        unsafe {
            let font = GetStockObject(DEFAULT_GUI_FONT);
            let hinst = Some(HINSTANCE(hinstance as *mut c_void));

            let nav = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("LISTBOX"),
                PCWSTR::null(),
                WS_CHILD | WS_VISIBLE | WS_VSCROLL | WINDOW_STYLE(LBS_NOTIFY as u32),
                0,
                0,
                NAV_WIDTH,
                100,
                Some(parent),
                Some(HMENU(ID_NAV as *mut c_void)),
                hinst,
                None,
            )
            .unwrap_or_default();
            state.handles.nav = nav.0 as isize;
            for name in &state.spec.view_names {
                let s = wide(name);
                let _ = SendMessageW(
                    nav,
                    LB_ADDSTRING,
                    Some(WPARAM(0)),
                    Some(LPARAM(s.as_ptr() as isize)),
                );
            }

            let label = wide(&state.spec.backup_label);
            let button = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("BUTTON"),
                PCWSTR(label.as_ptr()),
                WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_PUSHBUTTON as u32),
                NAV_WIDTH + GUTTER,
                GUTTER,
                BUTTON_W,
                BUTTON_H,
                Some(parent),
                Some(HMENU(ID_BACKUP as *mut c_void)),
                hinst,
                None,
            )
            .unwrap_or_default();
            state.handles.button = button.0 as isize;

            let list = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                WC_LISTVIEWW,
                PCWSTR(std::ptr::null()),
                WS_CHILD
                    | WS_VISIBLE
                    | WS_BORDER
                    | WINDOW_STYLE(
                        windows::Win32::UI::Controls::LVS_REPORT
                            | windows::Win32::UI::Controls::LVS_SHOWSELALWAYS,
                    ),
                NAV_WIDTH + GUTTER,
                52,
                400,
                300,
                Some(parent),
                Some(HMENU(ID_LIST as *mut c_void)),
                hinst,
                None,
            )
            .unwrap_or_default();
            state.handles.list = list.0 as isize;
            SendMessageW(
                list,
                LVM_SETEXTENDEDLISTVIEWSTYLE,
                Some(WPARAM(0)),
                Some(LPARAM((LVS_EX_FULLROWSELECT | LVS_EX_GRIDLINES) as isize)),
            );
            for (i, (title, width)) in state.spec.columns.iter().enumerate() {
                let mut t = wide(title);
                let col = LVCOLUMNW {
                    mask: LVCF_WIDTH | LVCF_TEXT | LVCF_SUBITEM,
                    cx: *width,
                    pszText: PWSTR(t.as_mut_ptr()),
                    iSubItem: i as i32,
                    ..Default::default()
                };
                SendMessageW(
                    list,
                    LVM_INSERTCOLUMNW,
                    Some(WPARAM(i)),
                    Some(LPARAM(&col as *const LVCOLUMNW as isize)),
                );
            }

            let status = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                PCWSTR::null(),
                WS_CHILD | WS_VISIBLE,
                NAV_WIDTH + GUTTER,
                10,
                400,
                STATUS_H,
                Some(parent),
                None,
                hinst,
                None,
            )
            .unwrap_or_default();
            state.handles.status = status.0 as isize;

            for child in [nav, button, list, status] {
                SendMessageW(
                    child,
                    WM_SETFONT,
                    Some(WPARAM(font.0 as usize)),
                    Some(LPARAM(1)),
                );
            }
        }
    }

    let queue: Arc<Mutex<VecDeque<M>>> = Arc::new(Mutex::new(VecDeque::new()));
    let columns: ColumnCount = Arc::new(Cell::new(spec.columns.len()));
    let state = Box::new(State {
        handler: Box::new(handler),
        queue,
        spec,
        handles: Handles::default(),
        hinstance: 0,
        columns,
    });
    let ptr = Box::into_raw(state);

    unsafe {
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES,
        };
        let _ = InitCommonControlsEx(&icc);

        let hinstance = GetModuleHandleW(None).map_err(api)?;
        (*ptr).hinstance = hinstance.0 as isize;

        let class_name = w!("DriverGuardMainWindow");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc::<M>),
            hInstance: HINSTANCE(hinstance.0),
            hCursor: LoadCursorW(None, IDC_ARROW).map_err(api)?,
            hbrBackground: GetSysColorBrush(COLOR_WINDOW),
            lpszClassName: class_name,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            drop(Box::from_raw(ptr));
            return Err(crate::WinError::Api {
                code: 0,
                message: "RegisterClassW failed".to_owned(),
            });
        }

        let spec_ref = &(*ptr).spec;
        let title = wide(&spec_ref.title);
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            spec_ref.width,
            spec_ref.height,
            None,
            None,
            Some(HINSTANCE(hinstance.0)),
            Some(ptr as *const c_void),
        ) {
            Ok(h) => h,
            Err(e) => {
                drop(Box::from_raw(ptr));
                return Err(api(e));
            }
        };

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);

            let mut msg = MSG::default();
            loop {
                let r = GetMessageW(&mut msg, None, 0, 0);
                // GetMessageW returns BOOL(0) on WM_QUIT and BOOL(-1) on error.
                let val = r.0 as i32;
                if val == 0 || val == -1 {
                    break;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
    }
    Ok(())
}
