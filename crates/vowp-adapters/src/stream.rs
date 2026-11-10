//! Incremental stream parsing contract shared by every harness adapter.
//! Spec: `docs/design/crates/vowp-adapters.md` §2 and `docs/design/adapters.md` §1.

use vowp_protocol::{HarnessEvent, RunSummary};

/// Incremental, process-free parser of one harness's stdout.
///
/// Adapters own one parser per run. The parser is pure: it performs no I/O and reads no
/// clock except the `now` value its constructor was given. [`feed_line`](StreamParser::feed_line)
/// is called for every stdout line (without the trailing newline) and may return zero or
/// more events; [`finish`](StreamParser::finish) consumes the parser after the child exits
/// and returns the trailing events plus the classified summary.
pub trait StreamParser: Send {
    /// Feeds one stdout line (no trailing newline), returning events in order.
    fn feed_line(&mut self, line: &str) -> Vec<HarnessEvent>;

    /// Feeds one stderr line; parsers that classify stderr keep a bounded tail.
    fn feed_stderr(&mut self, _line: &str) {}

    /// Ends the stream and consumes the parser.
    ///
    /// `exit` is `None` when the child was killed by a signal; `killed` is true when the
    /// caller requested stop/interrupt/timeout. Returns the trailing events
    /// (`Error`/`Limit`/`Cost`/`TurnEnded`) and the classified summary.
    fn finish(self: Box<Self>, exit: Option<i32>, killed: bool, stderr_tail: &str) -> (Vec<HarnessEvent>, RunSummary);
}

/// The result of parsing a whole recorded run (tests and the transcript importer).
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedRun {
    /// Normalised events in order.
    pub events: Vec<HarnessEvent>,
    /// Classified summary of the run.
    pub summary: RunSummary,
}
