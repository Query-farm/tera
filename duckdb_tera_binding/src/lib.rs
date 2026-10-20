use std::slice;
use std::str;

use serde_json::Value;
use tera::{Context, Tera};
use std::error::Error;
use std::ffi::{c_char, CString, CStr};

/// The name Tera gives a one-off (inline) template; error messages mention it, so inline templates are
/// registered under the same name to keep those messages unchanged.
const INLINE_TEMPLATE_NAME: &str = "__tera_one_off";

#[repr(C)]
pub enum ResultCString {
    Ok(*mut c_char),
    Err(*mut c_char),
}

/// A parsed template set that can be rendered any number of times.
///
/// For an inline template it holds that single template; for `template_path` it holds every template
/// matched by the glob, and the template to render is chosen by name for each call.
pub struct CompiledTemplate {
    // `tera` borrows the strings in `_autoescape_suffixes` (see `compile_template`), so it is declared
    // first: struct fields are dropped in declaration order.
    tera: Tera,
    inline: bool,
    _autoescape_suffixes: Vec<String>,
}

#[repr(C)]
pub enum CompileResult {
    Ok(*mut CompiledTemplate),
    Err(*mut c_char),
}

macro_rules! make_str {
    ( $s : expr , $len : expr ) => {
        unsafe { str::from_utf8_unchecked(slice::from_raw_parts($s as *const u8, $len)) }
    };
}

fn c_char_to_string(template_path: *const c_char) -> Option<String> {
    if template_path.is_null() {
        return None;
    }

    unsafe {
        // Convert C string to &CStr
        let c_str = CStr::from_ptr(template_path);
        // Convert &CStr to Rust String (UTF-8)
        match c_str.to_str() {
            Ok(s) => Some(s.to_owned()),
            Err(_) => None, // invalid UTF-8
        }
    }
}

fn into_c_string(s: String) -> *mut c_char {
    // Interior NUL bytes cannot be represented in a C string; drop them rather than panic across the FFI.
    match CString::new(s) {
        Ok(c) => c.into_raw(),
        Err(e) => {
            let mut bytes = e.into_vec();
            bytes.retain(|&b| b != 0);
            CString::new(bytes).unwrap().into_raw()
        }
    }
}

/// Formats a Tera error together with its chain of causes.
fn format_tera_error(error: &tera::Error) -> String {
    let mut messages = vec![format!("Tera render error: {}", error)];
    let mut source_opt = error.source();
    while let Some(source) = source_opt {
        messages.push(format!("Caused by: {}", source));
        source_opt = source.source();
    }
    messages.join("\n")
}

/// Parses a template once so it can be rendered for many rows.
///
/// # Arguments
/// * `template_source` - The template content. Ignored when `template_path` is set.
/// * `template_path` - A glob of template files to load, or NULL/empty to compile `template_source`.
/// * `autoescape` - Whether to HTML-escape the output of `{{ ... }}` expressions.
/// * `autoescape_on` - Suffixes that restrict escaping to `template_path` templates whose file names end
///   with one of them. When empty, `autoescape` escapes every template regardless of its name. Inline
///   templates have no file name, so the suffixes do not apply to them.
///
/// # Returns
/// * `Ok(handle)` to pass to `render_compiled_template` and finally `free_compiled_template`, or
///   `Err(message)` to free with `free_compile_error`.
#[no_mangle]
pub extern "C" fn compile_template(
    template_source: *const c_char,
    template_source_len: usize,
    template_path: *const c_char,
    autoescape: bool,
    autoescape_on: *const *const c_char,
    autoescape_on_count: usize,
) -> CompileResult {
    let template_path_str = c_char_to_string(template_path);

    let (mut tera, inline) = match template_path_str {
        Some(ref path) if !path.is_empty() => match Tera::new(path.as_str()) {
            Ok(t) => (t, false),
            Err(e) => {
                return CompileResult::Err(into_c_string(format!("Template loading error: {}", e)));
            }
        },
        _ => {
            let template_str = make_str!(template_source, template_source_len);
            let mut t = Tera::default();
            if let Err(e) = t.add_raw_template(INLINE_TEMPLATE_NAME, template_str) {
                return CompileResult::Err(into_c_string(format_tera_error(&e)));
            }
            (t, true)
        }
    };

    // Tera decides whether to escape a template by matching its name (or file path) against a list of
    // suffixes. Its default list is [".html", ".htm", ".xml"]; the empty suffix matches every name.
    let mut suffixes: Vec<String> = Vec::new();
    if !autoescape {
        tera.autoescape_on(vec![]);
    } else if inline || autoescape_on_count == 0 {
        tera.autoescape_on(vec![""]);
    } else {
        let ptrs: &[*const c_char] = unsafe { slice::from_raw_parts(autoescape_on, autoescape_on_count) };
        for &ptr in ptrs {
            if ptr.is_null() {
                continue;
            }
            suffixes.push(unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned());
        }
        // SAFETY: the strings are owned by the CompiledTemplate alongside `tera`, are never modified, and
        // outlive it (`tera` is dropped first), so the borrowed slices stay valid for Tera's lifetime.
        let borrowed: Vec<&'static str> = suffixes
            .iter()
            .map(|s| unsafe { std::mem::transmute::<&str, &'static str>(s.as_str()) })
            .collect();
        tera.autoescape_on(borrowed);
    }

    let compiled = Box::new(CompiledTemplate { tera, inline, _autoescape_suffixes: suffixes });
    CompileResult::Ok(Box::into_raw(compiled))
}

/// Renders a compiled template with context variables provided as JSON.
///
/// # Arguments
/// * `compiled` - A handle returned by `compile_template`.
/// * `template_name` - For `template_path` templates, the name of the template to render. Ignored for
///   inline templates.
/// * `json_context` - JSON string containing context variables.
///
/// # Returns
/// * `Ok(String)` containing the rendered template, or `Err(String)` if rendering fails.
#[no_mangle]
pub extern "C" fn render_compiled_template(
    compiled: *const CompiledTemplate,
    template_name: *const c_char,
    template_name_len: usize,
    json_context: *const c_char,
    json_context_len: usize,
) -> ResultCString {
    let compiled = unsafe { &*compiled };
    let json_str = make_str!(json_context, json_context_len);

    // Parse the JSON string into a serde_json::Value
    let context_value: Value = match serde_json::from_str(json_str) {
        Ok(val) => val,
        Err(e) => {
            return ResultCString::Err(into_c_string(format!("Invalid JSON: {}", e)));
        }
    };

    // Convert the JSON Value into a Tera Context
    let mut context = Context::new();
    if let Value::Object(map) = context_value {
        for (key, value) in map {
            context.insert(&key, &value);
        }
    }

    let name = if compiled.inline { INLINE_TEMPLATE_NAME } else { make_str!(template_name, template_name_len) };

    match compiled.tera.render(name, &context) {
        Ok(output) => ResultCString::Ok(into_c_string(output)),
        Err(error) => ResultCString::Err(into_c_string(format_tera_error(&error))),
    }
}

/// Frees a template returned by `compile_template`.
///
/// # Safety
/// `compiled` must have been returned by `compile_template` and must not be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn free_compiled_template(compiled: *mut CompiledTemplate) {
    if !compiled.is_null() {
        drop(Box::from_raw(compiled));
    }
}

/// Frees the error message of a failed `compile_template`.
///
/// # Safety
/// `message` must be the `Err` payload of a `CompileResult` and must not be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn free_compile_error(message: *mut c_char) {
    if !message.is_null() {
        let _ = CString::from_raw(message);
    }
}

/// Frees the memory allocated for a ResultCString.
///
/// # Arguments
/// * `result` - The ResultCString to free
///
/// # Safety
/// This function is unsafe because it takes ownership of raw pointers.
/// The caller must ensure that:
/// - The ResultCString was created by this library
/// - The ResultCString is not used after calling this function
/// - This function is called exactly once for each ResultCString
#[no_mangle]
pub unsafe extern "C" fn free_result_cstring(result: ResultCString) {
    match result {
        ResultCString::Ok(ptr) => {
            if !ptr.is_null() {
                let _ = CString::from_raw(ptr);
            }
        }
        ResultCString::Err(ptr) => {
            if !ptr.is_null() {
                let _ = CString::from_raw(ptr);
            }
        }
    }
}
