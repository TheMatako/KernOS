// kernel/src/shell/parser.rs
//
// Command-line tokenizer and parser.
//
// ── Supported Grammar ────────────────────────────────────────────────────────
//
//   command = TOKEN (SPACE TOKEN)*
//   TOKEN   = word_without_spaces | "word with spaces" | 'word with spaces'
//
// Examples:
//   ls /tmp
//   echo "hello world"
//   cat /etc/motd
//   ping 10.0.2.2
//
// Note: No redirection or pipe support at this stage (planned for Shell v2).

#![allow(dead_code)]
#![allow(static_mut_refs)]

/// Maximum number of tokens (arguments) allowed per command.
pub const MAX_ARGS: usize = 16;
/// Maximum length of the entire command line input.
pub const LINE_MAX: usize = 256;
/// Maximum length of a single parsed token.
pub const TOKEN_MAX: usize = 128;

// ---------------------------------------------------------------------------
// Parsing Result
// ---------------------------------------------------------------------------

/// The result of successfully parsing a command line.
///
/// Designed to be zero-allocation (stack-based), utilizing fixed-size buffers
/// to avoid heap fragmentation during intensive shell usage.
pub struct Command {
    /// Array of null-terminated tokens. `argv[0]` is the command name.
    pub argv: [[u8; TOKEN_MAX]; MAX_ARGS],
    /// Number of valid tokens stored in `argv`.
    pub argc: usize,
}

impl Command {
    /// Returns `argv[0]` (the command name) as a UTF-8 string slice.
    pub fn name(&self) -> &str {
        let s = &self.argv[0];
        let len = s.iter().position(|&b| b == 0).unwrap_or(TOKEN_MAX);
        core::str::from_utf8(&s[..len]).unwrap_or("")
    }

    /// Returns `argv[i]` as a UTF-8 string slice.
    /// Returns an empty string if `i` is out of bounds.
    pub fn arg(&self, i: usize) -> &str {
        if i >= self.argc {
            return "";
        }
        let s = &self.argv[i];
        let len = s.iter().position(|&b| b == 0).unwrap_or(TOKEN_MAX);
        core::str::from_utf8(&s[..len]).unwrap_or("")
    }

    /// Reconstructs arguments starting from `start` into a single space-separated string.
    ///
    /// Useful for commands like `echo` where multiple tokens need to be printed
    /// as a single normalized sentence. Writes the result into `out` and returns the length.
    pub fn args_joined(&self, start: usize, out: &mut [u8; TOKEN_MAX]) -> usize {
        let mut pos = 0usize;
        for i in start..self.argc {
            let tok = self.arg(i).as_bytes();
            for &b in tok {
                if pos >= TOKEN_MAX - 1 {
                    break;
                }
                out[pos] = b;
                pos += 1;
            }
            // Add a space between tokens if it's not the last one
            if i + 1 < self.argc && pos < TOKEN_MAX - 1 {
                out[pos] = b' ';
                pos += 1;
            }
        }
        out[pos] = 0; // Null-terminator
        pos
    }
}

// ---------------------------------------------------------------------------
// Core Parsing Logic
// ---------------------------------------------------------------------------

/// Parses a raw byte slice `line` into a `Command` structure.
///
/// Returns `None` if the line is empty or contains only whitespace.
pub fn parse(line: &[u8]) -> Option<Command> {
    let mut cmd = Command {
        argv: [[0u8; TOKEN_MAX]; MAX_ARGS],
        argc: 0,
    };

    let mut i = 0usize;
    let len = line.len();

    loop {
        // 1. Skip leading whitespace (spaces and tabs)
        while i < len && (line[i] == b' ' || line[i] == b'\t') {
            i += 1;
        }

        // Stop if we reached the end of the line or the maximum argument limit
        if i >= len {
            break;
        }
        if cmd.argc >= MAX_ARGS {
            break;
        }

        // 2. Begin token extraction
        let mut tok_len = 0usize;
        let tok = &mut cmd.argv[cmd.argc];

        // Check if the token is wrapped in quotes
        let quote = if line[i] == b'"' || line[i] == b'\'' {
            let q = line[i];
            i += 1; // Skip the opening quote
            Some(q)
        } else {
            None
        };

        loop {
            if i >= len {
                break;
            }
            let b = line[i];

            match quote {
                Some(q) => {
                    // If inside quotes, stop only when the matching closing quote is found
                    if b == q {
                        i += 1;
                        break;
                    }
                    if tok_len < TOKEN_MAX - 1 {
                        tok[tok_len] = b;
                        tok_len += 1;
                    }
                    i += 1;
                }
                None => {
                    // If not in quotes, stop at the first whitespace
                    if b == b' ' || b == b'\t' {
                        break;
                    }
                    if tok_len < TOKEN_MAX - 1 {
                        tok[tok_len] = b;
                        tok_len += 1;
                    }
                    i += 1;
                }
            }
        }

        // 3. Null-terminate the token and increment argument count
        tok[tok_len] = 0;
        cmd.argc += 1;
    }

    // Return None if nothing was parsed
    if cmd.argc == 0 {
        None
    } else {
        Some(cmd)
    }
}
