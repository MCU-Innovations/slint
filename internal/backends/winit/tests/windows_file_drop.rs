// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

#[cfg(target_os = "windows")]
fn main() {
    use std::os::windows::ffi::OsStrExt;

    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use slint::ComponentHandle;
    use slint::winit_030::WinitWindowAccessor;
    use windows::Win32::Foundation::{HWND, POINT, POINTL};
    use windows::Win32::Graphics::Gdi::ClientToScreen;
    use windows::Win32::System::Com::IDataObject;
    use windows::Win32::System::Ole::{
        DROPEFFECT_COPY, DROPEFFECT_MOVE, DROPEFFECT_NONE, IDropTarget,
    };
    use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
    use windows::Win32::UI::Shell::{BHID_DataObject, IShellItem, SHCreateItemFromParsingName};
    use windows::Win32::UI::WindowsAndMessaging::GetPropW;
    use windows::core::{Interface, PCWSTR, w};

    slint::slint! {
        export component App inherits Window {
            width: 200px;
            height: 100px;
            in-out property <bool> accept-files: true;
            out property <bool> hovering: target.has-drag;
            out property <string> dropped-path;
            out property <int> drop-count;
            pure callback path-of(data-transfer) -> string;
            target := DropArea {
                x: 20px; y: 20px; width: 80px; height: 60px;
                can-drop(event) => root.accept-files && root.path-of(event.data) != ""
                    ? DragAction.copy : DragAction.none;
                dropped(event) => {
                    root.dropped-path = root.path-of(event.data);
                    root.drop-count += 1;
                    return DragAction.copy;
                }
            }
        }
    }

    // Exercise screen-to-client conversion with a fractional scale factor,
    // and registration cleanup while an application retains the native window.
    unsafe {
        std::env::set_var("SLINT_SCALE_FACTOR", "1.5");
    }
    let backend =
        i_slint_backend_winit::Backend::builder().with_renderer_name("software").build().unwrap();
    slint::platform::set_platform(Box::new(backend)).unwrap();

    slint::spawn_local(async move {
        let app = App::new().unwrap();
        app.on_path_of(|data| {
            data.file_paths()
                .ok()
                .and_then(|mut paths| paths.next().map(|p| p.to_string_lossy().into_owned()))
                .unwrap_or_default()
                .into()
        });
        let native = app.window().winit_window().await.unwrap();
        let RawWindowHandle::Win32(handle) = native.window_handle().unwrap().as_raw() else {
            panic!("expected a Win32 window");
        };
        let hwnd = HWND(handle.hwnd.get() as *mut _);
        let target_raw = unsafe { GetPropW(hwnd, w!("OleDropTargetInterface")) }.0;
        // Use the registered COM vtable, so x86 tests catch an incorrect POINTL ABI.
        let target = unsafe { IDropTarget::from_raw_borrowed(&target_raw) }.unwrap().clone();
        let file = std::env::temp_dir()
            .join(format!("slint-drop-{}-test firmware-\u{03a9}.bin", std::process::id()));
        std::fs::write(&file, [0u8; 8]).unwrap();
        let file_wide: Vec<u16> = file.as_os_str().encode_wide().chain(Some(0)).collect();
        let item: IShellItem =
            unsafe { SHCreateItemFromParsingName(PCWSTR(file_wide.as_ptr()), None) }.unwrap();
        let data: IDataObject = unsafe { item.BindToHandler(None, &BHID_DataObject) }.unwrap();
        let at = |x: f32, y: f32| {
            let scale = app.window().scale_factor();
            let mut point = POINT { x: (x * scale) as i32, y: (y * scale) as i32 };
            assert!(unsafe { ClientToScreen(hwnd, &mut point) }.as_bool());
            POINTL { x: point.x, y: point.y }
        };
        let inside = at(50., 50.);
        let outside = at(150., 50.);
        let keys = MODIFIERKEYS_FLAGS(0);
        let mut effect = DROPEFFECT_COPY | DROPEFFECT_MOVE;
        unsafe { target.DragEnter(&data, keys, inside, &mut effect) }.unwrap();
        assert_eq!(effect, DROPEFFECT_COPY);
        assert!(app.get_hovering());
        effect = DROPEFFECT_COPY;
        unsafe { target.DragOver(keys, outside, &mut effect) }.unwrap();
        assert_eq!(effect, DROPEFFECT_NONE);
        assert!(!app.get_hovering());
        effect = DROPEFFECT_COPY;
        unsafe { target.DragOver(keys, inside, &mut effect) }.unwrap();
        assert_eq!(effect, DROPEFFECT_COPY);
        assert!(app.get_hovering());
        effect = DROPEFFECT_COPY;
        unsafe { target.Drop(&data, keys, inside, &mut effect) }.unwrap();
        assert_eq!(effect, DROPEFFECT_COPY);
        assert_eq!(app.get_drop_count(), 1);
        assert_eq!(app.get_dropped_path().as_str(), file.to_str().unwrap());
        assert!(!app.get_hovering());

        app.set_accept_files(false);
        effect = DROPEFFECT_COPY;
        unsafe { target.DragEnter(&data, keys, inside, &mut effect) }.unwrap();
        assert_eq!(effect, DROPEFFECT_NONE);
        unsafe { target.DragLeave() }.unwrap();
        assert!(!app.get_hovering());
        assert_eq!(app.get_drop_count(), 1);

        app.set_accept_files(true);
        effect = DROPEFFECT_MOVE;
        unsafe { target.DragEnter(&data, keys, inside, &mut effect) }.unwrap();
        assert_eq!(effect, DROPEFFECT_NONE, "a copy-only target must not claim a move");
        unsafe { target.DragLeave() }.unwrap();
        effect = DROPEFFECT_COPY;
        unsafe { target.DragEnter(None, keys, inside, &mut effect) }.unwrap();
        assert_eq!(effect, DROPEFFECT_NONE);
        unsafe { target.DragLeave() }.unwrap();

        effect = DROPEFFECT_COPY;
        unsafe { target.DragEnter(&data, keys, inside, &mut effect) }.unwrap();
        assert!(app.get_hovering());
        unsafe { target.DragLeave() }.unwrap();
        assert!(!app.get_hovering());
        assert_eq!(app.get_drop_count(), 1);
        assert!(file.is_file(), "copy negotiation must preserve the source file");
        drop(data);
        drop(item);
        std::fs::remove_file(&file).unwrap();
        drop(target);
        drop(app);
        assert!(unsafe { GetPropW(hwnd, w!("OleDropTargetInterface")) }.0.is_null());
        drop(native);

        let recreated = App::new().unwrap();
        let native = recreated.window().winit_window().await.unwrap();
        let RawWindowHandle::Win32(handle) = native.window_handle().unwrap().as_raw() else {
            panic!("expected a Win32 window");
        };
        let hwnd = HWND(handle.hwnd.get() as *mut _);
        assert!(!unsafe { GetPropW(hwnd, w!("OleDropTargetInterface")) }.0.is_null());
        drop(recreated);
        assert!(unsafe { GetPropW(hwnd, w!("OleDropTargetInterface")) }.0.is_null());
        drop(native);
        slint::quit_event_loop().unwrap();
    })
    .unwrap();
    slint::run_event_loop_until_quit().unwrap();
}

#[cfg(not(target_os = "windows"))]
fn main() {}
