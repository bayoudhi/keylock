//! Output filter. Tracks escape-sequence state in the command's output so
//! keylock can safely insert its own sequences, and prefixes window titles
//! with a lock while the session is locked.

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const MAX_OSC: usize = 8192;

pub const LOCK_PREFIX: &str = "🔒 ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Esc,
    Csi,
    Osc,
    OscEsc,
    /// DCS, APC, PM or SOS string, passed through until ST.
    Str,
    StrEsc,
    /// Continuation bytes still expected for a UTF-8 character.
    Utf8(u8),
}

#[derive(Debug)]
pub struct TitleFilter {
    state: State,
    osc: Vec<u8>,
    last_title: Option<Vec<u8>>,
}

impl Default for TitleFilter {
    fn default() -> Self {
        TitleFilter {
            state: State::Ground,
            osc: Vec::new(),
            last_title: None,
        }
    }
}

impl TitleFilter {
    pub fn filter(&mut self, chunk: &[u8], locked: bool) -> Vec<u8> {
        let mut out = Vec::with_capacity(chunk.len() + 8);
        for &byte in chunk {
            self.step(byte, locked, &mut out);
        }
        out
    }

    pub fn in_sequence(&self) -> bool {
        self.state != State::Ground
    }

    pub fn last_title(&self) -> Option<&[u8]> {
        self.last_title.as_deref()
    }

    fn step(&mut self, byte: u8, locked: bool, out: &mut Vec<u8>) {
        match self.state {
            State::Ground => match byte {
                ESC => self.state = State::Esc,
                0xc0..=0xdf => self.utf8_lead(byte, 1, out),
                0xe0..=0xef => self.utf8_lead(byte, 2, out),
                0xf0..=0xf7 => self.utf8_lead(byte, 3, out),
                _ => out.push(byte),
            },
            State::Utf8(remaining) => {
                if (0x80..=0xbf).contains(&byte) {
                    out.push(byte);
                    self.state = if remaining == 1 {
                        State::Ground
                    } else {
                        State::Utf8(remaining - 1)
                    };
                } else {
                    self.state = State::Ground;
                    self.step(byte, locked, out);
                }
            }
            State::Esc => match byte {
                b']' => {
                    self.osc.clear();
                    self.state = State::Osc;
                }
                b'[' => {
                    out.extend_from_slice(&[ESC, byte]);
                    self.state = State::Csi;
                }
                b'P' | b'_' | b'^' | b'X' => {
                    out.extend_from_slice(&[ESC, byte]);
                    self.state = State::Str;
                }
                ESC => out.push(ESC),
                _ => {
                    out.extend_from_slice(&[ESC, byte]);
                    self.state = State::Ground;
                }
            },
            State::Csi => {
                out.push(byte);
                if (0x40..=0x7e).contains(&byte) {
                    self.state = State::Ground;
                }
            }
            State::Osc => match byte {
                BEL => self.finish_osc(&[BEL], locked, out),
                ESC => self.state = State::OscEsc,
                _ if self.osc.len() >= MAX_OSC => {
                    self.abandon_osc(out);
                    self.state = State::Ground;
                    self.step(byte, locked, out);
                }
                _ => self.osc.push(byte),
            },
            State::OscEsc => {
                if byte == b'\\' {
                    self.finish_osc(&[ESC, b'\\'], locked, out);
                } else {
                    self.abandon_osc(out);
                    self.state = State::Esc;
                    self.step(byte, locked, out);
                }
            }
            State::Str => {
                out.push(byte);
                if byte == ESC {
                    self.state = State::StrEsc;
                }
            }
            State::StrEsc => {
                out.push(byte);
                self.state = match byte {
                    b'\\' => State::Ground,
                    ESC => State::StrEsc,
                    _ => State::Str,
                };
            }
        }
    }

    fn utf8_lead(&mut self, byte: u8, continuation: u8, out: &mut Vec<u8>) {
        out.push(byte);
        self.state = State::Utf8(continuation);
    }

    /// Writes the buffered OSC unchanged, without a terminator.
    fn abandon_osc(&mut self, out: &mut Vec<u8>) {
        out.extend_from_slice(&[ESC, b']']);
        out.append(&mut self.osc);
    }

    fn finish_osc(&mut self, terminator: &[u8], locked: bool, out: &mut Vec<u8>) {
        let osc = std::mem::take(&mut self.osc);
        self.state = State::Ground;
        out.extend_from_slice(&[ESC, b']']);
        let title = osc.strip_prefix(b"0;").or_else(|| osc.strip_prefix(b"2;"));
        match title {
            Some(title) => {
                self.last_title = Some(title.to_vec());
                out.extend_from_slice(&osc[..2]);
                if locked {
                    out.extend_from_slice(LOCK_PREFIX.as_bytes());
                }
                out.extend_from_slice(title);
            }
            None => out.extend_from_slice(&osc),
        }
        out.extend_from_slice(terminator);
    }
}

pub fn set_title(title: &[u8]) -> Vec<u8> {
    let mut seq = b"\x1b]2;".to_vec();
    seq.extend_from_slice(title);
    seq.push(BEL);
    seq
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(input: &[u8], locked: bool) -> Vec<u8> {
        TitleFilter::default().filter(input, locked)
    }

    #[test]
    fn plain_output_passes_through() {
        let input = "hi \x1b[31mred\x1b[0m ✓ \x1bPq#0\x1b\\ \x1b]11;?\x07".as_bytes();
        assert_eq!(run(input, false), input);
        assert_eq!(run(input, true), input);
    }

    #[test]
    fn titles_prefixed_only_while_locked() {
        assert_eq!(run(b"\x1b]2;build\x07", false), b"\x1b]2;build\x07");
        assert_eq!(
            run(b"\x1b]2;build\x07", true),
            "\x1b]2;🔒 build\x07".as_bytes()
        );
        assert_eq!(
            run(b"\x1b]0;build\x1b\\", true),
            "\x1b]0;🔒 build\x1b\\".as_bytes()
        );
    }

    #[test]
    fn remembers_last_title_without_prefix() {
        let mut f = TitleFilter::default();
        assert_eq!(f.last_title(), None);
        f.filter(b"\x1b]2;one\x07x\x1b]0;two\x1b\\", true);
        assert_eq!(f.last_title(), Some(&b"two"[..]));
        f.filter(b"\x1b]11;rgb:0/0/0\x07", true);
        assert_eq!(f.last_title(), Some(&b"two"[..]));
    }

    #[test]
    fn malformed_osc_is_passed_through() {
        assert_eq!(run(b"\x1b]2;abc\x1bXdef", true), b"\x1b]2;abc\x1bXdef");
    }

    #[test]
    fn same_output_however_chunks_are_split() {
        let input = "a\x1b]2;tïtle\x07b\x1b[1;2Hc\x1bPdata\x1b\\d\x1b]0;x\x1b\\é".as_bytes();
        let whole = run(input, true);
        for at in 0..=input.len() {
            let mut f = TitleFilter::default();
            let (a, b) = input.split_at(at);
            let mut out = f.filter(a, true);
            out.extend(f.filter(b, true));
            assert_eq!(out, whole, "split at {at}");
        }
    }

    #[test]
    fn tracks_whether_inside_a_sequence() {
        let mut f = TitleFilter::default();
        let steps: &[(&[u8], bool)] = &[
            (b"text", false),
            (b"\x1b", true),
            (b"[3", true),
            (b"1m", false),
            (b"\xe2\x94", true),
            (b"\x80", false),
            (b"\x1b]2;x", true),
            (b"\x07", false),
            (b"\x1bPq#0", true),
            (b"\x1b\\", false),
        ];
        for (chunk, inside) in steps {
            f.filter(chunk, false);
            assert_eq!(f.in_sequence(), *inside, "after {chunk:?}");
        }
    }

    #[test]
    fn set_title_sequence() {
        assert_eq!(set_title(b"migrate"), b"\x1b]2;migrate\x07");
    }
}
