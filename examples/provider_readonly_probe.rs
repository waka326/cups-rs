//! Read-only capability probe against the local CUPS scheduler.
//!
//! Exercises the provider contract on real queues and prints what came back.
//! It imports nothing from `cups_rs::job`, so it cannot create, submit or
//! cancel a print job.

use std::time::Duration;

use cups_rs::provider::{
    AttributeState, CupsProvider, IppValue, MediaQuery, ReadyMedia, POSTCARD_LENGTH_UM,
    POSTCARD_WIDTH_UM,
};

/// Generous enough that a slow queue answers rather than reporting a timeout
/// we would have to interpret. This is a probe setting, not a product default.
const TIMEOUT: Duration = Duration::from_secs(30);

const CAPABILITY_ATTRIBUTES: &[&str] = &[
    "sides",
    "print-color-mode",
    "print-quality",
    "media-type",
    "media-source",
    "copies",
    "orientation-requested",
];

fn main() {
    println!("=== ENV ===");
    for key in ["LANG", "LC_ALL", "LC_CTYPE"] {
        println!(
            "{key}={}",
            std::env::var(key).unwrap_or_else(|_| "<unset>".into())
        );
    }

    let provider = match CupsProvider::new() {
        Ok(provider) => provider,
        Err(err) => {
            println!("FATAL: {err}");
            std::process::exit(1);
        }
    };

    let printers = match provider.list_printers(TIMEOUT) {
        Ok(printers) => printers,
        Err(err) => {
            println!("FATAL: list_printers: {err}");
            std::process::exit(1);
        }
    };

    println!("\n=== DESTINATION COUNT: {} ===", printers.len());

    for printer in &printers {
        println!("\n================================================================");
        println!("NAME: {}", printer.name);
        println!("FULL_NAME: {}", printer.full_name);
        println!("IS_DEFAULT: {}", printer.is_default);
        println!("STATE: {:?}", printer.state);
        println!("ACCEPTING_JOBS: {}", printer.accepting_jobs);
        println!("STATE_REASONS: {:?}", printer.state_reasons);
        println!("PRINTER_INFO: {:?}", printer.info);
        println!("PRINTER_LOCATION: {:?}", printer.location);
        println!("PRINTER_MAKE_AND_MODEL: {:?}", printer.make_and_model);
        println!("PRINTER_URI_SUPPORTED: {:?}", printer.printer_uri);
        println!("DEVICE_URI: {:?}", printer.device_uri);

        for attribute in CAPABILITY_ATTRIBUTES {
            match provider.get_supported_values(&printer.name, attribute, TIMEOUT) {
                Ok(state) => println!("SUPPORTED[{attribute}]: {}", render(&state)),
                Err(err) => println!("SUPPORTED[{attribute}]: ERROR {err}"),
            }
            match provider.get_default_value(&printer.name, attribute, TIMEOUT) {
                Ok(state) => println!("DEFAULT[{attribute}]: {}", render(&state)),
                Err(err) => println!("DEFAULT[{attribute}]: ERROR {err}"),
            }
        }

        // The two regressions this probe exists to catch.
        match provider.get_supported_copies(&printer.name, TIMEOUT) {
            Ok(state) => {
                println!("COPIES_ACCEPTS_1: {}", state.accepts_integer(1));
                println!("COPIES_RAW: {}", render(&state));
            }
            Err(err) => println!("COPIES: ERROR {err}"),
        }
        match provider.get_default_value(&printer.name, "orientation-requested", TIMEOUT) {
            Ok(state) => {
                let is_no_value = state
                    .values()
                    .is_some_and(|values| values.contains(&IppValue::NoValue));
                println!("ORIENTATION_DEFAULT_IS_NO_VALUE: {is_no_value}");
            }
            Err(err) => println!("ORIENTATION_DEFAULT: ERROR {err}"),
        }

        for (label, query) in [
            ("DEFAULT", MediaQuery::Default),
            ("BORDERLESS", MediaQuery::Borderless),
            ("EXACT", MediaQuery::Exact),
        ] {
            match provider.get_media_by_size(
                &printer.name,
                POSTCARD_WIDTH_UM,
                POSTCARD_LENGTH_UM,
                query,
                TIMEOUT,
            ) {
                Ok(media) => println!(
                    "POSTCARD_100x148[{label}]: name={} w={}um l={}um \
                     margins(t/b/l/r)={}/{}/{}/{}um borderless={} is_postcard={}",
                    media.canonical_name,
                    media.width_um,
                    media.length_um,
                    media.margins.top_um,
                    media.margins.bottom_um,
                    media.margins.left_um,
                    media.margins.right_um,
                    media.borderless,
                    media.is_postcard_100x148(),
                ),
                Err(err) => println!("POSTCARD_100x148[{label}]: {err}"),
            }
        }

        match provider.get_supported_media(&printer.name, MediaQuery::Default, TIMEOUT) {
            Ok(media) => {
                println!("ALL_MEDIA ({}):", media.len());
                for size in &media {
                    println!(
                        "  {} w={}um l={}um",
                        size.canonical_name, size.width_um, size.length_um
                    );
                }
            }
            Err(err) => println!("ALL_MEDIA: ERROR {err}"),
        }

        match provider.get_ready_media(&printer.name, TIMEOUT) {
            Ok(ReadyMedia::NotAdvertised) => {
                println!("READY_MEDIA: NotAdvertised (loaded media is unknown, NOT empty)")
            }
            Ok(ReadyMedia::Present(media)) => {
                println!("READY_MEDIA: Present ({} entries)", media.len());
                for size in &media {
                    println!(
                        "  {} w={}um l={}um",
                        size.canonical_name, size.width_um, size.length_um
                    );
                }
            }
            Err(err) => println!("READY_MEDIA: ERROR {err}"),
        }
    }
}

fn render(state: &AttributeState<IppValue>) -> String {
    match state {
        AttributeState::NotAdvertised => "NotAdvertised".to_string(),
        AttributeState::Empty => "Empty".to_string(),
        AttributeState::Present(values) => format!("Present({values:?})"),
    }
}
