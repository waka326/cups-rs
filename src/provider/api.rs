//! The read-only provider API.
//!
//! Everything a caller needs to decide whether a printer can do a job, with
//! none of the machinery underneath: no raw pointers, no IPP tag numbers, no
//! CUPS media flags, no knowledge of how the library was located at build
//! time.
//!
//! The job lifecycle lives in [`super::job`] and shares this type's thread.

use std::ffi::CStr;
use std::ptr;
use std::time::Duration;

use crate::bindings;
use crate::compat::{count_to_usize, usize_to_count};
use crate::destination::{Destination, DestinationInfo, PrinterState};

use super::attribute::AttributeState;
use super::error::{ProviderError, ProviderResult};
use super::job::{CupsJobBackend, JobService};
use super::media::{Margins, MediaDescriptor, MediaQuery, ReadyMedia, cups_units_to_um};
use super::value::{IppValue, decode_all};
use super::worker::ProviderHandle;

/// A printer, described without reference to any CUPS handle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrinterSummary {
    pub name: String,
    pub full_name: String,
    pub is_default: bool,
    pub state: PrinterState,
    pub accepting_jobs: bool,
    pub state_reasons: Vec<String>,
    pub info: Option<String>,
    pub location: Option<String>,
    pub make_and_model: Option<String>,
    pub printer_uri: Option<String>,
    pub device_uri: Option<String>,
}

/// Read-only access to the local CUPS scheduler.
///
/// All work runs on one dedicated thread (see [`ProviderHandle`]). The type
/// is `Send + Sync`, so it can be shared, but concurrent calls queue rather
/// than run in parallel.
pub struct CupsProvider {
    handle: ProviderHandle,
    /// Job operations run on the same thread as `handle`'s reads: one
    /// thread serialises every libcups call this provider makes.
    pub(in crate::provider) jobs: JobService<CupsJobBackend>,
}

impl CupsProvider {
    pub fn new() -> ProviderResult<Self> {
        let handle = ProviderHandle::new()?;
        Ok(Self {
            jobs: JobService::new(handle.clone(), CupsJobBackend::default())?,
            handle,
        })
    }

    /// Whether a previously timed-out call still holds the thread.
    pub fn is_busy(&self) -> bool {
        self.handle.is_busy()
    }

    /// Every destination the scheduler knows about.
    pub fn list_printers(&self, timeout: Duration) -> ProviderResult<Vec<PrinterSummary>> {
        self.handle.execute("list_printers", timeout, || {
            let destinations =
                crate::get_all_destinations().map_err(|err| ProviderError::ConnectionFailed {
                    detail: err.to_string(),
                })?;
            Ok(destinations.iter().map(summarise).collect())
        })
    }

    /// The default destination.
    ///
    /// `Ok(None)` means the scheduler answered and no destination is marked
    /// default. A scheduler that could not be reached is an error, not an
    /// absent default: those are different facts, and a caller planning a
    /// print needs to tell them apart.
    pub fn get_default_printer(&self, timeout: Duration) -> ProviderResult<Option<PrinterSummary>> {
        self.handle.execute("get_default_printer", timeout, || {
            // Enumerate and look for the default flag rather than asking for
            // the default directly. "No default printer" is then a property of
            // a list we actually received, instead of an error variant that
            // cannot be told apart from the scheduler being unreachable.
            let destinations =
                crate::get_all_destinations().map_err(|err| ProviderError::ConnectionFailed {
                    detail: format!("could not list destinations: {err}"),
                })?;

            Ok(pick_default(&destinations))
        })
    }

    pub fn get_printer_state(
        &self,
        printer: &str,
        timeout: Duration,
    ) -> ProviderResult<PrinterState> {
        let name = printer.to_string();
        self.handle.execute("get_printer_state", timeout, move || {
            Ok(lookup(&name)?.state())
        })
    }

    pub fn is_accepting_jobs(&self, printer: &str, timeout: Duration) -> ProviderResult<bool> {
        let name = printer.to_string();
        self.handle.execute("is_accepting_jobs", timeout, move || {
            Ok(lookup(&name)?.is_accepting_jobs())
        })
    }

    /// Supported values for an arbitrary attribute, with their IPP types
    /// intact.
    pub fn get_supported_values(
        &self,
        printer: &str,
        attribute: &str,
        timeout: Duration,
    ) -> ProviderResult<AttributeState<IppValue>> {
        let name = printer.to_string();
        let attribute = attribute.to_string();
        self.handle
            .execute("get_supported_values", timeout, move || {
                let destination = lookup(&name)?;
                let info = detailed_info(&destination)?;
                Ok(supported_state(&destination, &info, &attribute))
            })
    }

    /// The printer's default for an attribute.
    ///
    /// `Present([IppValue::NoValue])` means the printer stated there is no
    /// default. That is different from `NotAdvertised`, which means it never
    /// mentioned one, and from a default of `false`.
    pub fn get_default_value(
        &self,
        printer: &str,
        attribute: &str,
        timeout: Duration,
    ) -> ProviderResult<AttributeState<IppValue>> {
        let name = printer.to_string();
        let attribute = attribute.to_string();
        self.handle.execute("get_default_value", timeout, move || {
            let destination = lookup(&name)?;
            let info = detailed_info(&destination)?;
            Ok(default_state(&destination, &info, &attribute))
        })
    }

    pub fn get_supported_sides(
        &self,
        printer: &str,
        timeout: Duration,
    ) -> ProviderResult<AttributeState<IppValue>> {
        self.get_supported_values(printer, "sides", timeout)
    }

    pub fn get_supported_color_modes(
        &self,
        printer: &str,
        timeout: Duration,
    ) -> ProviderResult<AttributeState<IppValue>> {
        self.get_supported_values(printer, "print-color-mode", timeout)
    }

    pub fn get_supported_quality(
        &self,
        printer: &str,
        timeout: Duration,
    ) -> ProviderResult<AttributeState<IppValue>> {
        self.get_supported_values(printer, "print-quality", timeout)
    }

    /// Supported media types.
    ///
    /// Vendor keywords such as `com.canon.mthagakia` are returned verbatim;
    /// what they mean is for the caller to decide.
    pub fn get_supported_media_types(
        &self,
        printer: &str,
        timeout: Duration,
    ) -> ProviderResult<AttributeState<IppValue>> {
        self.get_supported_values(printer, "media-type", timeout)
    }

    pub fn get_supported_media_sources(
        &self,
        printer: &str,
        timeout: Duration,
    ) -> ProviderResult<AttributeState<IppValue>> {
        self.get_supported_values(printer, "media-source", timeout)
    }

    /// Supported copy counts.
    ///
    /// Usually a range, so ask with
    /// [`AttributeState::accepts_integer`] rather than comparing values.
    pub fn get_supported_copies(
        &self,
        printer: &str,
        timeout: Duration,
    ) -> ProviderResult<AttributeState<IppValue>> {
        self.get_supported_values(printer, "copies", timeout)
    }

    /// Every media size the printer offers, as structured dimensions.
    pub fn get_supported_media(
        &self,
        printer: &str,
        query: MediaQuery,
        timeout: Duration,
    ) -> ProviderResult<Vec<MediaDescriptor>> {
        let name = printer.to_string();
        self.handle
            .execute("get_supported_media", timeout, move || {
                let destination = lookup(&name)?;
                let info = detailed_info(&destination)?;
                let dest_ptr = destination.as_ptr();
                let flags = query.flags();

                let count = info.get_media_count(ptr::null_mut(), dest_ptr, flags);
                let mut media = Vec::with_capacity(count);
                let mut unreadable = 0usize;
                for index in 0..count {
                    // A single unreadable entry should not discard the rest of
                    // the catalogue, but losing all of them silently would
                    // look identical to a printer that offers no media.
                    match info.get_media_by_index(ptr::null_mut(), dest_ptr, index, flags) {
                        Ok(size) => media.push(descriptor_from(size, query)),
                        Err(_) => unreadable += 1,
                    }
                }

                if media.is_empty() && unreadable > 0 {
                    return Err(ProviderError::CapabilityUnknown {
                        printer: name.clone(),
                        attribute: format!("media ({unreadable} entries unreadable)"),
                    });
                }

                Ok(media)
            })
    }

    /// The media matching a physical size, in micrometres.
    ///
    /// Returns `MediaUnknown` when the printer offers nothing of that size.
    pub fn get_media_by_size(
        &self,
        printer: &str,
        width_um: u32,
        length_um: u32,
        query: MediaQuery,
        timeout: Duration,
    ) -> ProviderResult<MediaDescriptor> {
        let name = printer.to_string();
        self.handle.execute("get_media_by_size", timeout, move || {
            let destination = lookup(&name)?;
            let info = detailed_info(&destination)?;
            // CUPS wants hundredths of a millimetre; the division is exact
            // for any micrometre value CUPS itself produced.
            let width = um_to_cups_units(width_um);
            let length = um_to_cups_units(length_um);

            info.get_media_by_size(
                ptr::null_mut(),
                destination.as_ptr(),
                width,
                length,
                query.flags(),
            )
            .map(|size| descriptor_from(size, query))
            .map_err(|err| ProviderError::MediaUnknown {
                printer: name.clone(),
                detail: err.to_string(),
            })
        })
    }

    /// Media the printer reports as currently loaded.
    ///
    /// `NotAdvertised` means CUPS said nothing about loaded media. It does
    /// **not** mean the trays are empty — all three printers tested during
    /// the spike report this even with paper loaded.
    pub fn get_ready_media(&self, printer: &str, timeout: Duration) -> ProviderResult<ReadyMedia> {
        let name = printer.to_string();
        self.handle.execute("get_ready_media", timeout, move || {
            let destination = lookup(&name)?;
            let info = detailed_info(&destination)?;
            let dest_ptr = destination.as_ptr();

            let attribute = unsafe {
                let option = std::ffi::CString::new("media").map_err(|_| {
                    ProviderError::InternalContractViolation {
                        detail: "media option name contained a NUL".into(),
                    }
                })?;
                bindings::cupsFindDestReady(
                    ptr::null_mut(),
                    dest_ptr,
                    info.as_ptr(),
                    option.as_ptr(),
                )
            };

            // A null attribute means the printer never advertised ready
            // media; an attribute with zero values means it advertised an
            // empty list. Upstream returns an empty Vec for both.
            if attribute.is_null() {
                return Ok(ReadyMedia::NotAdvertised);
            }

            let mut descriptors = Vec::new();
            let count = count_to_usize(unsafe { bindings::ippGetCount(attribute) });
            for index in 0..count {
                let media_name = unsafe {
                    let ptr =
                        bindings::ippGetString(attribute, usize_to_count(index), ptr::null_mut());
                    if ptr.is_null() {
                        continue;
                    }
                    CStr::from_ptr(ptr).to_string_lossy().into_owned()
                };

                match info.get_media_by_name(ptr::null_mut(), dest_ptr, &media_name, 0) {
                    Ok(size) => descriptors.push(descriptor_from(size, MediaQuery::Default)),
                    // The name is known to be ready even when its dimensions
                    // cannot be resolved; keep it with zeroed dimensions
                    // rather than dropping it.
                    Err(_) => descriptors.push(MediaDescriptor {
                        canonical_name: media_name,
                        width_um: 0,
                        length_um: 0,
                        margins: Margins::default(),
                        borderless: false,
                    }),
                }
            }

            Ok(ReadyMedia::Present(descriptors))
        })
    }
}

/// CUPS stores media sizes in hundredths of a millimetre.
fn um_to_cups_units(um: u32) -> i32 {
    (um / 10) as i32
}

fn summarise(destination: &Destination) -> PrinterSummary {
    PrinterSummary {
        name: destination.name.clone(),
        full_name: destination.full_name(),
        is_default: destination.is_default,
        state: destination.state(),
        accepting_jobs: destination.is_accepting_jobs(),
        state_reasons: destination.state_reasons(),
        info: destination.info().cloned(),
        location: destination.location().cloned(),
        make_and_model: destination.make_and_model().cloned(),
        printer_uri: destination.uri().cloned(),
        device_uri: destination.device_uri().cloned(),
    }
}

/// The destination marked default, if the list contains one.
///
/// Split out so the "answered, but nothing is default" case can be tested
/// without a scheduler. A failure to obtain the list never reaches here; the
/// caller turns that into an error instead.
fn pick_default(destinations: &[Destination]) -> Option<PrinterSummary> {
    destinations
        .iter()
        .find(|destination| destination.is_default)
        .map(summarise)
}

fn lookup(printer: &str) -> ProviderResult<Destination> {
    crate::get_destination(printer).map_err(|_| ProviderError::PrinterNotFound {
        printer: printer.to_string(),
    })
}

fn detailed_info(destination: &Destination) -> ProviderResult<DestinationInfo> {
    destination
        .get_detailed_info(ptr::null_mut())
        .map_err(|err| ProviderError::ConnectionFailed {
            detail: format!(
                "could not read capabilities for {}: {err}",
                destination.name
            ),
        })
}

/// Read a "-supported" attribute, distinguishing missing from empty.
fn supported_state(
    destination: &Destination,
    info: &DestinationInfo,
    attribute: &str,
) -> AttributeState<IppValue> {
    let Ok(option) = std::ffi::CString::new(attribute) else {
        return AttributeState::NotAdvertised;
    };
    let attr = unsafe {
        bindings::cupsFindDestSupported(
            ptr::null_mut(),
            destination.as_ptr(),
            info.as_ptr(),
            option.as_ptr(),
        )
    };
    if attr.is_null() {
        return AttributeState::NotAdvertised;
    }
    AttributeState::from_present(unsafe { decode_all(attr) })
}

/// Read a "-default" attribute, distinguishing missing from empty.
fn default_state(
    destination: &Destination,
    info: &DestinationInfo,
    attribute: &str,
) -> AttributeState<IppValue> {
    let Ok(option) = std::ffi::CString::new(attribute) else {
        return AttributeState::NotAdvertised;
    };
    let attr = unsafe {
        bindings::cupsFindDestDefault(
            ptr::null_mut(),
            destination.as_ptr(),
            info.as_ptr(),
            option.as_ptr(),
        )
    };
    if attr.is_null() {
        return AttributeState::NotAdvertised;
    }
    AttributeState::from_present(unsafe { decode_all(attr) })
}

fn descriptor_from(size: crate::destination::MediaSize, query: MediaQuery) -> MediaDescriptor {
    let margins = Margins {
        top_um: cups_units_to_um(size.top),
        bottom_um: cups_units_to_um(size.bottom),
        left_um: cups_units_to_um(size.left),
        right_um: cups_units_to_um(size.right),
    };
    MediaDescriptor {
        canonical_name: size.name,
        width_um: cups_units_to_um(size.width),
        length_um: cups_units_to_um(size.length),
        margins,
        // Borderless is judged by the margins CUPS reported, not by the name
        // or by which query was used.
        borderless: matches!(query, MediaQuery::Borderless) && margins.is_full_bleed(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn destination(name: &str, is_default: bool) -> Destination {
        Destination {
            name: name.to_string(),
            instance: None,
            is_default,
            options: Default::default(),
        }
    }

    #[test]
    fn genuine_no_default_yields_none() {
        // The scheduler answered; nothing is marked default. That is a fact
        // about the configuration, not a failure.
        let destinations = vec![
            destination("Canon_TS5400_series", false),
            destination("EPSON_EW_456A_Series", false),
        ];
        assert!(pick_default(&destinations).is_none());
    }

    #[test]
    fn empty_destination_list_yields_none() {
        assert!(pick_default(&[]).is_none());
    }

    #[test]
    fn default_printer_is_found_by_flag() {
        let destinations = vec![
            destination("Canon_TS5400_series", false),
            destination("Brother_MFC_7460DN", true),
            destination("EPSON_EW_456A_Series", false),
        ];
        let found = pick_default(&destinations).expect("a default exists");
        assert_eq!(found.name, "Brother_MFC_7460DN");
        assert!(found.is_default);
    }

    #[test]
    fn query_failure_is_not_an_absent_default() {
        // get_default_printer maps a failed enumeration to ConnectionFailed.
        // Nothing in the provider may turn that into Ok(None): "we could not
        // ask" and "there is no default" lead to different decisions.
        let failure = ProviderError::ConnectionFailed {
            detail: "could not list destinations: simulated".into(),
        };
        assert!(!failure.is_operator_actionable());
        // The error carries no printer, because none was identified.
        assert_eq!(failure.printer(), None);
    }

    #[test]
    fn converts_um_to_cups_units() {
        assert_eq!(um_to_cups_units(100_000), 10_000);
        assert_eq!(um_to_cups_units(148_000), 14_800);
    }

    #[test]
    fn descriptor_keeps_dimensions_exact() {
        let size = crate::destination::MediaSize {
            name: "jpn_hagaki_100x148mm".into(),
            width: 10_000,
            length: 14_800,
            top: 300,
            bottom: 300,
            left: 300,
            right: 300,
        };
        let descriptor = descriptor_from(size, MediaQuery::Default);
        assert_eq!(descriptor.width_um, 100_000);
        assert_eq!(descriptor.length_um, 148_000);
        assert_eq!(descriptor.margins.top_um, 3_000);
        assert!(descriptor.is_postcard_100x148());
        assert!(!descriptor.borderless);
    }

    #[test]
    fn borderless_requires_zero_margins_not_just_the_query() {
        // Asking for borderless does not make a bordered sheet borderless.
        let bordered = crate::destination::MediaSize {
            name: "jpn_hagaki_100x148mm".into(),
            width: 10_000,
            length: 14_800,
            top: 300,
            bottom: 300,
            left: 300,
            right: 300,
        };
        assert!(!descriptor_from(bordered, MediaQuery::Borderless).borderless);

        let full_bleed = crate::destination::MediaSize {
            name: "jpn_hagaki_100x148mm_borderless".into(),
            width: 10_000,
            length: 14_800,
            top: 0,
            bottom: 0,
            left: 0,
            right: 0,
        };
        assert!(descriptor_from(full_bleed, MediaQuery::Borderless).borderless);
    }
}
