// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::rc::Weak;

use i_slint_core::DataTransfer;
use i_slint_core::api::LogicalPosition;
use i_slint_core::input::{BackendDragEvent, KeyboardModifiers};
use i_slint_core::items::{AllowedDragActions, DragAction, DropEvent, compute_proposed_action};
use i_slint_core::platform::PlatformError;
use i_slint_core::window::{WindowAdapter, WindowInner};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::{E_POINTER, HWND, POINT, POINTL};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::System::Com::{
    DVASPECT_CONTENT, FORMATETC, IDataObject, STGMEDIUM, TYMED_HGLOBAL,
};
use windows::Win32::System::Ole::{
    CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, DROPEFFECT_NONE,
    IDropTarget, IDropTarget_Impl, OleInitialize, OleUninitialize, RegisterDragDrop,
    ReleaseStgMedium, RevokeDragDrop,
};
use windows::Win32::System::SystemServices::{MK_CONTROL, MK_SHIFT, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::core::{Ref, Result, implement};

use crate::WinitWindowAdapter;

struct OleApartment;

impl Drop for OleApartment {
    fn drop(&mut self) {
        unsafe { OleUninitialize() };
    }
}

pub(crate) struct DropTargetRegistration {
    hwnd: HWND,
    _apartment: OleApartment,
}

impl DropTargetRegistration {
    pub(crate) fn new(
        window: &winit::window::Window,
        adapter: Weak<WinitWindowAdapter>,
    ) -> std::result::Result<Self, PlatformError> {
        let RawWindowHandle::Win32(handle) =
            window.window_handle().map_err(|e| PlatformError::from(e.to_string()))?.as_raw()
        else {
            return Err("Expected a Win32 window for the OLE drop target".into());
        };
        let hwnd = HWND(handle.hwnd.get() as *mut _);
        unsafe { OleInitialize(None) }.map_err(|e| PlatformError::from(e.to_string()))?;
        let apartment = OleApartment;
        let target: IDropTarget =
            DropTarget { adapter, hwnd, data: RefCell::default(), allowed: Cell::default() }.into();
        unsafe { RegisterDragDrop(hwnd, &target) }
            .map_err(|e| PlatformError::from(e.to_string()))?;
        Ok(Self { hwnd, _apartment: apartment })
    }
}

impl Drop for DropTargetRegistration {
    fn drop(&mut self) {
        let _ = unsafe { RevokeDragDrop(self.hwnd) };
    }
}

#[implement(IDropTarget)]
struct DropTarget {
    adapter: Weak<WinitWindowAdapter>,
    hwnd: HWND,
    data: RefCell<DataTransfer>,
    allowed: Cell<AllowedDragActions>,
}

impl DropTarget {
    fn dispatch(&self, pt: &POINTL, keys: MODIFIERKEYS_FLAGS, is_drop: bool) -> DROPEFFECT {
        let Some(adapter) = self.adapter.upgrade() else {
            return DROPEFFECT_NONE;
        };
        let mut position = POINT { x: pt.x, y: pt.y };
        if !unsafe { ScreenToClient(self.hwnd, &mut position) }.as_bool() {
            return DROPEFFECT_NONE;
        }
        let scale = adapter.window().scale_factor();
        let allowed = self.allowed.get();
        let mut modifiers = KeyboardModifiers::default();
        modifiers.control = keys.contains(MK_CONTROL);
        modifiers.shift = keys.contains(MK_SHIFT);
        let mut event = DropEvent::default();
        event.data = self.data.borrow().clone();
        event.position = LogicalPosition::new(position.x as f32 / scale, position.y as f32 / scale);
        event.proposed_action = compute_proposed_action(modifiers, allowed);
        let event = if is_drop {
            BackendDragEvent::Drop { event, allowed }
        } else {
            BackendDragEvent::Move { event, allowed }
        };
        let chosen = WindowInner::from_pub(adapter.window()).process_drag_event(event);
        match chosen {
            Some(DragAction::Copy) => DROPEFFECT_COPY,
            Some(DragAction::Move) => DROPEFFECT_MOVE,
            Some(DragAction::Link) => DROPEFFECT_LINK,
            _ => DROPEFFECT_NONE,
        }
    }

    fn leave(&self) {
        self.data.take();
        self.allowed.set(AllowedDragActions::default());
        if let Some(adapter) = self.adapter.upgrade() {
            WindowInner::from_pub(adapter.window()).process_drag_event(BackendDragEvent::Leave);
        }
    }
}

#[allow(non_snake_case)]
impl IDropTarget_Impl for DropTarget_Impl {
    fn DragEnter(
        &self,
        data: Ref<IDataObject>,
        keys: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> Result<()> {
        let effect =
            unsafe { effect.as_mut() }.ok_or_else(|| windows::core::Error::from(E_POINTER))?;
        self.allowed.set(AllowedDragActions {
            copy: effect.contains(DROPEFFECT_COPY),
            move_: effect.contains(DROPEFFECT_MOVE),
            link: effect.contains(DROPEFFECT_LINK),
        });
        self.data.replace(data.as_ref().map(file_transfer).unwrap_or_default());
        *effect = self.dispatch(pt, keys, false);
        Ok(())
    }

    fn DragOver(
        &self,
        keys: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> Result<()> {
        let effect =
            unsafe { effect.as_mut() }.ok_or_else(|| windows::core::Error::from(E_POINTER))?;
        *effect = self.dispatch(pt, keys, false);
        Ok(())
    }

    fn DragLeave(&self) -> Result<()> {
        self.leave();
        Ok(())
    }

    fn Drop(
        &self,
        data: Ref<IDataObject>,
        keys: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> Result<()> {
        let effect =
            unsafe { effect.as_mut() }.ok_or_else(|| windows::core::Error::from(E_POINTER))?;
        self.data.replace(data.as_ref().map(file_transfer).unwrap_or_default());
        *effect = self.dispatch(pt, keys, true);
        self.leave();
        Ok(())
    }
}

struct StorageMedium(STGMEDIUM);

impl Drop for StorageMedium {
    fn drop(&mut self) {
        unsafe { ReleaseStgMedium(&mut self.0) };
    }
}

fn file_transfer(object: &IDataObject) -> DataTransfer {
    let format = FORMATETC {
        cfFormat: CF_HDROP.0,
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
        ..Default::default()
    };
    let Ok(medium) = (unsafe { object.GetData(&format) }) else {
        return DataTransfer::default();
    };
    let medium = StorageMedium(medium);
    if medium.0.tymed != TYMED_HGLOBAL.0 as u32 {
        return DataTransfer::default();
    }
    let hdrop = HDROP(unsafe { medium.0.u.hGlobal }.0);
    let count = unsafe { DragQueryFileW(hdrop, u32::MAX, None) };
    let mut paths = Vec::new();
    for index in 0..count {
        let len = unsafe { DragQueryFileW(hdrop, index, None) } as usize;
        let mut buffer = vec![0u16; len + 1];
        let copied = unsafe { DragQueryFileW(hdrop, index, Some(&mut buffer)) } as usize;
        if copied > 0 {
            paths.push(PathBuf::from(OsString::from_wide(&buffer[..copied])));
        }
    }
    let mut transfer = DataTransfer::default();
    transfer.set_file_paths(paths);
    transfer
}
