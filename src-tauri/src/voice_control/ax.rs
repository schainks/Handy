//! A small owned wrapper over the macOS Accessibility API (AXUIElement), used
//! to read and press other apps' menus and on-screen items, and to click where
//! the pointer is. Every call needs the Accessibility permission Handy already
//! has for pasting.

use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType, CGPoint, CGSize, Type,
};
use std::ffi::c_void;
use std::ptr::NonNull;

pub type AXError = i32;
pub const AX_SUCCESS: AXError = 0;
pub const AX_CANNOT_COMPLETE: AXError = -25204;
pub const AX_API_DISABLED: AXError = -25211;

const AX_VALUE_CG_POINT: u32 = 1;
const AX_VALUE_CG_SIZE: u32 = 2;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateApplication(pid: i32) -> *mut c_void;
    fn AXUIElementCreateSystemWide() -> *mut c_void;
    fn AXUIElementCopyAttributeValue(
        element: *const c_void,
        attribute: *const c_void,
        value: *mut *const c_void,
    ) -> AXError;
    fn AXUIElementCopyParameterizedAttributeValue(
        element: *const c_void,
        attribute: *const c_void,
        parameter: *const c_void,
        result: *mut *const c_void,
    ) -> AXError;
    fn AXUIElementSetAttributeValue(
        element: *const c_void,
        attribute: *const c_void,
        value: *const c_void,
    ) -> AXError;
    fn AXUIElementPerformAction(element: *const c_void, action: *const c_void) -> AXError;
    fn AXUIElementSetMessagingTimeout(element: *const c_void, seconds: f32) -> AXError;
    fn AXUIElementCopyElementAtPosition(
        application: *const c_void,
        x: f32,
        y: f32,
        element: *mut *const c_void,
    ) -> AXError;
    fn AXValueGetValue(value: *const c_void, value_type: u32, value_ptr: *mut c_void) -> u8;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventCreate(source: *const c_void) -> *mut c_void;
    fn CGEventGetLocation(event: *const c_void) -> CGPoint;
    fn CGEventCreateMouseEvent(
        source: *const c_void,
        mouse_type: u32,
        position: CGPoint,
        button: u32,
    ) -> *mut c_void;
    fn CGEventSetIntegerValueField(event: *const c_void, field: u32, value: i64);
    fn CGEventPost(tap: u32, event: *const c_void);
}

fn cf_ptr<T: ?Sized + Type>(object: &CFRetained<T>) -> *const c_void {
    CFRetained::as_ptr(object).as_ptr().cast_const().cast()
}

/// Take ownership of a +1 reference (Create/Copy rule), if not null.
fn owned(raw: *const c_void) -> Option<CFRetained<CFType>> {
    // SAFETY: callers pass references they own, from Create/Copy functions.
    NonNull::new(raw.cast_mut().cast::<CFType>()).map(|ptr| unsafe { CFRetained::from_raw(ptr) })
}

/// An AXUIElement, owned, with the messaging timeout its requests use.
/// Elements reached from it get the same timeout.
pub struct Element(CFRetained<CFType>, f32);

// SAFETY: an AXUIElementRef is an immutable CF object. Retaining, releasing
// and sending it requests are thread-safe, so it may move between threads.
unsafe impl Send for Element {}

impl Element {
    /// The app with process id `pid`.
    pub fn application(pid: i32, timeout: f32) -> Option<Self> {
        // SAFETY: returns a +1 reference or null.
        owned(unsafe { AXUIElementCreateApplication(pid) }).map(|app| Self::new(app, timeout))
    }

    /// The whole screen, for finding what's at a point.
    pub fn system_wide(timeout: f32) -> Option<Self> {
        // SAFETY: returns a +1 reference or null.
        owned(unsafe { AXUIElementCreateSystemWide() }).map(|wide| Self::new(wide, timeout))
    }

    fn new(object: CFRetained<CFType>, timeout: f32) -> Self {
        let element = Element(object, timeout);
        // SAFETY: valid element; the timeout only affects this element.
        unsafe { AXUIElementSetMessagingTimeout(element.as_ptr(), timeout) };
        element
    }

    fn as_ptr(&self) -> *const c_void {
        cf_ptr(&self.0)
    }

    pub fn attribute(&self, name: &str) -> Result<Option<CFRetained<CFType>>, AXError> {
        let name = CFString::from_str(name);
        let mut value: *const c_void = std::ptr::null();
        // SAFETY: valid element and attribute name; on success `value` holds
        // a +1 reference or stays null.
        let err =
            unsafe { AXUIElementCopyAttributeValue(self.as_ptr(), cf_ptr(&name), &mut value) };
        if err != AX_SUCCESS {
            return Err(err);
        }
        Ok(owned(value))
    }

    pub fn string(&self, name: &str) -> Option<String> {
        self.attribute(name)
            .ok()??
            .downcast::<CFString>()
            .ok()
            .map(|value| value.to_string())
    }

    pub fn flag(&self, name: &str) -> Option<bool> {
        self.attribute(name)
            .ok()??
            .downcast::<CFBoolean>()
            .ok()
            .map(|value| value.as_bool())
    }

    pub fn element(&self, name: &str) -> Result<Option<Element>, AXError> {
        Ok(self
            .attribute(name)?
            .map(|object| Element::new(object, self.1)))
    }

    fn element_array(&self, value: CFRetained<CFType>) -> Vec<Element> {
        let Ok(array) = value.downcast::<CFArray>() else {
            return Vec::new();
        };
        // SAFETY: the arrays read here hold AXUIElements, which are CF types.
        let array: CFRetained<CFArray<CFType>> = unsafe { CFRetained::cast_unchecked(array) };
        array
            .iter()
            .map(|object| Element::new(object, self.1))
            .collect()
    }

    pub fn elements(&self, name: &str) -> Vec<Element> {
        match self.attribute(name) {
            Ok(Some(value)) => self.element_array(value),
            _ => Vec::new(),
        }
    }

    pub fn children(&self) -> Vec<Element> {
        self.elements("AXChildren")
    }

    pub fn title(&self) -> String {
        self.string("AXTitle").unwrap_or_default()
    }

    pub fn role(&self) -> String {
        self.string("AXRole").unwrap_or_default()
    }

    /// Screen position (top-left origin) and size.
    pub fn frame(&self) -> Option<(CGPoint, CGSize)> {
        let position = self.attribute("AXPosition").ok()??;
        let size = self.attribute("AXSize").ok()??;
        let mut point = CGPoint { x: 0.0, y: 0.0 };
        let mut extent = CGSize {
            width: 0.0,
            height: 0.0,
        };
        // SAFETY: AXPosition and AXSize are AXValues of these types; the
        // out-pointers match them.
        let ok = unsafe {
            AXValueGetValue(
                cf_ptr(&position),
                AX_VALUE_CG_POINT,
                (&mut point as *mut CGPoint).cast(),
            ) != 0
                && AXValueGetValue(
                    cf_ptr(&size),
                    AX_VALUE_CG_SIZE,
                    (&mut extent as *mut CGSize).cast(),
                ) != 0
        };
        ok.then_some((point, extent))
    }

    /// Descendants matching any of `search_keys` ("AXLinkSearchKey",
    /// "AXControlSearchKey"...) that are visible on screen, in page order.
    /// Web views (Safari's WebKit, Chromium) answer this in one request, the
    /// way VoiceOver's rotor finds links.
    pub fn search_visible(
        &self,
        search_keys: &[&str],
        limit: i32,
    ) -> Result<Vec<Element>, AXError> {
        let keys: Vec<CFRetained<CFString>> = search_keys
            .iter()
            .map(|key| CFString::from_str(key))
            .collect();
        let keys = CFArray::from_retained_objects(&keys);
        let limit = CFNumber::new_i32(limit);
        let direction = CFString::from_str("AXDirectionNext");
        let names = [
            CFString::from_str("AXSearchKey"),
            CFString::from_str("AXVisibleOnly"),
            CFString::from_str("AXResultsLimit"),
            CFString::from_str("AXDirection"),
        ];
        let names: Vec<&CFString> = names.iter().map(|name| &**name).collect();
        let values: [&CFType; 4] = [&keys, CFBoolean::new(true), &limit, &direction];
        let predicate = CFDictionary::<CFString, CFType>::from_slices(&names, &values);

        let attribute = CFString::from_str("AXUIElementsForSearchPredicate");
        let mut result: *const c_void = std::ptr::null();
        // SAFETY: valid element, attribute and parameter; on success
        // `result` holds a +1 reference or stays null.
        let err = unsafe {
            AXUIElementCopyParameterizedAttributeValue(
                self.as_ptr(),
                cf_ptr(&attribute),
                cf_ptr(&predicate),
                &mut result,
            )
        };
        if err != AX_SUCCESS {
            return Err(err);
        }
        Ok(owned(result)
            .map(|value| self.element_array(value))
            .unwrap_or_default())
    }

    /// The deepest element at a screen point (top-left origin), in any app.
    pub fn element_at(&self, point: CGPoint) -> Option<Element> {
        let mut found: *const c_void = std::ptr::null();
        // SAFETY: valid element; on success `found` holds a +1 reference.
        let err = unsafe {
            AXUIElementCopyElementAtPosition(
                self.as_ptr(),
                point.x as f32,
                point.y as f32,
                &mut found,
            )
        };
        if err != AX_SUCCESS {
            return None;
        }
        owned(found).map(|object| Element::new(object, self.1))
    }

    pub fn press(&self) -> Result<(), AXError> {
        let action = CFString::from_str("AXPress");
        // SAFETY: valid element and action name.
        let err = unsafe { AXUIElementPerformAction(self.as_ptr(), cf_ptr(&action)) };
        if err == AX_SUCCESS {
            Ok(())
        } else {
            Err(err)
        }
    }

    pub fn set_flag(&self, name: &str, value: bool) -> Result<(), AXError> {
        let attribute = CFString::from_str(name);
        let value: &CFType = CFBoolean::new(value);
        // SAFETY: valid element, attribute and CFBoolean value.
        let err = unsafe {
            AXUIElementSetAttributeValue(
                self.as_ptr(),
                cf_ptr(&attribute),
                (value as *const CFType).cast(),
            )
        };
        if err == AX_SUCCESS {
            Ok(())
        } else {
            Err(err)
        }
    }

    /// Give the element keyboard focus (for text fields).
    pub fn focus(&self) -> Result<(), AXError> {
        self.set_flag("AXFocused", true)
    }
}

/// Where the mouse pointer is, in screen coordinates (top-left origin).
pub fn pointer_location() -> Option<CGPoint> {
    // SAFETY: CGEventCreate(NULL) returns a +1 event carrying the current
    // pointer location, or null.
    let event = owned(unsafe { CGEventCreate(std::ptr::null()) })?;
    Some(unsafe { CGEventGetLocation(cf_ptr(&event)) })
}

/// A left click at `point`, which should be where the pointer already is so
/// that it doesn't move.
pub fn click_at(point: CGPoint) -> Result<(), String> {
    const LEFT_MOUSE_DOWN: u32 = 1;
    const LEFT_MOUSE_UP: u32 = 2;
    const LEFT_BUTTON: u32 = 0;
    const CLICK_STATE: u32 = 1;
    const HID_EVENT_TAP: u32 = 0;
    for mouse_type in [LEFT_MOUSE_DOWN, LEFT_MOUSE_UP] {
        // SAFETY: returns a +1 event or null.
        let event = owned(unsafe {
            CGEventCreateMouseEvent(std::ptr::null(), mouse_type, point, LEFT_BUTTON)
        })
        .ok_or("could not create a mouse event")?;
        // SAFETY: valid event; a single click.
        unsafe {
            CGEventSetIntegerValueField(cf_ptr(&event), CLICK_STATE, 1);
            CGEventPost(HID_EVENT_TAP, cf_ptr(&event));
        }
    }
    Ok(())
}
