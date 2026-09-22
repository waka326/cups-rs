#[cfg(cups2)]
pub(crate) type CupsCount = std::os::raw::c_int;

#[cfg(cups3)]
pub(crate) type CupsCount = usize;

#[cfg(cups2)]
pub(crate) type CupsMedia = crate::bindings::cups_size_s;

#[cfg(cups3)]
pub(crate) type CupsMedia = crate::bindings::cups_media_s;

#[cfg(cups2)]
pub(crate) fn count_to_usize(count: CupsCount) -> usize {
    usize::try_from(count).unwrap_or(0)
}

#[cfg(cups3)]
pub(crate) fn count_to_usize(count: CupsCount) -> usize {
    count
}

#[cfg(cups2)]
pub(crate) fn usize_to_count(count: usize) -> CupsCount {
    std::os::raw::c_int::try_from(count).unwrap_or(std::os::raw::c_int::MAX)
}

#[cfg(cups3)]
pub(crate) fn usize_to_count(count: usize) -> CupsCount {
    count
}

pub(crate) fn empty_media() -> CupsMedia {
    unsafe { std::mem::zeroed() }
}

#[cfg(cups2)]
pub(crate) fn cups_bool(value: std::os::raw::c_int) -> bool {
    value != 0
}

#[cfg(cups3)]
pub(crate) fn cups_bool(value: bool) -> bool {
    value
}

/// The user CUPS will attribute requests to. Renamed in CUPS 3.
#[cfg(cups2)]
pub(crate) fn cups_user() -> *const std::os::raw::c_char {
    unsafe { crate::bindings::cupsUser() }
}

#[cfg(cups3)]
pub(crate) fn cups_user() -> *const std::os::raw::c_char {
    unsafe { crate::bindings::cupsGetUser() }
}

/// An IPP boolean argument: a C `char` in CUPS 2, a `bool` in CUPS 3.
#[cfg(cups2)]
pub(crate) fn ipp_boolean(value: bool) -> std::os::raw::c_char {
    std::os::raw::c_char::from(value)
}

#[cfg(cups3)]
pub(crate) fn ipp_boolean(value: bool) -> bool {
    value
}

/// Attribute iteration. Renamed `ippGet{First,Next}Attribute` in CUPS 3.
///
/// # Safety
/// `ipp` must be a valid IPP message.
#[cfg(cups2)]
pub(crate) unsafe fn ipp_first_attribute(
    ipp: *mut crate::bindings::ipp_t,
) -> *mut crate::bindings::ipp_attribute_t {
    unsafe { crate::bindings::ippFirstAttribute(ipp) }
}

#[cfg(cups3)]
pub(crate) unsafe fn ipp_first_attribute(
    ipp: *mut crate::bindings::ipp_t,
) -> *mut crate::bindings::ipp_attribute_t {
    unsafe { crate::bindings::ippGetFirstAttribute(ipp) }
}

/// # Safety
/// `ipp` must be a valid IPP message being iterated.
#[cfg(cups2)]
pub(crate) unsafe fn ipp_next_attribute(
    ipp: *mut crate::bindings::ipp_t,
) -> *mut crate::bindings::ipp_attribute_t {
    unsafe { crate::bindings::ippNextAttribute(ipp) }
}

#[cfg(cups3)]
pub(crate) unsafe fn ipp_next_attribute(
    ipp: *mut crate::bindings::ipp_t,
) -> *mut crate::bindings::ipp_attribute_t {
    unsafe { crate::bindings::ippGetNextAttribute(ipp) }
}
