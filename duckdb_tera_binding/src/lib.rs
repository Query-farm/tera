use std::slice;
use std::str;

use serde_json::Value;
use tera::{Context, Tera};
use std::error::Error;
use std::ffi::{c_char, CString, CStr};


#[repr(C)]
pub enum ResultCString {
    Ok(*mut c_char),
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


/// Renders a Tera template from a file or a string with context variables provided as JSON.
///
/// # Arguments
/// * `template_source` - Either the filename of the template or the template content as a string.
/// * `from_file` - If true, treat `template_source` as a filename, otherwise as template content.
/// * `json_context` - JSON string containing context variables.
///
/// # Returns
/// * `Ok(String)` containing the rendered template, or `Err(tera::Error)` if rendering fails.
#[no_mangle]
pub extern "C" fn render_template(
    template_source: *const c_char,
    template_source_len: usize,
    json_context: *const c_char,
    json_context_len: usize,
    template_path: *const c_char,
    autoescape: bool,
    autoescape_on: *const *const c_char,
    autoescape_on_count: usize
) -> ResultCString {

    let template_str = make_str!(template_source, template_source_len);
    let json_str = make_str!(json_context, json_context_len);
    let template_path_str = c_char_to_string(template_path);

    // Parse the JSON string into a serde_json::Value
    let context_value: Value = match serde_json::from_str(json_str) {
        Ok(val) => val,
        Err(e) => {
            let error_msg = format!("Invalid JSON: {}", e);
            let error_str = CString::new(error_msg).unwrap();
            return ResultCString::Err(error_str.into_raw());
        }
    };

    // Convert the JSON Value into a Tera Context
    let mut context = Context::new();
    if let Value::Object(map) = context_value {
        for (key, value) in map {
            context.insert(&key, &value);
        }
    }


    // Render the template

    let result = match template_path_str {
        Some(ref path) if !path.is_empty() => {

        let mut tera = match Tera::new(path.as_str()) {
            Ok(t) => t,
            Err(e) => {
                let error_msg = format!("Template loading error: {}", e);
                let error_str = CString::new(error_msg).unwrap();
                return ResultCString::Err(error_str.into_raw());
            }
        };


        if !autoescape || autoescape_on_count == 0 {
            tera.autoescape_on(vec![]);
        } else if autoescape && autoescape_on_count > 0 {

            unsafe {
                // create a slice of *const c_char
                let slice: &[*const c_char] = std::slice::from_raw_parts(autoescape_on, autoescape_on_count);

                // convert each C string to &str
                let mut autoescape_on_vec: Vec<&str> = Vec::with_capacity(autoescape_on_count);
                for &ptr in slice {
                    if ptr.is_null() {
                        continue;
                    }
                    let s = CStr::from_ptr(ptr).to_str().unwrap_or_default();
                    autoescape_on_vec.push(s);
                }

                // Transmute the lifetime to 'static

                tera.autoescape_on(std::mem::transmute::<Vec<&str>, Vec<&'static str>>(autoescape_on_vec));
            }
        }

        // Get the first template name and render it
        tera.render(template_str, &context)

        }
        _ => {
            // Render from string directly
        Tera::one_off(template_str, &context, autoescape)

        }
    };

    match result {
        Ok(output) => {
            let value_str = CString::new(output).unwrap();
            ResultCString::Ok(value_str.into_raw())
        }
        Err(error) => {
                        // Build a detailed error message
            let mut messages = vec![format!("Tera render error: {}", error)];

            // Include nested sources if any
            let mut source_opt = error.source();
            while let Some(source) = source_opt {
                messages.push(format!("Caused by: {}", source));
                source_opt = source.source();
            }

            let formatted_error = messages.join("\n");

            let error_str = CString::new(formatted_error).unwrap();
            ResultCString::Err(error_str.into_raw())
        }
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

