// SPDX-License-Identifier: MIT
//! `addJavascriptInterface` over an engine with no synchronous bridge (webview plan S4.1, W2).
//! Android's bridge is synchronous (`var r = Android.f(1)` returns the value) and runs the Java method on a background thread (Chromium's "JavaBridge" thread). WebKit offers only async messages, so each
//! injected method calls the page's `prompt` (captured at injection, so a page override cannot break it) with [`MARKER`] + a JSON call; JavaScript blocks until the provider answers the dialog,
//! which it does from the bridge thread after the Java call. Method selection and argument conversion follow Chromium's rules where they are known (by name and argument count; numbers
//! truncated toward zero for integral types).

use serde_json::Value;

/// Prefix of a bridge call's prompt message; a prompt without it is the page's own.
pub const MARKER: &str = "atl-javabridge:";

/// The text a JavaScript exception thrown for a failed Java call carries (Chromium's wording).
pub const JAVA_EXCEPTION: &str = "Java exception was raised during method invocation";

/// The message of the bootstrap's query prompt: the asking document's view answers [`interface_list`] of its own interfaces (WebView.java:1879: injected into all frames, each frame asks).
pub const QUERY: &str = "atl-javabridge?";

/// The one document-start script, identical for every view (all frames): asks its own view for its interfaces through `prompt` (the prompt reaches the asking document's view, not the
/// controller's other views) and exposes `window[name]` for each. View-agnostic, so a script controller shared by several views (a popup's related view) gives each view exactly its own set.
pub fn bootstrap() -> String {
    format!(
        "(function(){{var P=window.prompt.bind(window),l=P({query},'');if(l===null)return;try{{l=JSON.parse(l)}}catch(e){{return}}         l.forEach(function(x){{var n=x[0],o={{}};x[1].forEach(function(m){{o[m]=function(){{         var r=P({marker}+JSON.stringify({{o:n,m:m,a:Array.prototype.slice.call(arguments)}}),'');         if(r===null)return undefined;var v=JSON.parse(r);if('e' in v)throw new Error(v.e);return v.r;}};}});         Object.defineProperty(window,n,{{value:o,configurable:true,writable:true}});}});}})();",
        query = Value::String(QUERY.to_owned()),
        marker = Value::String(MARKER.to_owned())
    )
}

/// The query's answer: `[[name, [method, ...]], ...]` in `list` order.
pub fn interface_list<'a>(list: impl IntoIterator<Item = (&'a str, &'a [String])>) -> String {
    Value::from(
        list.into_iter()
            .map(|(name, methods)| {
                Value::from(vec![Value::from(name), Value::from(methods.to_vec())])
            })
            .collect::<Vec<_>>(),
    )
    .to_string()
}

/// A parsed bridge call.
#[derive(Debug, PartialEq)]
pub struct Call {
    /// The interface name.
    pub object: String,
    /// The method name.
    pub method: String,
    /// The JavaScript arguments (`undefined` arrives as `null`, JSON's limit).
    pub args: Vec<Value>,
}

/// The call in a prompt message, or None for a page's own prompt.
pub fn parse(message: &str) -> Option<Call> {
    let v: Value = serde_json::from_str(message.strip_prefix(MARKER)?).ok()?;
    Some(Call {
        object: v.get("o")?.as_str()?.to_owned(),
        method: v.get("m")?.as_str()?.to_owned(),
        args: v.get("a")?.as_array()?.clone(),
    })
}

/// A Java argument converted from a JavaScript value for a parameter type (`Class.getName()`).
#[derive(Debug, PartialEq)]
pub enum Arg {
    /// `int`.
    Int(i32),
    /// `long`.
    Long(i64),
    /// `short`.
    Short(i16),
    /// `byte`.
    Byte(i8),
    /// `char`.
    Char(u16),
    /// `float`.
    Float(f32),
    /// `double`.
    Double(f64),
    /// `boolean`.
    Bool(bool),
    /// `java.lang.String`; None = null.
    Str(Option<String>),
    /// Any other reference type: null (injected-object arguments are not supported).
    Null,
}

/// Converts `v` for a parameter of type `ty`. Integral types take a number truncated toward zero (NaN and non-numbers 0); boolean takes a boolean (else false); String takes a string (null for null; other values their JSON text).
pub fn convert(ty: &str, v: &Value) -> Arg {
    let num = v.as_f64().unwrap_or(0.0);
    // `as` saturates and maps NaN to 0: the truncation toward zero the integral cases need
    match ty {
        "int" => Arg::Int(num as i32),
        "long" => Arg::Long(num as i64),
        "short" => Arg::Short(num as i16),
        "byte" => Arg::Byte(num as i8),
        "char" => Arg::Char(num as u16),
        "float" => Arg::Float(num as f32),
        "double" => Arg::Double(num),
        "boolean" => Arg::Bool(v.as_bool().unwrap_or(false)),
        "java.lang.String" => Arg::Str(match v {
            Value::Null => None,
            Value::String(s) => Some(s.clone()),
            other => Some(other.to_string()),
        }),
        _ => Arg::Null,
    }
}

/// The prompt answer for a returned value: `ret` is the JSON of the result, None for a void method (JavaScript gets undefined).
pub fn reply_value(ret: Option<String>) -> String {
    match ret {
        Some(json) => format!("{{\"r\":{json}}}"),
        None => "{}".to_owned(),
    }
}

/// The prompt answer that makes JavaScript throw `message`.
pub fn reply_error(message: &str) -> String {
    format!("{{\"e\":{}}}", Value::String(message.to_owned()))
}

/// The JSON of a boxed Java result given its class name and `toString()`; None when not representable (JavaScript gets undefined, as for an unsupported object return).
pub fn result_json(class: &str, text: &str) -> Option<String> {
    match class {
        "java.lang.String" | "java.lang.Character" => {
            Some(Value::String(text.to_owned()).to_string())
        }
        "java.lang.Boolean" => Some(text.to_owned()),
        "java.lang.Integer" | "java.lang.Long" | "java.lang.Short" | "java.lang.Byte" => {
            Some(text.to_owned())
        }
        // NaN and the infinities have no JSON form
        "java.lang.Float" | "java.lang.Double" => text
            .parse::<f64>()
            .ok()
            .filter(|d| d.is_finite())
            .map(|d| Value::from(d).to_string()),
        _ => None,
    }
}

/// `WebView.loadData`'s URL (WebView.java loadData doc; Chromium builds the same `data:` URL): `data:<mime>[;base64],<data>`, `mime` defaulting to text/html.
pub fn data_url(data: &str, mime: Option<&str>, encoding: Option<&str>) -> String {
    let mime = mime.filter(|m| !m.is_empty()).unwrap_or("text/html");
    let b64 = if encoding.is_some_and(|e| e.eq_ignore_ascii_case("base64")) {
        ";base64"
    } else {
        ""
    };
    format!("data:{mime}{b64},{data}")
}

/// The part of `url` safe to log: `scheme://host[:port]` for a hierarchical URL, `scheme:` otherwise (data:, about:, javascript:). Path, query, fragment and userinfo are left out: they can carry user identifiers.
pub fn log_origin(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once(':') else {
        return "<no scheme>".to_owned();
    };
    let Some(after) = rest.strip_prefix("//") else {
        return format!("{scheme}:");
    };
    let authority = after.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    format!("{scheme}://{host}")
}

/// The script of a `javascript:` URL (scheme case-insensitive, RFC 3986 3.1), percent-decoded as the URL spec's javascript: URL evaluation does; None for any other URL. Bytes that are not
/// UTF-8 after decoding are replaced (U+FFFD). Chromium runs such a URL in the current document (no navigation, the page's load goes on); WebKit's load_uri cancels the load in progress.
pub fn javascript_source(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once(':')?;
    if !scheme.eq_ignore_ascii_case("javascript") {
        return None;
    }
    let b = rest.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        match (
            b[i],
            b.get(i + 1).copied().and_then(hex),
            b.get(i + 2).copied().and_then(hex),
        ) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (c, ..) => {
                out.push(c);
                i += 1;
            }
        }
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// URL spec: a javascript: URL's script is its percent-decoded remainder; a `%` not followed by two hex digits stays.
    #[test]
    fn javascript_source_decodes_the_script() {
        assert_eq!(
            javascript_source("javascript:a(%22x%20y%22)").as_deref(),
            Some("a(\"x y\")")
        );
        assert_eq!(javascript_source("JavaScript:1%2").as_deref(), Some("1%2"));
        assert_eq!(
            javascript_source("javascript:%E4%B8%AD").as_deref(),
            Some("中")
        );
        assert_eq!(javascript_source("https://a.example/javascript:x"), None);
        assert_eq!(javascript_source("about:blank"), None);
    }

    /// Spec: RFC 3986 section 3 (scheme ":" ["//" [userinfo "@"] host [":" port]] path ["?" query] ["#" fragment]).
    #[test]
    fn log_origin_keeps_scheme_and_host_only() {
        assert_eq!(
            log_origin("https://u:p@survey.example.com:8443/s/123?uid=42#x"),
            "https://survey.example.com:8443"
        );
        assert_eq!(log_origin("https://a.example?uid=1"), "https://a.example");
        assert_eq!(log_origin("data:text/html,<p>"), "data:");
        assert_eq!(log_origin("about:blank"), "about:");
        assert_eq!(log_origin("nothing"), "<no scheme>");
    }

    #[test]
    fn parse_reads_the_injected_call() {
        // the message the injected proxy builds for Android.add(1, "x")
        let m = format!("{MARKER}{{\"o\":\"Android\",\"m\":\"add\",\"a\":[1,\"x\"]}}");
        assert_eq!(
            parse(&m),
            Some(Call {
                object: "Android".into(),
                method: "add".into(),
                args: vec![Value::from(1), Value::from("x")]
            })
        );
        assert_eq!(parse("hello"), None);
    }

    #[test]
    fn interface_list_is_the_json_the_bootstrap_reads() {
        let ms = ["f".to_owned(), "g".to_owned()];
        assert_eq!(
            interface_list([("A", &ms[..]), ("B", &[][..])]),
            r#"[["A",["f","g"]],["B",[]]]"#
        );
        assert_eq!(interface_list([]), "[]");
        // the query is not a call: a page prompt with it must not parse as one
        assert_eq!(parse(QUERY), None);
        assert!(bootstrap().contains(&Value::String(QUERY.to_owned()).to_string()));
    }

    #[test]
    fn integral_params_truncate_toward_zero() {
        assert_eq!(convert("int", &Value::from(2.9)), Arg::Int(2));
        assert_eq!(convert("int", &Value::from(-2.9)), Arg::Int(-2));
        assert_eq!(convert("int", &Value::from("7")), Arg::Int(0));
        assert_eq!(convert("boolean", &Value::from(1)), Arg::Bool(false));
        assert_eq!(convert("java.lang.String", &Value::Null), Arg::Str(None));
        assert_eq!(convert("java.lang.Object", &Value::from(1)), Arg::Null);
    }

    #[test]
    fn replies_are_json_the_proxy_reads() {
        assert_eq!(reply_value(Some("3".into())), r#"{"r":3}"#);
        assert_eq!(reply_value(None), "{}");
        assert_eq!(reply_error("x\"y"), r#"{"e":"x\"y"}"#);
        assert_eq!(result_json("java.lang.Double", "NaN"), None);
        assert_eq!(
            result_json("java.lang.String", "a\"b").as_deref(),
            Some(r#""a\"b""#)
        );
    }

    #[test]
    fn data_url_matches_the_loaddata_doc() {
        assert_eq!(data_url("<p>", None, None), "data:text/html,<p>");
        assert_eq!(
            data_url("PHA+", Some("text/html"), Some("base64")),
            "data:text/html;base64,PHA+"
        );
    }
}
