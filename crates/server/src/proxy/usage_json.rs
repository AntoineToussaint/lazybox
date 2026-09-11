//! Bounded JSON projection for metering. Validate the document incrementally,
//! retaining only provider-owned metadata. Output strings and arrays can be
//! arbitrarily long; neither is buffered. serde_json validates scalar syntax,
//! UTF-8 and escapes (including surrogate pairs) in bounded string segments.
use serde_json::{Map, Value};

const STRING_SEGMENT: usize = 4096;
const MAX_DEPTH: usize = 128;

#[derive(Debug, Clone, Copy)]
enum Scope {
    Root,
    Response,
    Message,
    Usage,
    Details,
    Ignore,
}

impl Scope {
    fn field(self, key: &str) -> Option<Self> {
        use Scope::*;
        match (self, key) {
            (Root, "response") => Some(Response),
            (Root, "message") => Some(Message),
            (Root | Response | Message, "usage") => Some(Usage),
            (Root, "type") | (Root | Response | Message, "model") => Some(Ignore),
            (Usage, "input_tokens_details" | "prompt_tokens_details") => Some(Details),
            (
                Usage,
                "input_tokens"
                | "output_tokens"
                | "prompt_tokens"
                | "completion_tokens"
                | "cache_creation_input_tokens"
                | "cache_read_input_tokens"
                | "cached_input_tokens",
            )
            | (Details, "cached_tokens") => Some(Ignore),
            _ => None,
        }
    }
}

#[derive(Debug, PartialEq)]
enum Expect {
    KeyOrEnd,
    Key,
    Colon,
    ValueOrEnd,
    Value,
    CommaOrEnd,
}

#[derive(Debug)]
struct Frame {
    object: bool,
    expect: Expect,
    scope: Scope,
    key: Option<String>,
    fields: Map<String, Value>,
}

/// Number grammar is checked without retaining arbitrarily long mantissas
/// or exponents. Values too large to be token counts need no representation.
#[derive(Debug, Clone, Copy)]
enum Number {
    Sign,
    Zero,
    Integer,
    Dot,
    Fraction,
    Exponent,
    ExponentSign,
    ExponentDigits,
}

impl Number {
    fn next(self, byte: u8) -> Option<Self> {
        use Number::*;
        match (self, byte) {
            (Sign, b'0') => Some(Zero),
            (Sign, b'1'..=b'9') | (Integer, b'0'..=b'9') => Some(Integer),
            (Zero | Integer, b'.') => Some(Dot),
            (Dot | Fraction, b'0'..=b'9') => Some(Fraction),
            (Zero | Integer | Fraction, b'e' | b'E') => Some(Exponent),
            (Exponent, b'+' | b'-') => Some(ExponentSign),
            (Exponent | ExponentSign | ExponentDigits, b'0'..=b'9') => Some(ExponentDigits),
            _ => None,
        }
    }

    fn complete(self) -> bool {
        matches!(
            self,
            Self::Zero | Self::Integer | Self::Fraction | Self::ExponentDigits
        )
    }
}

#[derive(Debug, Default)]
pub(super) struct UsageJson {
    stack: Vec<Frame>,
    token: Vec<u8>,
    string: bool,
    escaped: bool,
    segmented: bool,
    number: Option<Number>,
    oversized_number: bool,
    invalid: bool,
    root: Option<Value>,
}

impl UsageJson {
    pub(super) fn push(&mut self, byte: u8) {
        if self.invalid {
            return;
        }
        if self.string {
            self.token.push(byte);
            if byte == b'"' && !self.escaped {
                self.string = false;
                match serde_json::from_slice::<Value>(&self.token) {
                    Ok(value) => self.scalar(if self.segmented { Value::Null } else { value }),
                    Err(_) => self.invalid = true,
                }
                self.token.clear();
                self.segmented = false;
            } else {
                self.escaped = byte == b'\\' && !self.escaped;
                if self.token.len() >= STRING_SEGMENT && !self.escaped {
                    // Try closing a segment. A partial UTF-8 code point or
                    // Unicode escape/pair needs at most twelve more bytes.
                    self.token.push(b'"');
                    let valid = serde_json::from_slice::<String>(&self.token).is_ok();
                    self.token.pop();
                    if valid {
                        self.token.clear();
                        self.token.push(b'"');
                        self.segmented = true;
                    } else if self.token.len() > STRING_SEGMENT + 12 {
                        self.invalid = true;
                    }
                }
            }
            return;
        }
        if !self.token.is_empty() {
            if byte.is_ascii_whitespace() || matches!(byte, b',' | b']' | b'}') {
                if self.number.is_some_and(|number| !number.complete()) {
                    self.invalid = true;
                } else if self.oversized_number {
                    self.scalar(Value::Null);
                } else {
                    match serde_json::from_slice::<Value>(&self.token) {
                        Ok(value) => self.scalar(value),
                        // The number grammar was already validated. A valid
                        // JSON number outside serde_json's numeric range is
                        // irrelevant output or an invalid token count; neither
                        // should prevent extracting other provider metadata.
                        Err(_) if self.number.is_some() => self.scalar(Value::Null),
                        Err(_) => self.invalid = true,
                    }
                }
                self.token.clear();
                self.number = None;
                self.oversized_number = false;
            } else {
                if let Some(number) = self.number {
                    self.number = number.next(byte);
                    if self.number.is_none() {
                        self.invalid = true;
                    }
                }
                if self.token.len() < 128 {
                    self.token.push(byte);
                } else if self.number.is_some() {
                    self.oversized_number = true;
                } else {
                    self.invalid = true;
                }
                return;
            }
        }
        match byte {
            b' ' | b'\t' | b'\r' | b'\n' => {}
            b'"' => {
                self.string = true;
                self.escaped = false;
                self.token.push(byte);
            }
            b'{' | b'[' => {
                let scope = self.stack.last().map_or(Scope::Root, |frame| {
                    frame
                        .key
                        .as_deref()
                        .and_then(|key| frame.scope.field(key))
                        .unwrap_or(Scope::Ignore)
                });
                if !self.expects_value() || self.stack.len() == MAX_DEPTH {
                    self.invalid = true;
                    return;
                }
                self.stack.push(Frame {
                    object: byte == b'{',
                    expect: if byte == b'{' {
                        Expect::KeyOrEnd
                    } else {
                        Expect::ValueOrEnd
                    },
                    scope: if byte == b'{' { scope } else { Scope::Ignore },
                    key: None,
                    fields: Map::new(),
                });
            }
            b'}' | b']' => {
                let Some(frame) = self.stack.pop() else {
                    self.invalid = true;
                    return;
                };
                if frame.object != (byte == b'}')
                    || !matches!(
                        frame.expect,
                        Expect::KeyOrEnd | Expect::ValueOrEnd | Expect::CommaOrEnd
                    )
                {
                    self.invalid = true;
                    return;
                }
                self.value(if frame.object {
                    Value::Object(frame.fields)
                } else {
                    Value::Null
                });
            }
            b':' => match self.stack.last_mut() {
                Some(frame) if frame.expect == Expect::Colon => frame.expect = Expect::Value,
                _ => self.invalid = true,
            },
            b',' => match self.stack.last_mut() {
                Some(frame) if frame.expect == Expect::CommaOrEnd => {
                    frame.expect = if frame.object {
                        Expect::Key
                    } else {
                        Expect::Value
                    };
                }
                _ => self.invalid = true,
            },
            b'-' | b'0'..=b'9' => {
                self.number = Some(match byte {
                    b'-' => Number::Sign,
                    b'0' => Number::Zero,
                    _ => Number::Integer,
                });
                self.token.push(byte);
            }
            b't' | b'f' | b'n' => self.token.push(byte),
            _ => self.invalid = true,
        }
    }

    fn expects_value(&self) -> bool {
        self.stack.last().map_or(self.root.is_none(), |frame| {
            matches!(frame.expect, Expect::Value | Expect::ValueOrEnd)
        })
    }

    fn scalar(&mut self, value: Value) {
        if let Some(frame) = self.stack.last_mut()
            && matches!(frame.expect, Expect::Key | Expect::KeyOrEnd)
        {
            // An oversized key is validated but cannot name a metering field.
            if let Value::String(key) = value {
                frame.key = frame.scope.field(&key).map(|_| key);
            } else if self.segmented {
                frame.key = None;
            } else {
                self.invalid = true;
            }
            frame.expect = Expect::Colon;
            return;
        }
        self.value(value);
    }

    fn value(&mut self, value: Value) {
        if !self.expects_value() {
            self.invalid = true;
            return;
        }
        if let Some(frame) = self.stack.last_mut() {
            if let Some(key) = frame.key.take() {
                frame.fields.insert(key, value);
            }
            frame.expect = Expect::CommaOrEnd;
        } else {
            self.root = Some(value);
        }
    }

    pub(super) fn finish(mut self) -> Option<Value> {
        self.push(b' ');
        if self.invalid || self.string || !self.stack.is_empty() {
            tracing::debug!("Cannot meter malformed or excessively nested response JSON");
            None
        } else {
            self.root
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(bytes: &[u8]) -> Option<Value> {
        let mut parser = UsageJson::default();
        for &byte in bytes {
            parser.push(byte);
            assert!(parser.token.capacity() <= 2 * (STRING_SEGMENT + 12));
            assert!(parser.stack.len() <= MAX_DEPTH);
            for frame in &parser.stack {
                assert!(frame.fields.len() <= 10);
            }
        }
        parser.finish()
    }

    #[test]
    fn discarded_strings_validate_utf8_and_escapes_across_segments() {
        for padding in 0..16 {
            let text = format!(
                "{}{}",
                "a".repeat(STRING_SEGMENT - padding),
                "😀\\\"\n".repeat(2000)
            );
            let body = serde_json::json!({"output": text, "usage":{"input_tokens":3}});
            assert_eq!(
                parse(body.to_string().as_bytes()).unwrap()["usage"]["input_tokens"],
                3
            );
            let escaped = format!(
                r#"{{"output":"{}{}","usage":{{"input_tokens":3}}}}"#,
                "a".repeat(STRING_SEGMENT - padding),
                r"\uD83D\uDE00".repeat(2000)
            );
            assert_eq!(
                parse(escaped.as_bytes()).unwrap()["usage"]["input_tokens"],
                3
            );
        }
    }

    #[test]
    fn large_values_keys_and_arrays_have_bounded_retained_state() {
        let body = format!(
            r#"{{"{}":"{}","output":[{}null],"usage":{{"input_tokens":3}}}}"#,
            "k".repeat(10_000),
            "a".repeat(9 * 1024 * 1024),
            r#"{"ignored":[1,true,null]},"#.repeat(10_000)
        );
        assert_eq!(
            parse(body.as_bytes()).unwrap(),
            serde_json::json!({"usage":{"input_tokens":3}})
        );
    }

    #[test]
    fn malformed_discarded_values_cannot_produce_a_usage_report() {
        for bad in [
            r#"{"x":1,}"#,
            "[1,]",
            "[1 2]",
            "[true false]",
            "01",
            "1e",
            "true0",
            r#""\uD800""#,
            r#""\q""#,
            "{\"x\" 1}",
            "{1:2}",
        ] {
            let body = format!(r#"{{"usage":{{"input_tokens":3}},"output":{bad}}}"#);
            assert!(parse(body.as_bytes()).is_none(), "{bad}");
        }
        let mut body = b"{\"usage\":{\"input_tokens\":3},\"output\":\"".to_vec();
        body.extend(vec![b'a'; STRING_SEGMENT * 2]);
        body.extend_from_slice(b"\xff\"}");
        assert!(parse(&body).is_none());
        assert!(parse(b"{\"usage\":{\"input_tokens\":3}} {}").is_none());
        assert!(parse(b"{\"usage\":{\"input_tokens\":3}").is_none());
    }

    #[test]
    fn long_output_numbers_do_not_discard_usage_or_bypass_number_validation() {
        for number in [
            format!("1e+{}", "0".repeat(10_000)),
            format!("0.{}1", "0".repeat(10_000)),
        ] {
            let body = format!(r#"{{"output":{number},"usage":{{"input_tokens":3}}}}"#);
            assert!(serde_json::from_str::<Value>(&body).is_ok());
            assert_eq!(parse(body.as_bytes()).unwrap()["usage"]["input_tokens"], 3);
            for suffix in ["e", ".", "-", "x"] {
                let bad = format!(r#"{{"output":{number}{suffix},"usage":{{"input_tokens":3}}}}"#);
                assert!(parse(bad.as_bytes()).is_none());
            }
        }
    }

    #[test]
    fn out_of_range_output_numbers_preserve_usage_but_cannot_be_token_counts() {
        for number in ["1e10000", "-1e10000"] {
            let body = format!(r#"{{"output":{number},"usage":{{"input_tokens":3}}}}"#);
            assert_eq!(parse(body.as_bytes()).unwrap()["usage"]["input_tokens"], 3);
            let body = format!(r#"{{"usage":{{"input_tokens":{number}}}}}"#);
            assert!(parse(body.as_bytes()).unwrap()["usage"]["input_tokens"].is_null());
        }
    }

    #[test]
    fn output_metadata_cannot_impersonate_provider_usage() {
        let body = br#"{"model":"real","output":[{"usage":{"input_tokens":999},"model":"fake"}],"response":{"model":"real","usage":{"input_tokens":3,"input_tokens_details":{"cached_tokens":1}}}}"#;
        assert_eq!(
            parse(body).unwrap(),
            serde_json::json!({
                "model":"real", "response":{"model":"real", "usage":{"input_tokens":3,"input_tokens_details":{"cached_tokens":1}}}
            })
        );
    }
}
