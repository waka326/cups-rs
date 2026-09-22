//! The libcups implementation of [`JobBackend`].
//!
//! Requests are built as raw IPP and sent with `cupsDoRequest` /
//! `cupsSendRequest` on the provider thread's default scheduler connection,
//! rather than through the `cupsCreateDestJob` family. The `Dest` wrappers
//! delete the scheduler's response and return only `cupsLastError()`, which
//! cannot tell "the scheduler refused" from "no answer arrived" — the one
//! distinction duplicate-print safety depends on. Here a null response means
//! no answer, and a response's status code means the scheduler's verdict.
//!
//! libcups re-sends a request by itself only when the POST could not be
//! written at all, or after HTTP 401, 417 or 426 — answers cupsd gives before
//! it reads the IPP operation (`cupsSendRequest`, `cupsDoIORequest`). No
//! request that could have created or changed a job is ever repeated.
//!
//! Nothing here allocates a `cups_dest_t`, holds a `cups_dinfo_t`, or lets a
//! pointer outlive the call that created it.

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::ptr;

use crate::bindings;
use crate::compat::{
    cups_user, ipp_boolean, ipp_first_attribute, ipp_next_attribute, usize_to_count,
};
use crate::error::Error;
use crate::error_helpers::get_cups_error_details;
use crate::provider::error::{ProviderError, ProviderResult};

use super::backend::{CreateAccepted, JobBackend, MutationFailure};
use super::options::{AttributeValue, JobAttribute};
use super::types::{JobRejection, ProviderJobId, ProviderJobStatus};

const PDF_FORMAT: &CStr = c"application/pdf";

/// `CUPS_LENGTH_VARIABLE`: send the body chunked, length unknown up front.
const LENGTH_VARIABLE: usize = 0;

/// The scheduler's resource for job URIs (`ipp://host/jobs/<id>`).
const JOBS_RESOURCE: &CStr = c"/jobs/";

#[derive(Default)]
pub(crate) struct CupsJobBackend {
    /// Resource of the Send-Document request in progress, between
    /// `start_document` and `finish_document`.
    open_document: Option<CString>,
}

impl JobBackend for CupsJobBackend {
    fn create_job(
        &mut self,
        printer: &str,
        title: &str,
        attributes: &[JobAttribute],
    ) -> Result<CreateAccepted, MutationFailure> {
        refuse_in_unit_tests()?;
        let target = resolve(printer)?;
        let mut request = Ipp::request(bindings::ipp_op_e_IPP_OP_CREATE_JOB)?;
        request.add_target(&target)?;
        if !title.is_empty() {
            request.add_name("job-name", title)?;
        }
        for attribute in attributes {
            request.add_attribute(bindings::ipp_tag_e_IPP_TAG_JOB, attribute)?;
        }

        let response = send(request, &target.resource)?;
        let status = response.status();
        let job_id = response.integer("job-id", bindings::ipp_tag_e_IPP_TAG_INTEGER);
        if !is_successful(status) {
            // IPP says a refused Create-Job creates nothing. A refusal that
            // still names a job is not something to trust either way.
            if job_id.is_some_and(|id| id > 0) {
                return Err(MutationFailure::NoAnswer {
                    detail: format!("refusal 0x{status:04x} carried a job-id"),
                });
            }
            return Err(rejected(status));
        }
        Ok(CreateAccepted {
            // A success without a usable id is recorded as an unknown outcome
            // by the service; 0 is never a valid id.
            job_id: job_id.and_then(|id| u32::try_from(id).ok()).unwrap_or(0),
            ignored_attributes: response.unsupported_attribute_names(),
        })
    }

    fn start_document(&mut self, job: &ProviderJobId) -> Result<(), MutationFailure> {
        refuse_in_unit_tests()?;
        if self.open_document.is_some() {
            return Err(not_sent("a document is already open"));
        }
        let target = resolve(job.printer())?;
        let mut request = Ipp::request(bindings::ipp_op_e_IPP_OP_SEND_DOCUMENT)?;
        request.add_target(&target)?;
        request.add_job_id(job)?;
        request.add_mimetype("document-format", PDF_FORMAT)?;
        // Not the last document: the job stays held until Close-Job, so a
        // submission that fails part-way never starts printing.
        request.add_boolean("last-document", false)?;

        // SAFETY: the request and resource outlive the call; cupsSendRequest
        // does not take ownership of the request.
        let status = unsafe {
            bindings::cupsSendRequest(
                ptr::null_mut(),
                request.as_ptr(),
                target.resource.as_ptr(),
                LENGTH_VARIABLE,
            )
        };
        if status == bindings::http_status_e_HTTP_STATUS_CONTINUE {
            self.open_document = Some(target.resource);
            return Ok(());
        }
        if status == bindings::http_status_e_HTTP_STATUS_OK {
            // The scheduler answered before any document data: read why.
            return match receive(&target.resource) {
                Some(response) if !is_successful(response.status()) => {
                    Err(rejected(response.status()))
                }
                _ => Err(no_answer("scheduler answered Send-Document early")),
            };
        }
        Err(no_answer(&format!(
            "Send-Document not started (HTTP {status})"
        )))
    }

    fn write_document(&mut self, chunk: &[u8]) -> Result<(), MutationFailure> {
        refuse_in_unit_tests()?;
        if self.open_document.is_none() {
            return Err(not_sent("no document is open"));
        }
        // SAFETY: `chunk` is valid for `chunk.len()` bytes for the call.
        let status = unsafe {
            bindings::cupsWriteRequestData(
                ptr::null_mut(),
                chunk.as_ptr().cast::<c_char>(),
                chunk.len(),
            )
        };
        if status == bindings::http_status_e_HTTP_STATUS_CONTINUE {
            return Ok(());
        }
        // The request is dead. libcups flushes or reconnects the connection
        // before the next request, so nothing needs sending to tidy it up.
        self.open_document = None;
        Err(no_answer(&format!("document write failed (HTTP {status})")))
    }

    fn finish_document(&mut self, _job: &ProviderJobId) -> Result<(), MutationFailure> {
        refuse_in_unit_tests()?;
        let resource = self
            .open_document
            .take()
            .ok_or_else(|| not_sent("no document is open"))?;
        match receive(&resource) {
            None => Err(no_answer("no verdict on the document")),
            Some(response) if is_successful(response.status()) => Ok(()),
            Some(response) => Err(rejected(response.status())),
        }
    }

    fn close_job(&mut self, job: &ProviderJobId) -> Result<(), MutationFailure> {
        refuse_in_unit_tests()?;
        // cupsd's Close-Job accepts only printer-uri + job-id.
        let target = resolve(job.printer())?;
        let mut request = Ipp::request(bindings::ipp_op_e_IPP_OP_CLOSE_JOB)?;
        request.add_target(&target)?;
        request.add_job_id(job)?;
        verdict(send(request, &target.resource)?)
    }

    fn cancel_job(&mut self, job: &ProviderJobId) -> Result<(), MutationFailure> {
        refuse_in_unit_tests()?;
        let mut request = Ipp::request(bindings::ipp_op_e_IPP_OP_CANCEL_JOB)?;
        request.add_job_uri(job)?;
        verdict(send(request, JOBS_RESOURCE)?)
    }

    fn job_status(&mut self, job: &ProviderJobId) -> ProviderResult<ProviderJobStatus> {
        let query_failed = |detail: String| ProviderError::JobStatusQueryFailed {
            job: job.clone(),
            detail,
        };
        let build = || -> Result<Ipp, MutationFailure> {
            let mut request = Ipp::request(bindings::ipp_op_e_IPP_OP_GET_JOB_ATTRIBUTES)?;
            request.add_job_uri(job)?;
            Ok(request)
        };
        let request = build().map_err(|failure| query_failed(format!("{failure:?}")))?;
        let response = send(request, JOBS_RESOURCE).map_err(|failure| match failure {
            MutationFailure::NoAnswer { detail } => query_failed(detail),
            other => query_failed(format!("{other:?}")),
        })?;

        let status = response.status();
        if status == i64::from(bindings::ipp_status_e_IPP_STATUS_ERROR_NOT_FOUND) {
            return Err(ProviderError::JobNotFound { job: job.clone() });
        }
        if !is_successful(status) {
            return Err(query_failed(format!("scheduler answered 0x{status:04x}")));
        }

        let owner = response
            .string("job-printer-uri", bindings::ipp_tag_e_IPP_TAG_URI)
            .ok_or_else(|| query_failed("no job-printer-uri in the answer".into()))?;
        let owner_queue = queue_name(&owner)
            .ok_or_else(|| query_failed(format!("unrecognised job-printer-uri {owner}")))?;
        if !owner_queue.eq_ignore_ascii_case(job.printer()) {
            return Err(ProviderError::JobDestinationMismatch {
                job: job.clone(),
                reported_printer: owner_queue,
            });
        }

        Ok(response
            .integer("job-state", bindings::ipp_tag_e_IPP_TAG_ENUM)
            .map_or(ProviderJobStatus::Unknown, |state| {
                ProviderJobStatus::from_ipp_job_state(state)
            }))
    }
}

/// Defence in depth for the test build.
///
/// Unit tests drive the lifecycle through a fake backend and never need this
/// one to mutate anything. If a test ever reaches it by mistake, it fails
/// here — before a printer is even looked up.
fn refuse_in_unit_tests() -> Result<(), MutationFailure> {
    if cfg!(test) {
        return Err(MutationFailure::NotSent(ProviderError::Environment {
            detail: "real CUPS job requests are disabled in unit tests".into(),
        }));
    }
    Ok(())
}

/// A scheduler-local queue, as the request needs it.
struct Target {
    uri: CString,
    resource: CString,
}

/// Find exactly `printer` on the local scheduler.
///
/// Refuses anything that would send the request elsewhere: a destination
/// whose URI is not a local `/printers/` or `/classes/` queue of this very
/// name is an error, never a fallback.
fn resolve(printer: &str) -> Result<Target, MutationFailure> {
    let destination = crate::get_destination(printer).map_err(|err| match err {
        Error::DestinationNotFound(_) => MutationFailure::NotSent(ProviderError::PrinterNotFound {
            printer: printer.to_string(),
        }),
        other => MutationFailure::NotSent(ProviderError::ConnectionFailed {
            detail: format!("could not look up {printer}: {other}"),
        }),
    })?;

    let uri = destination
        .options
        .get("printer-uri-supported")
        .ok_or_else(|| not_sent(&format!("{printer} has no printer-uri-supported")))?;
    let queue = queue_name(uri)
        .ok_or_else(|| not_sent(&format!("{printer} has an unusable printer URI {uri}")))?;
    if !queue.eq_ignore_ascii_case(printer) {
        return Err(not_sent(&format!(
            "{printer} resolved to a URI for {queue}; refusing to send elsewhere"
        )));
    }
    let resource = resource_path(uri).unwrap_or_default();
    Ok(Target {
        uri: CString::new(uri.as_str()).map_err(|_| not_sent("printer URI contains NUL"))?,
        resource: CString::new(resource).map_err(|_| not_sent("resource contains NUL"))?,
    })
}

/// The path of an `ipp://` or `ipps://` URI, without query or fragment.
fn resource_path(uri: &str) -> Option<&str> {
    let (scheme, rest) = uri.split_once("://")?;
    if !matches!(scheme, "ipp" | "ipps") {
        return None;
    }
    let path = &rest[rest.find('/')?..];
    Some(path.split(['?', '#']).next().unwrap_or(path))
}

/// The queue a printer or job URI names, e.g. `/printers/Canon_TS5400`.
fn queue_name(uri: &str) -> Option<String> {
    let path = resource_path(uri)?;
    let name = path
        .strip_prefix("/printers/")
        .or_else(|| path.strip_prefix("/classes/"))?;
    let decoded = percent_decode(name)?;
    (!decoded.is_empty() && !decoded.contains('/')).then_some(decoded)
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = input.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// IPP successful-* statuses are 0x0000–0x00FF. Anything in that range
/// means the operation was done, including "ok, but some attributes were
/// ignored" — treating only 0x0000 as success, as the crate's older job
/// helpers do, reports a created job as a failure and invites a second one.
fn is_successful(status: i64) -> bool {
    (0..0x0100).contains(&status)
}

fn rejection_reason(status: i64) -> JobRejection {
    let is = |code: bindings::ipp_status_e| status == i64::from(code);
    if is(bindings::ipp_status_e_IPP_STATUS_ERROR_NOT_FOUND)
        || is(bindings::ipp_status_e_IPP_STATUS_ERROR_GONE)
    {
        JobRejection::NotFound
    } else if is(bindings::ipp_status_e_IPP_STATUS_ERROR_NOT_ACCEPTING_JOBS) {
        JobRejection::NotAccepting
    } else if is(bindings::ipp_status_e_IPP_STATUS_ERROR_FORBIDDEN)
        || is(bindings::ipp_status_e_IPP_STATUS_ERROR_NOT_AUTHENTICATED)
        || is(bindings::ipp_status_e_IPP_STATUS_ERROR_NOT_AUTHORIZED)
    {
        JobRejection::NotAuthorized
    } else if is(bindings::ipp_status_e_IPP_STATUS_ERROR_NOT_POSSIBLE) {
        JobRejection::NotPossible
    } else if is(bindings::ipp_status_e_IPP_STATUS_ERROR_DOCUMENT_FORMAT_NOT_SUPPORTED) {
        JobRejection::DocumentFormatNotSupported
    } else if is(bindings::ipp_status_e_IPP_STATUS_ERROR_BAD_REQUEST)
        || is(bindings::ipp_status_e_IPP_STATUS_ERROR_ATTRIBUTES_OR_VALUES)
        || is(bindings::ipp_status_e_IPP_STATUS_ERROR_CONFLICTING)
    {
        JobRejection::BadRequest
    } else {
        JobRejection::Other
    }
}

fn rejected(status: i64) -> MutationFailure {
    let (_, message) = get_cups_error_details();
    MutationFailure::Rejected {
        reason: rejection_reason(status),
        detail: format!("scheduler answered 0x{status:04x}: {message}"),
    }
}

fn verdict(response: Ipp) -> Result<(), MutationFailure> {
    let status = response.status();
    if is_successful(status) {
        Ok(())
    } else {
        Err(rejected(status))
    }
}

fn no_answer(context: &str) -> MutationFailure {
    let (_, message) = get_cups_error_details();
    MutationFailure::NoAnswer {
        detail: format!("{context}: {message}"),
    }
}

fn not_sent(detail: &str) -> MutationFailure {
    MutationFailure::NotSent(ProviderError::InternalContractViolation {
        detail: detail.to_string(),
    })
}

/// Send a whole request and read the answer. `None` from libcups — the
/// request may or may not have been processed — becomes `NoAnswer`.
fn send(request: Ipp, resource: &CStr) -> Result<Ipp, MutationFailure> {
    // SAFETY: cupsDoRequest takes ownership of the request and frees it on
    // every path; `into_raw` gives up ours so it is not freed twice.
    let response =
        unsafe { bindings::cupsDoRequest(ptr::null_mut(), request.into_raw(), resource.as_ptr()) };
    Ipp::from_raw(response).ok_or_else(|| no_answer("no answer from the scheduler"))
}

/// Read the answer to a request whose body was streamed.
fn receive(resource: &CStr) -> Option<Ipp> {
    // SAFETY: the resource string outlives the call; the returned message,
    // if any, is owned by the caller.
    Ipp::from_raw(unsafe { bindings::cupsGetResponse(ptr::null_mut(), resource.as_ptr()) })
}

/// An owned IPP message, deleted on drop.
struct Ipp(*mut bindings::ipp_t);

impl Ipp {
    fn request(operation: bindings::ipp_op_t) -> Result<Self, MutationFailure> {
        // SAFETY: returns a new message or null.
        Self::from_raw(unsafe { bindings::ippNewRequest(operation) })
            .ok_or_else(|| not_sent("could not allocate IPP request"))
    }

    fn collection() -> Result<Self, MutationFailure> {
        // SAFETY: returns a new message or null.
        Self::from_raw(unsafe { bindings::ippNew() })
            .ok_or_else(|| not_sent("could not allocate IPP collection"))
    }

    fn from_raw(raw: *mut bindings::ipp_t) -> Option<Self> {
        (!raw.is_null()).then_some(Self(raw))
    }

    fn as_ptr(&self) -> *mut bindings::ipp_t {
        self.0
    }

    fn into_raw(self) -> *mut bindings::ipp_t {
        let raw = self.0;
        std::mem::forget(self);
        raw
    }

    fn checked(added: *mut bindings::ipp_attribute_t, name: &str) -> Result<(), MutationFailure> {
        if added.is_null() {
            Err(not_sent(&format!("could not add {name}")))
        } else {
            Ok(())
        }
    }

    fn add_string(
        &mut self,
        group: bindings::ipp_tag_t,
        tag: bindings::ipp_tag_t,
        name: &str,
        value: &CStr,
    ) -> Result<(), MutationFailure> {
        let c_name = c_string(name)?;
        // SAFETY: all strings are valid for the call; ippAddString copies them.
        let added = unsafe {
            bindings::ippAddString(
                self.0,
                group,
                tag,
                c_name.as_ptr(),
                ptr::null(),
                value.as_ptr(),
            )
        };
        Self::checked(added, name)
    }

    fn add_integer(
        &mut self,
        group: bindings::ipp_tag_t,
        tag: bindings::ipp_tag_t,
        name: &str,
        value: i32,
    ) -> Result<(), MutationFailure> {
        let c_name = c_string(name)?;
        // SAFETY: the name is valid for the call and copied.
        let added = unsafe { bindings::ippAddInteger(self.0, group, tag, c_name.as_ptr(), value) };
        Self::checked(added, name)
    }

    fn add_boolean(&mut self, name: &str, value: bool) -> Result<(), MutationFailure> {
        let c_name = c_string(name)?;
        // SAFETY: the name is valid for the call and copied.
        let added = unsafe {
            bindings::ippAddBoolean(
                self.0,
                bindings::ipp_tag_e_IPP_TAG_OPERATION,
                c_name.as_ptr(),
                ipp_boolean(value),
            )
        };
        Self::checked(added, name)
    }

    fn add_name(&mut self, name: &str, value: &str) -> Result<(), MutationFailure> {
        let value = c_string(value)?;
        self.add_string(
            bindings::ipp_tag_e_IPP_TAG_OPERATION,
            bindings::ipp_tag_e_IPP_TAG_NAME,
            name,
            &value,
        )
    }

    fn add_mimetype(&mut self, name: &str, value: &CStr) -> Result<(), MutationFailure> {
        self.add_string(
            bindings::ipp_tag_e_IPP_TAG_OPERATION,
            bindings::ipp_tag_e_IPP_TAG_MIMETYPE,
            name,
            value,
        )
    }

    /// `printer-uri` plus `requesting-user-name`, as libcups' own requests
    /// carry them.
    fn add_target(&mut self, target: &Target) -> Result<(), MutationFailure> {
        self.add_string(
            bindings::ipp_tag_e_IPP_TAG_OPERATION,
            bindings::ipp_tag_e_IPP_TAG_URI,
            "printer-uri",
            &target.uri,
        )?;
        self.add_user()
    }

    fn add_user(&mut self) -> Result<(), MutationFailure> {
        let user = cups_user();
        if user.is_null() {
            return Ok(());
        }
        // SAFETY: libcups returns a NUL-terminated string it owns.
        let user = unsafe { CStr::from_ptr(user) }.to_owned();
        self.add_string(
            bindings::ipp_tag_e_IPP_TAG_OPERATION,
            bindings::ipp_tag_e_IPP_TAG_NAME,
            "requesting-user-name",
            &user,
        )
    }

    fn add_job_id(&mut self, job: &ProviderJobId) -> Result<(), MutationFailure> {
        self.add_integer(
            bindings::ipp_tag_e_IPP_TAG_OPERATION,
            bindings::ipp_tag_e_IPP_TAG_INTEGER,
            "job-id",
            job.as_cups_id(),
        )
    }

    /// Address the job itself. cupsd ignores the host part of a job URI.
    fn add_job_uri(&mut self, job: &ProviderJobId) -> Result<(), MutationFailure> {
        let uri = c_string(&format!("ipp://localhost/jobs/{}", job.scheduler_job_id()))?;
        self.add_string(
            bindings::ipp_tag_e_IPP_TAG_OPERATION,
            bindings::ipp_tag_e_IPP_TAG_URI,
            "job-uri",
            &uri,
        )?;
        self.add_user()
    }

    fn add_attribute(
        &mut self,
        group: bindings::ipp_tag_t,
        attribute: &JobAttribute,
    ) -> Result<(), MutationFailure> {
        match &attribute.value {
            AttributeValue::Integer(value) => self.add_integer(
                group,
                bindings::ipp_tag_e_IPP_TAG_INTEGER,
                attribute.name,
                *value,
            ),
            AttributeValue::Enum(value) => self.add_integer(
                group,
                bindings::ipp_tag_e_IPP_TAG_ENUM,
                attribute.name,
                *value,
            ),
            AttributeValue::Keyword(value) => self.add_string(
                group,
                bindings::ipp_tag_e_IPP_TAG_KEYWORD,
                attribute.name,
                &c_string(value)?,
            ),
            AttributeValue::Collection(members) => {
                let mut collection = Self::collection()?;
                for member in members {
                    // Collection members carry no group of their own.
                    collection.add_attribute(bindings::ipp_tag_e_IPP_TAG_ZERO, member)?;
                }
                let c_name = c_string(attribute.name)?;
                // SAFETY: ippAddCollection takes a reference (use count +1)
                // rather than ownership, in both CUPS 2 and CUPS 3; dropping
                // `collection` afterwards releases only our reference.
                let added = unsafe {
                    bindings::ippAddCollection(self.0, group, c_name.as_ptr(), collection.0)
                };
                Self::checked(added, attribute.name)
            }
        }
    }

    fn status(&self) -> i64 {
        // SAFETY: self.0 is a valid message.
        i64::from(unsafe { bindings::ippGetStatusCode(self.0) })
    }

    fn find(&self, name: &str, tag: bindings::ipp_tag_t) -> Option<*mut bindings::ipp_attribute_t> {
        let c_name = CString::new(name).ok()?;
        // SAFETY: valid message and name; the attribute is owned by self.
        let attr = unsafe { bindings::ippFindAttribute(self.0, c_name.as_ptr(), tag) };
        (!attr.is_null()).then_some(attr)
    }

    fn integer(&self, name: &str, tag: bindings::ipp_tag_t) -> Option<i32> {
        let attr = self.find(name, tag)?;
        // SAFETY: attr belongs to self and has at least one value.
        Some(unsafe { bindings::ippGetInteger(attr, usize_to_count(0)) })
    }

    fn string(&self, name: &str, tag: bindings::ipp_tag_t) -> Option<String> {
        let attr = self.find(name, tag)?;
        // SAFETY: attr belongs to self; the string is copied before self drops.
        let value = unsafe { bindings::ippGetString(attr, usize_to_count(0), ptr::null_mut()) };
        if value.is_null() {
            return None;
        }
        Some(
            unsafe { CStr::from_ptr(value) }
                .to_string_lossy()
                .into_owned(),
        )
    }

    /// Names in the unsupported-attributes group: what the scheduler ignored
    /// or substituted.
    fn unsupported_attribute_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        // SAFETY: iteration stays within self; names are copied out.
        let mut attr = unsafe { ipp_first_attribute(self.0) };
        while !attr.is_null() {
            let group = unsafe { bindings::ippGetGroupTag(attr) };
            if group == bindings::ipp_tag_e_IPP_TAG_UNSUPPORTED_GROUP {
                let name = unsafe { bindings::ippGetName(attr) };
                if !name.is_null() {
                    names.push(
                        unsafe { CStr::from_ptr(name) }
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
            }
            attr = unsafe { ipp_next_attribute(self.0) };
        }
        names
    }
}

impl Drop for Ipp {
    fn drop(&mut self) {
        // SAFETY: we own one reference to a valid message.
        unsafe { bindings::ippDelete(self.0) };
    }
}

fn c_string(value: &str) -> Result<CString, MutationFailure> {
    CString::new(value).map_err(|_| not_sent("value contains NUL"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_is_the_uri_path() {
        assert_eq!(
            resource_path("ipp://localhost/printers/Brother_MFC_7460DN"),
            Some("/printers/Brother_MFC_7460DN")
        );
        assert_eq!(
            resource_path("ipps://host:631/classes/Office?waitjob=false"),
            Some("/classes/Office")
        );
        assert_eq!(resource_path("dnssd://Brother._ipp._tcp.local./"), None);
        assert_eq!(resource_path("ipp://localhost"), None);
    }

    #[test]
    fn queue_name_accepts_only_local_queue_paths() {
        assert_eq!(
            queue_name("ipp://localhost/printers/Canon_TS5400_series").as_deref(),
            Some("Canon_TS5400_series")
        );
        assert_eq!(
            queue_name("ipp://localhost:631/classes/Postcards").as_deref(),
            Some("Postcards")
        );
        assert_eq!(
            queue_name("ipp://localhost/printers/My%20Printer").as_deref(),
            Some("My Printer")
        );
        assert_eq!(queue_name("ipp://localhost/jobs/12"), None);
        assert_eq!(queue_name("ipp://localhost/printers/"), None);
        assert_eq!(queue_name("ipp://localhost/printers/a/b"), None);
        assert_eq!(queue_name("ipp://localhost/printers/bad%2"), None);
    }

    #[test]
    fn every_ipp_success_status_counts_as_done() {
        // 0x0001 successful-ok-ignored-or-substituted-attributes: the job
        // exists. Calling it a failure is how a duplicate job starts.
        for status in [0x0000, 0x0001, 0x0002, 0x0007, 0x00ff] {
            assert!(is_successful(status), "0x{status:04x}");
        }
        for status in [0x0100, 0x0400, 0x0406, 0x0500, -1] {
            assert!(!is_successful(status), "0x{status:04x}");
        }
    }

    #[test]
    fn rejections_map_by_meaning() {
        let reason = |code: bindings::ipp_status_e| rejection_reason(i64::from(code));
        assert_eq!(
            reason(bindings::ipp_status_e_IPP_STATUS_ERROR_NOT_FOUND),
            JobRejection::NotFound
        );
        assert_eq!(
            reason(bindings::ipp_status_e_IPP_STATUS_ERROR_NOT_ACCEPTING_JOBS),
            JobRejection::NotAccepting
        );
        assert_eq!(
            reason(bindings::ipp_status_e_IPP_STATUS_ERROR_NOT_AUTHORIZED),
            JobRejection::NotAuthorized
        );
        assert_eq!(
            reason(bindings::ipp_status_e_IPP_STATUS_ERROR_NOT_POSSIBLE),
            JobRejection::NotPossible
        );
        assert_eq!(
            reason(bindings::ipp_status_e_IPP_STATUS_ERROR_DOCUMENT_FORMAT_NOT_SUPPORTED),
            JobRejection::DocumentFormatNotSupported
        );
        assert_eq!(
            reason(bindings::ipp_status_e_IPP_STATUS_ERROR_ATTRIBUTES_OR_VALUES),
            JobRejection::BadRequest
        );
        assert_eq!(rejection_reason(0x0500), JobRejection::Other);
    }

    #[test]
    fn real_backend_refuses_every_mutation_in_unit_tests() {
        // Proves the guard fires before any lookup or request, so no unit
        // test can reach a printer through this backend.
        let mut backend = CupsJobBackend::default();
        let job = ProviderJobId::new("Any_Printer", 1).expect("valid");
        let refused = |result: Result<(), MutationFailure>| {
            matches!(result, Err(MutationFailure::NotSent(_)))
        };
        assert!(matches!(
            backend.create_job("Any_Printer", "t", &[]),
            Err(MutationFailure::NotSent(_))
        ));
        assert!(refused(backend.start_document(&job)));
        assert!(refused(backend.write_document(b"%PDF-")));
        assert!(refused(backend.finish_document(&job)));
        assert!(refused(backend.close_job(&job)));
        assert!(refused(backend.cancel_job(&job)));
    }
}
