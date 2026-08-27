//! Output discipline: what goes to stdout, what goes to stderr, and in what shape.
//!
//! # The stream contract
//!
//! Three rules, and every one of them exists because breaking it silently
//! corrupts someone's pipeline:
//!
//! 1. **stdout carries the result and nothing else.** Either exactly one JSON
//!    document, or human text, or raw image bytes — never a mixture. This is why
//!    `--stdout` conflicts with `--json` at the parser.
//! 2. **stderr carries logs, progress and guidance.** `tracing` is initialised
//!    with a stderr writer for exactly this reason. A debug log that lands in
//!    the middle of a PNG produces a corrupt file and a baffling bug report.
//! 3. **A JSON invocation always emits one document**, success or failure, so a
//!    script can parse stdout unconditionally rather than branching on the exit
//!    status before it knows what it has.
//!
//! # Schema stability
//!
//! Per decision D11 `--json` is a scripting contract and is treated as a public
//! API. The envelope carries an integer `schema` so a consumer can refuse a
//! version it does not understand, keys are ordered, and optional fields are
//! present-and-null rather than absent.

use std::io::{self, IsTerminal, Write};

use crate::{cli::GlobalArgs, fault::CliError, json::Json};

/// The version of the `--json` envelope.
///
/// Incremented only for a breaking change to the envelope itself. Adding a key
/// inside `data` is not breaking; removing or renaming one is.
pub const SCHEMA_VERSION: i64 = 1;

/// One command's result, in every representation the CLI can emit.
///
/// Commands build this and return; they never write to a stream themselves.
/// That keeps the stdout/stderr rules in one place and makes every command's
/// output testable as a value rather than by capturing file descriptors.
#[derive(Debug, Clone)]
pub struct Report {
    /// The `data` payload for `--json`.
    pub data: Json,
    /// The human-readable rendering.
    pub human: String,
    /// Raw bytes for `--stdout`, when the command produced an image or a video.
    pub raw: Option<Vec<u8>>,
}

impl Report {
    /// A report with no raw byte payload.
    pub fn new(data: Json, human: impl Into<String>) -> Self {
        Self {
            data,
            human: human.into(),
            raw: None,
        }
    }

    /// Attaches raw bytes to be written to stdout.
    #[must_use]
    pub fn with_raw(mut self, bytes: Vec<u8>) -> Self {
        self.raw = Some(bytes);
        self
    }
}

/// Wraps a successful payload in the stable envelope.
#[must_use]
pub fn success_envelope(command: &str, data: Json) -> Json {
    Json::obj([
        ("schema", Json::Int(SCHEMA_VERSION)),
        ("ok", Json::Bool(true)),
        ("command", Json::str(command)),
        ("data", data),
        ("error", Json::Null),
    ])
}

/// Wraps a failure in the stable envelope.
///
/// `data` is null and `error` is populated — the mirror image of
/// [`success_envelope`], so both documents have the same five keys in the same
/// order and a consumer can destructure without checking which it got.
#[must_use]
pub fn error_envelope(command: &str, err: &CliError) -> Json {
    Json::obj([
        ("schema", Json::Int(SCHEMA_VERSION)),
        ("ok", Json::Bool(false)),
        ("command", Json::str(command)),
        ("data", Json::Null),
        ("error", err.to_json()),
    ])
}

/// Renders reports and errors according to the stream contract.
#[derive(Debug, Clone, Copy)]
pub struct Reporter {
    json: bool,
    quiet: bool,
}

impl Reporter {
    /// Builds a reporter from the parsed global options.
    #[must_use]
    pub const fn from_global(global: &GlobalArgs) -> Self {
        Self {
            json: global.json,
            quiet: global.quiet,
        }
    }

    /// A reporter for tests and internal use.
    #[must_use]
    pub const fn new(json: bool, quiet: bool) -> Self {
        Self { json, quiet }
    }

    /// Whether machine-readable output was requested.
    #[must_use]
    pub const fn is_json(&self) -> bool {
        self.json
    }

    /// Writes a successful result.
    ///
    /// # Errors
    ///
    /// Returns an I/O error only for a genuine write failure. A closed
    /// downstream reader — `scrozz ... | head` — is not a failure and is
    /// swallowed, because the consumer got what it asked for.
    pub fn emit(&self, command: &str, report: &Report) -> io::Result<()> {
        if let Some(bytes) = &report.raw {
            // Raw mode: stdout is the payload. Anything else we might have said
            // moves to stderr so the byte stream stays exactly the file the
            // caller is piping into something.
            write_bytes(&mut io::stdout().lock(), bytes)?;
            if !self.quiet && !report.human.is_empty() {
                let _ = writeln!(io::stderr(), "{}", report.human.trim_end());
            }
            return Ok(());
        }

        if self.json {
            let document = success_envelope(command, report.data.clone());
            return write_line(&mut io::stdout().lock(), &document.to_compact_string());
        }

        if self.quiet || report.human.is_empty() {
            return Ok(());
        }
        write_line(&mut io::stdout().lock(), report.human.trim_end())
    }

    /// Writes a failure.
    ///
    /// Cancellation is silent in human mode: per
    /// [`scrozz_core::Error::Cancelled`] it is an outcome, not a fault, and
    /// printing "error" when someone pressed Escape teaches them the app is
    /// fragile. It is still reported in JSON, where a script needs to see it.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the diagnostic could not be written.
    pub fn emit_error(&self, command: &str, err: &CliError) -> io::Result<()> {
        if self.json {
            let document = error_envelope(command, err);
            return write_line(&mut io::stdout().lock(), &document.to_compact_string());
        }

        let text = err.to_human();
        if text.is_empty() {
            return Ok(());
        }
        // Errors are essential output, so `--quiet` does not suppress them. It
        // suppresses the chatter around a *successful* command.
        let mut stderr = io::stderr().lock();
        match stderr.write_all(text.as_bytes()) {
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
            other => other,
        }?;
        flush(&mut stderr)
    }

    /// Whether stdout is a terminal, for deciding on decoration.
    ///
    /// Never consulted for anything that changes the *content* of `--json`: a
    /// schema that varies with whether a human is watching is not a contract.
    #[must_use]
    pub fn stdout_is_terminal(&self) -> bool {
        io::stdout().is_terminal()
    }
}

fn write_line(out: &mut impl Write, line: &str) -> io::Result<()> {
    match writeln!(out, "{line}") {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(()),
        other => other,
    }?;
    flush(out)
}

fn write_bytes(out: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    match out.write_all(bytes) {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(()),
        other => other,
    }?;
    flush(out)
}

fn flush(out: &mut impl Write) -> io::Result<()> {
    match out.flush() {
        // `scrozz capture --stdout | head -c 8` closes the pipe early. That is
        // the caller doing something reasonable, not an error to report.
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use scrozz_core::Error as CoreError;

    use super::*;

    fn keys(value: &Json) -> Vec<String> {
        let Json::Obj(pairs) = value else {
            panic!("expected an object")
        };
        pairs.iter().map(|(k, _)| k.clone()).collect()
    }

    #[test]
    fn the_success_envelope_is_pinned() {
        let document = success_envelope("list.displays", Json::obj([("displays", Json::arr([]))]));
        assert_eq!(
            document.to_compact_string(),
            r#"{"schema":1,"ok":true,"command":"list.displays","data":{"displays":[]},"error":null}"#
        );
    }

    #[test]
    fn the_error_envelope_is_pinned() {
        let err = CliError::Core(CoreError::Cancelled);
        assert_eq!(
            error_envelope("capture", &err).to_compact_string(),
            r#"{"schema":1,"ok":false,"command":"capture","data":null,"error":{"kind":"cancelled","code":3,"message":"cancelled by user","cancelled":true,"actionable":false,"details":{}}}"#
        );
    }

    #[test]
    fn success_and_failure_have_identical_shapes() {
        // A consumer must be able to read `.schema`, `.ok`, `.command`, `.data`
        // and `.error` without first knowing which kind of document it holds.
        let ok = success_envelope("capture", Json::Obj(vec![]));
        let bad = error_envelope("capture", &CliError::usage("x"));
        assert_eq!(keys(&ok), keys(&bad));
        assert_eq!(keys(&ok), ["schema", "ok", "command", "data", "error"]);
    }

    #[test]
    fn the_schema_version_is_reported_as_an_integer() {
        let document = success_envelope("gui", Json::Obj(vec![]));
        let Json::Obj(pairs) = &document else {
            panic!()
        };
        assert_eq!(pairs[0].1, Json::Int(1));
    }

    #[test]
    fn ok_is_false_for_cancellation_even_though_it_is_not_a_fault() {
        // `ok` means "the command did what was asked". Cancellation did not, so
        // it is false — and `error.cancelled` is how a script tells the two
        // apart without matching on strings.
        let document = error_envelope("capture", &CliError::Core(CoreError::Cancelled));
        let Json::Obj(pairs) = &document else {
            panic!()
        };
        assert_eq!(pairs[1].1, Json::Bool(false));

        let Some((_, Json::Obj(error))) = pairs.iter().find(|(k, _)| k == "error") else {
            panic!("expected an error object")
        };
        let cancelled = error
            .iter()
            .find(|(k, _)| k == "cancelled")
            .map(|(_, v)| v.clone());
        assert_eq!(cancelled, Some(Json::Bool(true)));
    }

    #[test]
    fn every_envelope_is_exactly_one_line() {
        let long_title = "a window\nwith a newline\tand a tab";
        let document = success_envelope(
            "list.windows",
            Json::obj([("windows", Json::arr([Json::str(long_title)]))]),
        );
        let rendered = document.to_compact_string();
        assert!(
            !rendered.contains('\n'),
            "a newline in the payload escaped into the document"
        );
    }

    #[test]
    fn command_slugs_reach_the_envelope_verbatim() {
        for slug in [
            "capture",
            "record.stop",
            "list.displays",
            "history.pin",
            "hotkey.generate-config",
        ] {
            let document = success_envelope(slug, Json::Obj(vec![])).to_compact_string();
            assert!(
                document.contains(&format!(r#""command":"{slug}""#)),
                "{slug}"
            );
        }
    }

    #[test]
    fn a_report_carries_both_representations() {
        let report = Report::new(Json::obj([("n", Json::Int(1))]), "one");
        assert_eq!(report.human, "one");
        assert!(report.raw.is_none());

        let with_bytes = report.with_raw(vec![0x89, b'P', b'N', b'G']);
        assert_eq!(
            with_bytes.raw.as_deref(),
            Some(&[0x89, b'P', b'N', b'G'][..])
        );
    }

    #[test]
    fn a_broken_pipe_is_not_a_failure() {
        // A writer that always reports EPIPE, as `| head` produces.
        struct ClosedPipe;
        impl Write for ClosedPipe {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
            }
        }
        assert!(write_line(&mut ClosedPipe, "hello").is_ok());
        assert!(write_bytes(&mut ClosedPipe, b"hello").is_ok());
    }

    #[test]
    fn a_real_write_failure_still_surfaces() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::StorageFull))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert!(write_line(&mut Broken, "hello").is_err());
    }

    #[test]
    fn reporter_modes_are_readable() {
        assert!(Reporter::new(true, false).is_json());
        assert!(!Reporter::new(false, true).is_json());
    }
}
