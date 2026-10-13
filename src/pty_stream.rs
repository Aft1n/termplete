//! Incremental parser for hidden OSC frames in the interactive zsh PTY stream.

use std::mem;

use serde::Deserialize;

use super::{Captured, MAX_CAPTURE_OUTPUT_BYTES, OSC_FRAME_PREFIX};

const FRAME_PREFIX: &[u8] = OSC_FRAME_PREFIX.as_bytes();
const BEL: u8 = 0x07;
const ESC: u8 = 0x1b;
const MAX_FRAME_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub(super) struct StreamParser {
    pending: Vec<u8>,
    active: Option<Frame>,
    token: String,
    output_limit: usize,
}

#[derive(Debug)]
struct Frame {
    command: String,
    cwd: Option<String>,
    output: Vec<u8>,
    output_limit: usize,
}

#[derive(Debug, Default)]
pub(super) struct Update {
    pub display: Vec<u8>,
    pub captured: Vec<Captured>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "event")]
enum Event {
    #[serde(rename = "start")]
    Start {
        token: String,
        command: String,
        #[serde(default)]
        cwd: Option<String>,
    },
    #[serde(rename = "end")]
    End { code: i32, token: String },
}

impl Event {
    fn token(&self) -> &str {
        match self {
            Self::Start { token, .. } | Self::End { token, .. } => token,
        }
    }
}

impl StreamParser {
    #[cfg(test)]
    pub(super) fn new(token: String) -> Self {
        Self::with_output_limit(token, MAX_CAPTURE_OUTPUT_BYTES)
    }

    pub(super) fn with_output_limit(token: String, output_limit: usize) -> Self {
        Self {
            pending: Vec::new(),
            active: None,
            token,
            output_limit: output_limit.min(MAX_CAPTURE_OUTPUT_BYTES),
        }
    }

    pub(super) fn feed(&mut self, bytes: &[u8]) -> Update {
        self.pending.extend_from_slice(bytes);
        let mut update = Update::default();
        let mut cursor = 0;

        while cursor < self.pending.len() {
            let tail = &self.pending[cursor..];
            if tail[0] != ESC {
                let count = tail
                    .iter()
                    .position(|byte| *byte == ESC)
                    .unwrap_or(tail.len());
                Self::pass(&mut self.active, &tail[..count], &mut update.display);
                cursor += count;
                continue;
            }

            // Hold a possible prefix until the next PTY read so a marker split
            // across chunks can be recognized without delaying ordinary text.
            if tail.len() < FRAME_PREFIX.len() && FRAME_PREFIX.starts_with(tail) {
                break;
            }

            if !tail.starts_with(FRAME_PREFIX) {
                Self::pass(&mut self.active, &tail[..1], &mut update.display);
                cursor += 1;
                continue;
            }

            let payload_start = cursor + FRAME_PREFIX.len();
            let Some((terminator_at, terminator_len)) =
                find_terminator(&self.pending, payload_start)
            else {
                if self.pending.len() - cursor - FRAME_PREFIX.len() > MAX_FRAME_BYTES {
                    // A user program may print a long OSC that happens to begin
                    // with our namespace. Treat an oversized sequence as text
                    // instead of retaining it forever.
                    Self::pass(
                        &mut self.active,
                        &self.pending[cursor..cursor + 1],
                        &mut update.display,
                    );
                    cursor += 1;
                    continue;
                }
                break;
            };

            let end = terminator_at + terminator_len;
            let event =
                serde_json::from_slice::<Event>(&self.pending[payload_start..terminator_at])
                    .ok()
                    .filter(|event| event.token() == self.token);
            if let Some(event) = event {
                Self::apply(
                    &mut self.active,
                    event,
                    &mut update.captured,
                    self.output_limit,
                );
            } else {
                // Only valid termplete records are hidden. Other OSC payloads,
                // including malformed lookalikes, remain visible and capturable.
                Self::pass(
                    &mut self.active,
                    &self.pending[cursor..end],
                    &mut update.display,
                );
            }
            cursor = end;
        }

        if cursor > 0 {
            self.pending.drain(..cursor);
        }
        update
    }

    pub(super) fn finish(&mut self) -> Update {
        let pending = mem::take(&mut self.pending);
        let mut update = Update {
            display: pending.clone(),
            captured: Vec::new(),
        };
        Self::capture(&mut self.active, &pending);
        update.display.shrink_to_fit();
        update
    }

    fn pass(active: &mut Option<Frame>, bytes: &[u8], display: &mut Vec<u8>) {
        display.extend_from_slice(bytes);
        Self::capture(active, bytes);
    }

    fn capture(active: &mut Option<Frame>, bytes: &[u8]) {
        let Some(frame) = active else {
            return;
        };
        let limit = frame.output_limit;
        if limit == 0 || bytes.is_empty() {
            return;
        }
        if bytes.len() >= limit {
            frame.output.clear();
            frame
                .output
                .extend_from_slice(&bytes[bytes.len() - limit..]);
            return;
        }
        let overflow = frame
            .output
            .len()
            .saturating_add(bytes.len())
            .saturating_sub(limit);
        if overflow > 0 {
            frame.output.drain(..overflow);
        }
        frame.output.extend_from_slice(bytes);
    }

    fn apply(
        active: &mut Option<Frame>,
        event: Event,
        captured: &mut Vec<Captured>,
        output_limit: usize,
    ) {
        match event {
            Event::Start { command, cwd, .. } => {
                // A fresh start means an earlier end record was lost. Discard
                // that incomplete frame rather than mixing commands together.
                *active = Some(Frame {
                    command,
                    cwd,
                    output: Vec::with_capacity(output_limit.min(8192)),
                    output_limit,
                });
            }
            Event::End { code, .. } => {
                if let Some(frame) = active.take() {
                    let output = String::from_utf8_lossy(&frame.output);
                    captured.push(Captured {
                        command: frame.command,
                        cwd: frame.cwd,
                        exit_code: Some(code),
                        output: truncate_output_tail(&output, frame.output_limit),
                    });
                }
            }
        }
    }
}

pub(super) fn truncate_output_tail(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut start = value.len() - max_bytes;
    while !value.is_char_boundary(start) {
        start += 1;
    }
    value[start..].to_owned()
}

fn find_terminator(bytes: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            BEL => return Some((i, 1)),
            ESC if bytes.get(i + 1) == Some(&b'\\') => return Some((i, 2)),
            _ => i += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty::{frame_end, frame_start};

    const TEST_TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn drain_chunks(parser: &mut StreamParser, bytes: &[u8], chunk_size: usize) -> Update {
        let mut all = Update::default();
        for chunk in bytes.chunks(chunk_size) {
            let update = parser.feed(chunk);
            all.display.extend(update.display);
            all.captured.extend(update.captured);
        }
        all
    }

    #[test]
    fn exact_start_and_end_frames_capture_only_command_output() {
        let command = "printf 'hello\\n'";
        let start = frame_start(command, "/work/tree", TEST_TOKEN);
        let end = frame_end(7, TEST_TOKEN);
        let input = format!("prompt ghost {command}\r\n{start}hello\r\n{end}next prompt");

        let mut parser = StreamParser::new(TEST_TOKEN.into());
        let mut update = drain_chunks(&mut parser, input.as_bytes(), 1);
        let tail = parser.finish();
        update.display.extend(tail.display);
        update.captured.extend(tail.captured);

        assert_eq!(
            String::from_utf8(update.display).unwrap(),
            format!("prompt ghost {command}\r\nhello\r\nnext prompt")
        );
        assert_eq!(update.captured.len(), 1);
        assert_eq!(update.captured[0].command, command);
        assert_eq!(update.captured[0].cwd.as_deref(), Some("/work/tree"));
        assert_eq!(update.captured[0].exit_code, Some(7));
        assert_eq!(update.captured[0].output, "hello\r\n");
    }

    #[test]
    fn preserves_csi_and_unrelated_osc_sequences() {
        let start = frame_start("printf x", "/tmp", TEST_TOKEN);
        let end = frame_end(0, TEST_TOKEN);
        let visible = b"\x1b[31mred\x1b[0m\x1b]0;window title\x07";
        let mut input = start.into_bytes();
        input.extend_from_slice(visible);
        input.extend_from_slice(end.as_bytes());

        let mut parser = StreamParser::new(TEST_TOKEN.into());
        let update = parser.feed(&input);
        assert_eq!(update.display, visible);
        assert_eq!(update.captured[0].output.as_bytes(), visible);
    }

    #[test]
    fn multiple_frames_in_one_chunk_stay_separate() {
        let input = format!(
            "{}one{}{}two{}",
            frame_start("first", "/a", TEST_TOKEN),
            frame_end(0, TEST_TOKEN),
            frame_start("second", "/b", TEST_TOKEN),
            frame_end(2, TEST_TOKEN)
        );
        let mut parser = StreamParser::new(TEST_TOKEN.into());
        let update = parser.feed(input.as_bytes());
        assert_eq!(update.display, b"onetwo");
        assert_eq!(update.captured.len(), 2);
        assert_eq!(update.captured[0].command, "first");
        assert_eq!(update.captured[0].output, "one");
        assert_eq!(update.captured[1].command, "second");
        assert_eq!(update.captured[1].output, "two");
        assert_eq!(update.captured[1].exit_code, Some(2));
    }

    #[test]
    fn only_valid_termplete_records_are_hidden() {
        let malformed = b"\x1b]777;termplete;not-json\x07";
        let unknown = b"\x1b]777;termplete;{\"event\":\"other\"}\x07";
        let mut parser = StreamParser::new(TEST_TOKEN.into());
        let update = parser.feed(malformed);
        let next = parser.feed(unknown);
        assert_eq!(update.display, malformed);
        assert_eq!(next.display, unknown);
    }

    #[test]
    fn incomplete_frame_prefix_is_flushed_at_eof() {
        let partial = b"visible\x1b]777;termplete;";
        let mut parser = StreamParser::new(TEST_TOKEN.into());
        let update = parser.feed(partial);
        assert_eq!(update.display, b"visible");
        let tail = parser.finish();
        assert_eq!(tail.display, b"\x1b]777;termplete;");
        assert!(tail.captured.is_empty());
    }

    #[test]
    fn output_is_bounded_without_truncating_display() {
        let start = frame_start("big", "/tmp", TEST_TOKEN);
        let end = frame_end(0, TEST_TOKEN);
        let mut output = vec![b'x'; MAX_CAPTURE_OUTPUT_BYTES + 100];
        let final_header = b"OMP_SESSION_UUID=0123456789abcdef";
        output.extend_from_slice(final_header);
        let mut input = start.into_bytes();
        input.extend_from_slice(&output);
        input.extend_from_slice(end.as_bytes());

        let mut parser = StreamParser::new(TEST_TOKEN.into());
        let mut display = Vec::new();
        let mut captured = Vec::new();
        for chunk in input.chunks(4096) {
            let update = parser.feed(chunk);
            display.extend(update.display);
            captured.extend(update.captured);
        }
        assert_eq!(display, output);
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].output.len(), MAX_CAPTURE_OUTPUT_BYTES);
        assert!(captured[0]
            .output
            .ends_with(std::str::from_utf8(final_header).unwrap()));
    }

    #[test]
    fn configured_capture_keeps_the_session_tail_without_changing_display() {
        let start = frame_start("long-running", "/tmp", TEST_TOKEN);
        let end = frame_end(0, TEST_TOKEN);
        let final_header = "OMP_SESSION_UUID=resume-0123456789abcdef ready ✅";
        let mut output = vec![b'x'; 4096];
        output.extend_from_slice(final_header.as_bytes());
        let mut input = start.into_bytes();
        input.extend_from_slice(&output);
        input.extend_from_slice(end.as_bytes());

        let mut parser = StreamParser::with_output_limit(TEST_TOKEN.into(), 96);
        let mut update = drain_chunks(&mut parser, &input, 3);
        let tail = parser.finish();
        update.display.extend(tail.display);
        update.captured.extend(tail.captured);

        assert_eq!(update.display, output);
        assert_eq!(update.captured.len(), 1);
        assert!(update.captured[0].output.len() <= 96);
        assert!(update.captured[0].output.ends_with(final_header));
    }

    #[test]
    fn truncated_output_stays_on_utf8_boundaries() {
        let start = frame_start("unicode-tail", "/tmp", TEST_TOKEN);
        let end = frame_end(0, TEST_TOKEN);
        let output = "abcdefghé🙂";
        let mut input = start.into_bytes();
        input.extend_from_slice(output.as_bytes());
        input.extend_from_slice(end.as_bytes());

        let mut parser = StreamParser::with_output_limit(TEST_TOKEN.into(), 5);
        let update = drain_chunks(&mut parser, &input, 1);
        assert_eq!(update.captured.len(), 1);
        assert_eq!(update.captured[0].output, "🙂");
        assert!(update.captured[0].output.len() <= 5);
    }

    #[test]
    fn zero_output_limit_keeps_display_but_does_not_capture() {
        let output = b"terminal output stays visible";
        let input = format!(
            "{}{}{}",
            frame_start("quiet", "/tmp", TEST_TOKEN),
            std::str::from_utf8(output).unwrap(),
            frame_end(0, TEST_TOKEN)
        );
        let mut parser = StreamParser::with_output_limit(TEST_TOKEN.into(), 0);
        let update = parser.feed(input.as_bytes());
        assert_eq!(update.display, output);
        assert_eq!(update.captured.len(), 1);
        assert!(update.captured[0].output.is_empty());
    }

    #[test]
    fn a_new_start_discards_an_incomplete_previous_frame() {
        let input = format!(
            "{}stale{}fresh{}",
            frame_start("old", "/old", TEST_TOKEN),
            frame_start("new", "/new", TEST_TOKEN),
            frame_end(0, TEST_TOKEN)
        );
        let mut parser = StreamParser::new(TEST_TOKEN.into());
        let update = parser.feed(input.as_bytes());
        assert_eq!(update.captured.len(), 1);
        assert_eq!(update.captured[0].command, "new");
        assert_eq!(update.captured[0].output, "fresh");
    }
}
