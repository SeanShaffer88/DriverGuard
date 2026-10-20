//! Safe Win32 host for the DriverGuard window (plan §2.1, §6).
//!
//! All `unsafe` window plumbing lives here, behind a small safe API:
//! [`run_window`] creates the window (a report-style list view, one
//! action button, a status line), runs the message loop, and delivers
//! [`GuiEvent`]s to the app's handler. Worker threads report back
//! through [`GuiHandle::post`], which queues a message and wakes the UI
//! thread — the list is only ever touched on the UI thread.
//!
//! `dg-ui` builds on this API and keeps its `#![forbid(unsafe_code)]`.

use crate::Result;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Static description of the window to create.
#[derive(Debug, Clone)]
pub struct WindowSpec {
    pub title: String,
    /// (header, width in pixels) per list column.
    pub columns: Vec<(String, i32)>,
    /// Label of the single action button.
    pub backup_label: String,
    pub width: i32,
    pub height: i32,
}

/// Events delivered to the app handler, always on the UI thread.
pub enum GuiEvent<M> {
    /// The window and its controls exist; initial data load goes here.
    Started,
    /// The action button was clicked.
    BackupClicked,
    /// A worker thread posted a message.
    Worker(M),
    /// The window is closing; the message loop is about to end.
    Closed,
}

/// The app's safe handle to the running window, valid inside the event
/// handler.
pub struct Gui<M> {
    queue: Arc<Mutex<VecDeque<M>>>,
    /// Read by the Windows backend when filling the list; unused in the
    /// non-Windows stub build.
    #[cfg_attr(not(windows), allow(dead_code))]
    columns: usize,
    #[cfg(windows)]
    handles: imp::Handles,
}

impl<M> Gui<M> {
    /// A clonable, `Send` handle for worker threads to post results.
    pub fn handle(&self) -> GuiHandle<M> {
        GuiHandle {
            queue: self.queue.clone(),
            #[cfg(windows)]
            hwnd: self.handles.main,
        }
    }

    /// Replace the status line text.
    pub fn set_status(&self, text: &str) {
        #[cfg(windows)]
        imp::set_text(self.handles.status, text);
        #[cfg(not(windows))]
        let _ = text;
    }

    /// Enable or disable the action button (e.g. while a backup runs).
    pub fn set_backup_enabled(&self, enabled: bool) {
        #[cfg(windows)]
        imp::set_enabled(self.handles.button, enabled);
        #[cfg(not(windows))]
        let _ = enabled;
    }

    /// Replace all list rows. Each row should have one cell per column;
    /// missing cells render empty, extra cells are ignored.
    pub fn set_rows(&self, rows: &[Vec<String>]) {
        #[cfg(windows)]
        imp::set_rows(self.handles.list, self.columns, rows);
        #[cfg(not(windows))]
        let _ = rows;
    }
}

/// A `Send` handle a worker thread uses to deliver a message to the UI.
#[derive(Clone)]
pub struct GuiHandle<M> {
    queue: Arc<Mutex<VecDeque<M>>>,
    #[cfg(windows)]
    hwnd: isize,
}

impl<M> GuiHandle<M> {
    /// Queue `msg` for the UI thread and wake it. If the window is
    /// already gone, the message is silently dropped.
    pub fn post(&self, msg: M) {
        if let Ok(mut q) = self.queue.lock() {
            q.push_back(msg);
        }
        #[cfg(windows)]
        imp::post_worker_message(self.hwnd);
    }
}

/// Create the window described by `spec` and run the message loop until
/// it closes, delivering events to `handler`.
///
/// On non-Windows platforms this returns
/// [`WinError::NotImplemented`]; the CLI modes remain available there.
pub fn run_window<M, F>(spec: WindowSpec, handler: F) -> Result<()>
where
    M: Send + 'static,
    F: FnMut(GuiEvent<M>, &Gui<M>) + 'static,
{
    #[cfg(windows)]
    {
        imp::run(spec, handler)
    }
    #[cfg(not(windows))]
    {
        let _ = spec;
        let _ = handler;
        Err(crate::WinError::NotImplemented)
    }
}

#[cfg(windows)]
mod imp {
    use super::{Gui, GuiEvent, WindowSpec};
    use crate::{Result, WinError};
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
        LVCF_WIDTH, LVCOLUMNW, LVIF_TEXT, LVITEMW, LVM_DELETEALLITEMS, LVM_INSERTCOLUMNW,
        LVM_INSERTITEMW, LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETITEMTEXTW, LVS_EX_FULLROWSELECT,
        LVS_EX_GRIDLINES, WC_LISTVIEWW,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, GetWindowLongPtrW,
        LoadCursorW, MoveWindow, PostMessageW, PostQuitMessage, RegisterClassW, SendMessageW,
        SetWindowLongPtrW, SetWindowTextW, ShowWindow, TranslateMessage, BS_PUSHBUTTON,
        CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, GWLP_USERDATA, HMENU, IDC_ARROW, MSG,
        SW_SHOW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_COMMAND, WM_CREATE, WM_DESTROY, WM_NCCREATE,
        WM_SETFONT, WM_SIZE, WNDCLASSW, WS_BORDER, WS_CHILD, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
    };

    /// Custom message: a worker posted to the queue.
    const WM_WORKER: u32 = 0x8001; // WM_APP + 1
    const ID_BACKUP: usize = 1001;

    #[derive(Clone, Copy, Default)]
    pub(super) struct Handles {
        pub main: isize,
        pub list: isize,
        pub button: isize,
        pub status: isize,
    }

    type Handler<M> = Box<dyn FnMut(GuiEvent<M>, &Gui<M>) + 'static>;

    struct State<M> {
        handler: Handler<M>,
        queue: Arc<Mutex<VecDeque<M>>>,
        spec: WindowSpec,
        handles: Handles,
        hinstance: isize,
    }

    fn api(e: windows::core::Error) -> WinError {
        WinError::Api {
            code: e.code().0 as u32,
            message: e.message(),
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn hwnd(addr: isize) -> HWND {
        HWND(addr as *mut c_void)
    }

    pub(super) fn set_text(addr: isize, text: &str) {
        let t = wide(text);
        // Invariant: addr is a live control handle created by this
        // module; the wide string outlives the call.
        unsafe {
            let _ = SetWindowTextW(hwnd(addr), PCWSTR(t.as_ptr()));
        }
    }

    pub(super) fn set_enabled(addr: isize, enabled: bool) {
        // Invariant: addr is the button handle created by this module.
        unsafe {
            let _ = EnableWindow(hwnd(addr), enabled);
        }
    }

    pub(super) fn set_rows(list_addr: isize, columns: usize, rows: &[Vec<String>]) {
        let list = hwnd(list_addr);
        // Invariant: list is the list-view control created by this
        // module; LVITEMW pointers are valid for the duration of each
        // SendMessageW call, which is all the control requires.
        unsafe {
            SendMessageW(list, LVM_DELETEALLITEMS, Some(WPARAM(0)), Some(LPARAM(0)));
            for (i, row) in rows.iter().enumerate() {
                for (j, cell) in row.iter().take(columns).enumerate() {
                    let mut text = wide(cell);
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

    pub(super) fn post_worker_message(hwnd_addr: isize) {
        // Invariant: PostMessageW is thread-safe; if the window is gone
        // the call simply fails and the queued message is dropped.
        unsafe {
            let _ = PostMessageW(Some(hwnd(hwnd_addr)), WM_WORKER, WPARAM(0), LPARAM(0));
        }
    }

    pub(super) fn run<M, F>(spec: WindowSpec, handler: F) -> Result<()>
    where
        M: Send + 'static,
        F: FnMut(GuiEvent<M>, &Gui<M>) + 'static,
    {
        let queue: Arc<Mutex<VecDeque<M>>> = Arc::new(Mutex::new(VecDeque::new()));
        let state = Box::new(State {
            handler: Box::new(handler),
            queue,
            spec,
            handles: Handles::default(),
            hinstance: 0,
        });
        let ptr = Box::into_raw(state);

        // Invariant: every unsafe call below follows the standard Win32
        // window-creation sequence. The state box is owned by `ptr`
        // until CreateWindowExW hands it to the window via
        // lpCreateParams; from then on WM_DESTROY owns dropping it.
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
                return Err(WinError::Api {
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
                if r.0 == 0 || r.0 == -1 {
                    break; // WM_QUIT or error
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        Ok(())
    }

    fn make_gui<M>(state: &State<M>) -> Gui<M> {
        Gui {
            queue: state.queue.clone(),
            columns: state.spec.columns.len(),
            handles: state.handles,
        }
    }

    fn fire<M>(state: &mut State<M>, event: GuiEvent<M>) {
        let gui = make_gui(state);
        (state.handler)(event, &gui);
    }

    fn layout(state: &State<impl Sized>, width: i32, height: i32) {
        let h = state.handles;
        // Invariant: all four handles are live controls of this window;
        // MoveWindow with these rectangles keeps children inside the
        // client area for any width/height ≥ 0.
        unsafe {
            let _ = MoveWindow(hwnd(h.button), 10, 10, 200, 32, true);
            let _ = MoveWindow(
                hwnd(h.list),
                10,
                52,
                (width - 20).max(0),
                (height - 86).max(0),
                true,
            );
            let _ = MoveWindow(
                hwnd(h.status),
                10,
                (height - 24).max(0),
                (width - 20).max(0),
                18,
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
        // Invariant: on WM_NCCREATE, lparam points to the CREATESTRUCTW
        // passed by CreateWindowExW and its lpCreateParams is our state
        // box; the box stays alive until WM_DESTROY drops it.
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
                layout(state, state.spec.width, state.spec.height);
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
                // Invariant: ptr is the state box handed to the window at
                // creation; reclaiming and dropping it here ends its
                // lifetime exactly once.
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
            let label = wide(&state.spec.backup_label);
            let hinst = Some(HINSTANCE(hinstance as *mut c_void));
            let button = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("BUTTON"),
                PCWSTR(label.as_ptr()),
                WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_PUSHBUTTON as u32),
                10,
                10,
                200,
                32,
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
                10,
                52,
                400,
                300,
                Some(parent),
                None,
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
                PCWSTR(std::ptr::null()),
                WS_CHILD | WS_VISIBLE,
                10,
                10,
                400,
                18,
                Some(parent),
                None,
                hinst,
                None,
            )
            .unwrap_or_default();
            state.handles.status = status.0 as isize;

            for child in [button, list, status] {
                SendMessageW(
                    child,
                    WM_SETFONT,
                    Some(WPARAM(font.0 as usize)),
                    Some(LPARAM(1)),
                );
            }
        }
    }
}
