//! Input gate: decides which bytes from the user's terminal reach the command.
//!
//! Input is tokenized into single bytes and complete escape sequences, so a
//! sequence split across reads is handled exactly like one read in one go.

const ESC: u8 = 0x1b;
const CTRL_BRACKET: u8 = 0x1d;
const KITTY_CTRL_BRACKET: &[u8] = b"\x1b[93;5u";
const KITTY_L: &[u8] = b"\x1b[108u";
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";
const MAX_SEQUENCE: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Lock,
    Unlock,
    DroppedInput,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Output {
    pub forward: Vec<u8>,
    pub events: Vec<Event>,
}

#[derive(Debug)]
enum Token {
    Byte(u8),
    Seq(Vec<u8>),
}

impl Token {
    fn bytes(&self) -> &[u8] {
        match self {
            Token::Byte(b) => std::slice::from_ref(b),
            Token::Seq(s) => s,
        }
    }

    /// Something the terminal sends on its own (focus changes, mouse
    /// reports, replies to queries) rather than a key the user pressed.
    fn is_report(&self) -> bool {
        let Token::Seq(s) = self else {
            return false;
        };
        let last = s[s.len() - 1];
        match s[1] {
            // X10 mouse: ESC [ M and three raw bytes, whatever they are.
            b'[' if s.get(2) == Some(&b'M') => s.len() == 6,
            // Focus, SGR mouse, cursor position, device attributes, window reports.
            b'[' => s.len() > 2 && b"IOMmRct".contains(&last),
            // Terminated OSC, DCS, APC and PM strings. An unterminated `ESC P`
            // flushed on timeout is Alt+Shift+P.
            b']' | b'P' | b'_' | b'^' => string_terminated(s),
            _ => false,
        }
    }

    fn is(&self, byte: u8, kitty: &[u8]) -> bool {
        match self {
            Token::Byte(b) => *b == byte,
            Token::Seq(s) => s == kitty,
        }
    }
}

pub struct Gate {
    locked: bool,
    hotkey: bool,
    phrase: Option<Vec<u8>>,
    partial: Vec<u8>,
    prefix: Option<Token>,
    typed: Vec<u8>,
    in_paste: bool,
}

impl Gate {
    pub fn new(locked: bool, hotkey: bool, phrase: Option<String>) -> Gate {
        Gate {
            locked,
            hotkey,
            phrase: phrase.map(String::into_bytes),
            partial: Vec::new(),
            prefix: None,
            typed: Vec::new(),
            in_paste: false,
        }
    }

    pub fn is_locked(&self) -> bool {
        self.locked
    }

    pub fn has_partial(&self) -> bool {
        !self.partial.is_empty()
    }

    pub fn set_locked(&mut self, locked: bool) -> bool {
        if self.locked == locked {
            return false;
        }
        self.locked = locked;
        // A half-read escape belongs to the old state; flushing it later would
        // leak a key typed while locked.
        self.partial.clear();
        self.prefix = None;
        self.clear_typed();
        true
    }

    pub fn feed(&mut self, input: &[u8]) -> Output {
        let mut out = Output::default();
        for &byte in input {
            if let Some(token) = self.tokenize(byte) {
                self.handle(token, &mut out);
            }
        }
        out
    }

    pub fn flush(&mut self) -> Output {
        let mut out = Output::default();
        let partial = std::mem::take(&mut self.partial);
        match partial.len() {
            0 => {}
            1 => self.handle(Token::Byte(partial[0]), &mut out),
            _ => self.handle(Token::Seq(partial), &mut out),
        }
        out
    }

    fn tokenize(&mut self, byte: u8) -> Option<Token> {
        if self.partial.is_empty() {
            if byte == ESC {
                self.partial.push(byte);
                return None;
            }
            return Some(Token::Byte(byte));
        }
        self.partial.push(byte);
        sequence_complete(&self.partial).then(|| Token::Seq(std::mem::take(&mut self.partial)))
    }

    fn handle(&mut self, token: Token, out: &mut Output) {
        let marker = match token.bytes() {
            PASTE_START => Some(true),
            PASTE_END => Some(false),
            _ => None,
        };
        let pasting = self.in_paste || marker.is_some();
        if let Some(in_paste) = marker {
            self.in_paste = in_paste;
        }
        if self.locked {
            self.handle_locked(token, pasting, out);
        } else {
            self.handle_unlocked(token, pasting, out);
        }
    }

    fn handle_unlocked(&mut self, token: Token, pasting: bool, out: &mut Output) {
        if let Some(prefix) = self.prefix.take() {
            if token.is(CTRL_BRACKET, KITTY_CTRL_BRACKET) {
                out.forward.extend_from_slice(prefix.bytes());
            } else if token.is(b'l', KITTY_L) {
                self.locked = true;
                self.clear_typed();
                out.events.push(Event::Lock);
            } else {
                out.forward.extend_from_slice(prefix.bytes());
                out.forward.extend_from_slice(token.bytes());
            }
            return;
        }
        if self.hotkey && !pasting && token.is(CTRL_BRACKET, KITTY_CTRL_BRACKET) {
            self.prefix = Some(token);
            return;
        }
        out.forward.extend_from_slice(token.bytes());
    }

    fn handle_locked(&mut self, token: Token, pasting: bool, out: &mut Output) {
        if !token.is_report() && !out.events.contains(&Event::DroppedInput) {
            out.events.push(Event::DroppedInput);
        }
        let Some(phrase) = self.phrase.as_deref() else {
            return;
        };
        match token {
            Token::Byte(byte @ 0x20..=0x7e) if !pasting => {
                // Only the last `phrase.len()` printable keys can matter.
                if self.typed.len() == phrase.len() {
                    self.typed.remove(0);
                }
                self.typed.push(byte);
                if self.typed == phrase {
                    self.clear_typed();
                    self.locked = false;
                    out.events.push(Event::Unlock);
                }
            }
            _ => self.clear_typed(),
        }
    }

    fn clear_typed(&mut self) {
        self.typed.clear();
    }
}

/// `seq` starts with ESC and has at least two bytes.
fn sequence_complete(seq: &[u8]) -> bool {
    let last = seq[seq.len() - 1];
    if seq.len() >= MAX_SEQUENCE {
        return true;
    }
    match seq[1] {
        b'[' => match seq.len() {
            2 => false,
            // X10 mouse report: ESC [ M followed by three raw bytes.
            _ if seq[2] == b'M' => seq.len() == 6,
            _ => (0x40..=0x7e).contains(&last),
        },
        b'O' => seq.len() == 3,
        b']' | b'P' | b'_' | b'^' => string_terminated(seq),
        _ => true,
    }
}

/// An OSC, DCS, APC or PM string ends with ST (`ESC \\`), or BEL for OSC.
fn string_terminated(seq: &[u8]) -> bool {
    let last = seq[seq.len() - 1];
    (seq[1] == b']' && last == 0x07)
        || (seq.len() >= 4 && seq[seq.len() - 2] == ESC && last == b'\\')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unlocked() -> Gate {
        Gate::new(false, true, Some("unlock".into()))
    }
    fn locked() -> Gate {
        Gate::new(true, true, Some("unlock".into()))
    }
    fn no_hotkey() -> Gate {
        Gate::new(false, false, Some("unlock".into()))
    }
    fn locked_no_phrase() -> Gate {
        Gate::new(true, true, None)
    }

    /// Feeds `parts` in order, then flushes. Returns forwarded bytes and
    /// events without `DroppedInput` (its per-call dedup depends on splits).
    fn drive(mut gate: Gate, parts: &[&[u8]]) -> (Vec<u8>, Vec<Event>) {
        let mut forward = Vec::new();
        let mut events = Vec::new();
        let mut collect = |out: Output| {
            forward.extend(out.forward);
            events.extend(out.events.into_iter().filter(|e| *e != Event::DroppedInput));
        };
        for part in parts {
            collect(gate.feed(part));
        }
        collect(gate.flush());
        (forward, events)
    }

    type Case = (fn() -> Gate, &'static [u8], &'static [u8], &'static [Event]);

    const CASES: &[Case] = &[
        (unlocked, b"hello\x03\x1b[A", b"hello\x03\x1b[A", &[]),
        (unlocked, b"ab\x1dlcd", b"ab", &[Event::Lock]),
        (unlocked, b"\x1b[93;5ulcd", b"", &[Event::Lock]),
        (unlocked, b"\x1d\x1b[108u", b"", &[Event::Lock]),
        (unlocked, b"\x1d\x1d", b"\x1d", &[]),
        (unlocked, b"\x1b[93;5u\x1b[93;5u", b"\x1b[93;5u", &[]),
        (unlocked, b"\x1dx", b"\x1dx", &[]),
        (
            unlocked,
            b"\x1b[200~\x1dl\x1b[201~",
            b"\x1b[200~\x1dl\x1b[201~",
            &[],
        ),
        (no_hotkey, b"\x1dl", b"\x1dl", &[]),
        (locked, b"abc\x03\x1b[A\x1b[<0;3;4M\x1dl", b"", &[]),
        // The phrase unlocks as soon as its last letter is typed; no Enter.
        (locked, b"unlockhi", b"hi", &[Event::Unlock]),
        (locked, b"unlock\rhi", b"\rhi", &[Event::Unlock]),
        // Stray keys typed before the phrase don't stop it matching.
        (locked, b"xq unlockhi", b"hi", &[Event::Unlock]),
        (locked, b"unlunlock", b"", &[Event::Unlock]),
        (locked, b"unloc", b"", &[]),
        (locked, b"unlo\rck", b"", &[]),
        (locked, b"unl\x1b[Dock", b"", &[]),
        (locked, b"\x1b[200~unlock\x1b[201~", b"", &[]),
        (locked, b"\x1b[200~x\x1b[201~unlock", b"", &[Event::Unlock]),
        (locked, b"\x1b[M !!unlock", b"", &[Event::Unlock]),
        (
            locked,
            b"\x1b]11;rgb:0/0/0\x07unlock",
            b"",
            &[Event::Unlock],
        ),
        (locked_no_phrase, b"unlock", b"", &[]),
    ];

    #[test]
    fn cases_in_one_read() {
        for (i, (gate, input, forward, events)) in CASES.iter().enumerate() {
            let (f, e) = drive(gate(), &[*input]);
            assert_eq!(f, *forward, "case {i}: forward");
            assert_eq!(e, *events, "case {i}: events");
        }
    }

    #[test]
    fn cases_split_at_every_boundary() {
        for (i, (gate, input, forward, events)) in CASES.iter().enumerate() {
            for at in 0..=input.len() {
                let (a, b) = input.split_at(at);
                assert_eq!(
                    drive(gate(), &[a, b]),
                    (forward.to_vec(), events.to_vec()),
                    "case {i} split at {at}"
                );
            }
            let bytes: Vec<&[u8]> = input.chunks(1).collect();
            assert_eq!(
                drive(gate(), &bytes),
                (forward.to_vec(), events.to_vec()),
                "case {i} byte by byte"
            );
        }
    }

    #[test]
    fn dropped_input_reported_once_per_feed() {
        let mut gate = locked();
        assert_eq!(gate.feed(b"abc").events, vec![Event::DroppedInput]);
        assert_eq!(unlocked().feed(b"abc").events, vec![]);
    }

    #[test]
    fn terminal_reports_are_dropped_silently() {
        let silent: &[&[u8]] = &[
            b"\x1b[I",                  // focus in
            b"\x1b[O",                  // focus out
            b"\x1b[M !!",               // X10 mouse
            b"\x1b[<0;3;4M",            // SGR mouse press
            b"\x1b[<0;3;4m",            // SGR mouse release
            b"\x1b[12;40R",             // cursor position report
            b"\x1b[?62;22c",            // device attributes
            b"\x1b[8;24;80t",           // window size report
            b"\x1b]11;rgb:0/0/0\x07",   // OSC colour reply, BEL
            b"\x1b]11;rgb:0/0/0\x1b\\", // OSC colour reply, ST
            b"\x1bP1$r0m\x1b\\",        // DCS
            b"\x1b_Gi=1;OK\x1b\\",      // APC
            b"\x1b^x\x1b\\",            // PM
        ];
        let noisy: &[&[u8]] = &[
            b"a",
            b"\x03",
            b"\r",
            b"\x1b",
            b"\x1b[A",
            b"\x1bOP",
            b"\x1b[3~",
            b"\x1b[97;5u",
            b"\x1b[200~x\x1b[201~",
            b"\x1bP",              // Alt+Shift+P, not a DCS string
            b"\x1b[I\x1b[12;40Ra", // a key among reports still rings
        ];
        let events = |input: &[u8]| {
            let mut gate = locked();
            let mut out = gate.feed(input);
            let flushed = gate.flush();
            out.forward.extend(flushed.forward);
            out.events.extend(flushed.events);
            assert_eq!(out.forward, b"", "{input:?} was forwarded");
            out.events.contains(&Event::DroppedInput)
        };
        for input in silent {
            assert!(
                !events(input),
                "{:?} rang the bell",
                String::from_utf8_lossy(input)
            );
        }
        for input in noisy {
            assert!(
                events(input),
                "{:?} was silent",
                String::from_utf8_lossy(input)
            );
        }
    }

    #[test]
    fn phrase_matches_after_any_amount_of_stray_typing() {
        let mut gate = locked();
        let mut input = vec![b'x'; 10_000];
        input.extend_from_slice(b"unlock");
        assert!(gate.feed(&input).events.contains(&Event::Unlock));
        assert!(!gate.is_locked());
    }

    #[test]
    fn lone_escape_waits_for_flush() {
        let mut gate = unlocked();
        assert_eq!(gate.feed(b"\x1b").forward, b"");
        assert!(gate.has_partial());
        assert_eq!(gate.flush().forward, b"\x1b");
        assert!(!gate.has_partial());
    }

    #[test]
    fn set_locked_reports_change_and_resets_state() {
        let mut gate = unlocked();
        assert_eq!(gate.feed(b"\x1d").forward, b"");
        assert!(gate.set_locked(true));
        assert!(!gate.set_locked(true));
        assert!(gate.is_locked());
        // The pending Ctrl+] was discarded, so this `l` is forwarded, not a lock.
        assert!(gate.set_locked(false));
        assert_eq!(gate.feed(b"l").forward, b"l");
        // Half-typed phrase is discarded on lock.
        gate.set_locked(true);
        gate.feed(b"unl");
        gate.set_locked(false);
        gate.set_locked(true);
        assert!(!gate.feed(b"ock\r").events.contains(&Event::Unlock));
    }

    #[test]
    fn set_locked_discards_a_partial_escape() {
        // ESC typed while locked, then `keylock off` before the escape timeout.
        let mut gate = locked();
        gate.feed(b"\x1b");
        assert!(gate.set_locked(false));
        assert!(!gate.has_partial());
        assert_eq!(gate.flush(), Output::default());
        assert_eq!(gate.feed(b"x").forward, b"x");

        // And the other way: a half-typed sequence is not dropped as a key later.
        let mut gate = unlocked();
        gate.feed(b"\x1b[");
        assert!(gate.set_locked(true));
        assert!(!gate.has_partial());
        assert_eq!(gate.flush(), Output::default());
    }
}
