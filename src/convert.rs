use crate::scan::{skip_value, skip_whitespace};

/// Trait implemented by types that can be parsed directly from a raw JSON byte slice.
///
/// # String decoding
/// For `String`, a JSON string literal (leading/trailing `"`) is **unescaped**
/// (`\"`, `\\`, `\n`, `\uXXXX`, surrogate pairs, …). Non-string slices are decoded
/// as UTF-8 text as-is (useful for numbers and bare tokens). Prefer
/// [`from_json_string`] when you only want quoted-string semantics.
pub trait FromJsonSlice: Sized {
    /// Attempts to parse an instance of `Self` from the provided raw JSON byte slice.
    fn from_json_slice(slice: &[u8]) -> Option<Self>;
}

impl FromJsonSlice for String {
    fn from_json_slice(slice: &[u8]) -> Option<Self> {
        if slice.len() >= 2 && slice[0] == b'"' && slice[slice.len() - 1] == b'"' {
            unescape_json_string_literal(slice)
        } else {
            std::str::from_utf8(slice).ok().map(String::from)
        }
    }
}

/// Parse a JSON string literal (including surrounding quotes) into an unescaped `String`.
///
/// Returns `None` if `slice` is not a well-formed quoted string (raw controls, bad
/// escapes, lone surrogates, missing quotes).
///
/// # Examples
/// ```
/// use jshift::from_json_string;
///
/// assert_eq!(from_json_string(br#""say \"hi\"""#).as_deref(), Some(r#"say "hi""#));
/// assert_eq!(from_json_string(b"not-a-string"), None);
/// ```
pub fn from_json_string(slice: &[u8]) -> Option<String> {
    unescape_json_string_literal(slice)
}

impl FromJsonSlice for bool {
    fn from_json_slice(slice: &[u8]) -> Option<Self> {
        match slice {
            b"true" => Some(true),
            b"false" => Some(false),
            _ => None,
        }
    }
}

impl<T: FromJsonSlice> FromJsonSlice for Option<T> {
    /// `null` → `Some(None)` (parse success); a valid `T` → `Some(Some(t))`;
    /// invalid payload → `None` (parse failure).
    fn from_json_slice(slice: &[u8]) -> Option<Self> {
        let start = skip_whitespace(slice, 0);
        let mut end = slice.len();
        while end > start && matches!(slice[end - 1], b' ' | b'\t' | b'\n' | b'\r') {
            end -= 1;
        }
        if &slice[start..end] == b"null" {
            return Some(None);
        }
        T::from_json_slice(slice).map(Some)
    }
}

impl<T: FromJsonSlice> FromJsonSlice for Vec<T> {
    fn from_json_slice(slice: &[u8]) -> Option<Self> {
        let mut pos = skip_whitespace(slice, 0);
        if pos >= slice.len() || slice[pos] != b'[' {
            return None;
        }
        pos += 1;

        let mut vec = Vec::new();
        loop {
            pos = skip_whitespace(slice, pos);
            if pos >= slice.len() {
                return None;
            }
            if slice[pos] == b']' {
                break;
            }
            let val_start = pos;
            let val_end = skip_value(slice, val_start).ok()?;
            let val_slice = &slice[val_start..val_end];
            let item = T::from_json_slice(val_slice)?;
            vec.push(item);

            pos = val_end;
            pos = skip_whitespace(slice, pos);
            if pos >= slice.len() {
                return None;
            }
            if slice[pos] == b',' {
                pos += 1;
            } else if slice[pos] == b']' {
                // Handled in next loop iteration
            } else {
                return None;
            }
        }
        Some(vec)
    }
}

macro_rules! impl_from_json_numeric {
    ($($t:ty),*) => {
        $(
            impl FromJsonSlice for $t {
                fn from_json_slice(slice: &[u8]) -> Option<Self> {
                    std::str::from_utf8(slice).ok()?.parse().ok()
                }
            }
        )*
    };
}

impl_from_json_numeric!(u8, u16, u32, u64, usize, i8, i16, i32, i64, isize, f32, f64);

/// Trait implemented by types that can be serialized directly into a JSON byte representation.
pub trait ToJsonBytes {
    /// Serializes the value into a raw JSON byte vector.
    ///
    /// String implementations produce a JSON string literal with required escapes
    /// (`"`, `\`, and control characters).
    fn to_json_bytes(&self) -> Vec<u8>;
}

/// Append the escaped form of `s` (content only, no surrounding quotes) into `out`.
///
/// Escapes `"`, `\`, and ASCII control characters per RFC 8259.
pub fn write_json_string_content(out: &mut Vec<u8>, s: &str) {
    out.reserve(s.len());
    for &b in s.as_bytes() {
        match b {
            b'"' => out.extend_from_slice(br#"\""#),
            b'\\' => out.extend_from_slice(br#"\\"#),
            b'\n' => out.extend_from_slice(br#"\n"#),
            b'\r' => out.extend_from_slice(br#"\r"#),
            b'\t' => out.extend_from_slice(br#"\t"#),
            b'\x08' => out.extend_from_slice(br#"\b"#),
            b'\x0c' => out.extend_from_slice(br#"\f"#),
            c if c < 0x20 => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                out.extend_from_slice(br#"\u00"#);
                out.push(HEX[(c >> 4) as usize]);
                out.push(HEX[(c & 0xf) as usize]);
            }
            c => out.push(c),
        }
    }
}

/// Escape `s` as it would appear inside a JSON string (no surrounding quotes).
///
/// Used to match logical keys against the raw key bytes stored in a document.
pub fn escape_json_key(s: &str) -> String {
    let mut v = Vec::with_capacity(s.len());
    write_json_string_content(&mut v, s);
    // Escapes are ASCII; remaining bytes are copied from UTF-8 input, so this cannot fail.
    match String::from_utf8(v) {
        Ok(s) => s,
        Err(err) => String::from_utf8_lossy(err.as_bytes()).into_owned(),
    }
}

/// Append a JSON string literal (including surrounding quotes) for `s` into `out`.
///
/// Escapes `"`, `\`, and ASCII control characters per RFC 8259.
pub fn write_json_string(out: &mut Vec<u8>, s: &str) {
    out.reserve(s.len() + 2);
    out.push(b'"');
    write_json_string_content(out, s);
    out.push(b'"');
}

/// Serialize `s` as a JSON string literal (including surrounding quotes).
pub fn escape_json_string(s: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(s.len() + 2);
    write_json_string(&mut v, s);
    v
}

/// Unescape a JSON string value slice (including surrounding quotes) into a Rust `String`.
///
/// Returns `None` if the slice is not a well-formed JSON string literal or contains
/// invalid escape sequences / UTF-8.
pub(crate) fn unescape_json_string_literal(slice: &[u8]) -> Option<String> {
    if slice.len() < 2 || slice[0] != b'"' || slice[slice.len() - 1] != b'"' {
        return None;
    }
    unescape_json_string_content(&slice[1..slice.len() - 1])
}

/// Unescape JSON string content (bytes between the quotes).
///
/// Rejects raw control characters (U+0000..=U+001F) outside escapes, invalid
/// escape sequences, and lone UTF-16 surrogates. Accepts standard surrogate pairs.
pub(crate) fn unescape_json_string_content(content: &[u8]) -> Option<String> {
    let mut out = Vec::with_capacity(content.len());
    let mut i = 0;
    while i < content.len() {
        match content[i] {
            b'\\' => {
                i += 1;
                if i >= content.len() {
                    return None;
                }
                match content[i] {
                    b'"' => out.push(b'"'),
                    b'\\' => out.push(b'\\'),
                    b'/' => out.push(b'/'),
                    b'b' => out.push(0x08),
                    b'f' => out.push(0x0c),
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    b'u' => {
                        let (ch, consumed) = decode_json_unicode_escape(content, i)?;
                        let mut buf = [0u8; 4];
                        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        // `i` points at `u`; consumed is total bytes of the \u sequence(s)
                        // after the backslash (including `u` and hex digits).
                        i += consumed;
                        continue;
                    }
                    _ => return None,
                }
                i += 1;
            }
            // JSON strings may not contain unescaped control characters.
            c if c < 0x20 => return None,
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Decode a `\uXXXX` (or surrogate pair) starting at the `u` of the first escape.
/// Returns `(char, bytes_consumed_from_u_inclusive)`.
fn decode_json_unicode_escape(content: &[u8], u_pos: usize) -> Option<(char, usize)> {
    let code = parse_u4_hex(content, u_pos + 1)?;
    if (0xD800..=0xDBFF).contains(&code) {
        // High surrogate — require a low surrogate escape immediately after.
        let next = u_pos + 5;
        if next + 5 >= content.len() || content[next] != b'\\' || content[next + 1] != b'u' {
            return None;
        }
        let low = parse_u4_hex(content, next + 2)?;
        if !(0xDC00..=0xDFFF).contains(&low) {
            return None;
        }
        let cp = 0x10000 + (((u32::from(code) - 0xD800) << 10) | (u32::from(low) - 0xDC00));
        let ch = char::from_u32(cp)?;
        // `u` + 4 hex + `\` + `u` + 4 hex = 11 bytes from first `u`.
        Some((ch, 11))
    } else if (0xDC00..=0xDFFF).contains(&code) {
        // Lone low surrogate.
        None
    } else {
        let ch = char::from_u32(u32::from(code))?;
        Some((ch, 5)) // `u` + 4 hex digits
    }
}

fn parse_u4_hex(content: &[u8], start: usize) -> Option<u16> {
    if start + 4 > content.len() {
        return None;
    }
    let hex = std::str::from_utf8(&content[start..start + 4]).ok()?;
    u16::from_str_radix(hex, 16).ok()
}

impl ToJsonBytes for String {
    fn to_json_bytes(&self) -> Vec<u8> {
        escape_json_string(self)
    }
}

impl ToJsonBytes for str {
    fn to_json_bytes(&self) -> Vec<u8> {
        escape_json_string(self)
    }
}

impl ToJsonBytes for bool {
    fn to_json_bytes(&self) -> Vec<u8> {
        if *self {
            b"true".to_vec()
        } else {
            b"false".to_vec()
        }
    }
}

impl<T: ToJsonBytes> ToJsonBytes for Option<T> {
    /// `None` serializes as JSON `null`; `Some(v)` uses `v`'s encoding.
    fn to_json_bytes(&self) -> Vec<u8> {
        match self {
            Some(v) => v.to_json_bytes(),
            None => b"null".to_vec(),
        }
    }
}

macro_rules! impl_to_json_numeric {
    ($($t:ty),*) => {
        $(
            impl ToJsonBytes for $t {
                fn to_json_bytes(&self) -> Vec<u8> {
                    self.to_string().into_bytes()
                }
            }
        )*
    };
}

impl_to_json_numeric!(u8, u16, u32, u64, usize, i8, i16, i32, i64, isize, f32, f64);

impl<T: ToJsonBytes> ToJsonBytes for Vec<T> {
    fn to_json_bytes(&self) -> Vec<u8> {
        let mut v = Vec::new();
        v.push(b'[');
        for (i, item) in self.iter().enumerate() {
            if i > 0 {
                v.push(b',');
            }
            v.extend_from_slice(&item.to_json_bytes());
        }
        v.push(b']');
        v
    }
}

impl<T: ToJsonBytes> ToJsonBytes for [T] {
    fn to_json_bytes(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(self.len() * 10 + 2);
        v.push(b'[');
        for (i, item) in self.iter().enumerate() {
            if i > 0 {
                v.push(b',');
            }
            v.extend_from_slice(&item.to_json_bytes());
        }
        v.push(b']');
        v
    }
}
