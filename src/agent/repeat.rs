//! Flags degenerate output where a model repeats the same line/paragraph
//! over and over without making progress - a real failure mode observed with
//! some local models (via llama.cpp-based backends) that get stuck reasoning
//! in circles in plain text instead of calling a tool or giving a final
//! answer. Left unchecked, that burns tokens/time indefinitely (or until the
//! provider's own generation limit kicks in).
//!
//! Detection is line/paragraph-based: it tracks the last few non-trivial
//! lines and flags a loop once one of them recurs often enough within that
//! window. This catches both a line repeated back-to-back and small cycles
//! (e.g. two alternating sentences), which is what's been observed in
//! practice; it will not catch repetition confined to a single line/
//! paragraph with no newlines at all.
//!
//! Port of Go `internal/agent/repeat.go`.

const MIN_CHUNK_LEN: usize = 20; // ignore short lines (blank lines, "---", bullets) that would trivially "repeat"
const WINDOW: usize = 8; // how many recent non-trivial lines are remembered
const THRESHOLD: usize = 3; // how many recurrences before it's treated as a loop

#[derive(Default)]
pub struct RepeatDetector {
    buf: String,
    recent: Vec<String>,
}

impl RepeatDetector {
    /// Feeds one streamed delta and reports whether it just completed a line
    /// that pushes some recent line's count to the threshold.
    pub fn add(&mut self, delta: &str) -> bool {
        self.buf.push_str(delta);
        let mut detected = false;
        loop {
            let Some(pos) = self.buf.find('\n') else {
                break;
            };
            let chunk = self.buf[..pos].trim().to_string();
            self.buf.drain(..pos + 1);
            if chunk.len() < MIN_CHUNK_LEN {
                continue;
            }

            self.recent.push(chunk.clone());
            if self.recent.len() > WINDOW {
                self.recent.remove(0);
            }

            let count = self.recent.iter().filter(|c| **c == chunk).count();
            if count >= THRESHOLD {
                detected = true;
                break;
            }
        }
        detected
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_repeated_lines() {
        let mut d = RepeatDetector::default();
        let line = "I should carefully consider the architecture of this system.\n";
        assert!(!d.add(line));
        assert!(!d.add(line));
        assert!(d.add(line));
    }

    #[test]
    fn detects_alternating_cycle() {
        // Mirrors Go's TestRepeatDetectorCatchesAlternatingSentenceLoop: a
        // model alternating between two sentences forever. The third
        // occurrence of either line (the 6th line overall) is the loop.
        let mut d = RepeatDetector::default();
        let a = "First repeating thought about the task at hand.\n";
        let b = "Second repeating thought about the task at hand.\n";
        assert!(!d.add(a));
        assert!(!d.add(b));
        assert!(!d.add(a));
        assert!(!d.add(b)); // each line seen twice so far
        assert!(d.add(a)); // a now seen 3x within the window
    }

    #[test]
    fn ignores_short_and_distinct_lines() {
        let mut d = RepeatDetector::default();
        assert!(!d.add("---\n---\n---\n---\n"));
        for i in 0..10 {
            assert!(!d.add(&format!(
                "a distinct line number {} with plenty of length to matter\n",
                i
            )));
        }
    }

    #[test]
    fn partial_lines_accumulate_across_deltas() {
        let mut d = RepeatDetector::default();
        let line = "the exact same long line repeats over and over here\n";
        for _ in 0..2 {
            // split mid-line across deltas
            let (a, b) = line.split_at(25);
            assert!(!d.add(a));
            assert!(!d.add(b));
        }
        let (a, b) = line.split_at(25);
        d.add(a);
        assert!(d.add(b));
    }
}
