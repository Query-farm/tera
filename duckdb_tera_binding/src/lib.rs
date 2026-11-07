use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::ffi::{c_char, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::slice;
use std::str;
use std::sync::{Mutex, OnceLock};

use serde_json::Value;
use tera::{Context, Tera};

/// The name Tera gives a one-off (inline) template; error messages mention it, so inline templates are
/// registered under the same name to keep those messages unchanged.
const INLINE_TEMPLATE_NAME: &str = "__tera_one_off";

#[repr(C)]
pub enum ResultCString {
    Ok(*mut c_char),
    Err(*mut c_char),
}

/// A template loaded by the caller (for `template_path`): its name and its source text.
#[repr(C)]
pub struct TemplateSource {
    pub name: *const c_char,
    pub name_len: usize,
    pub source: *const c_char,
    pub source_len: usize,
}

/// A parsed template set that can be rendered any number of times.
///
/// For an inline template it holds that single template; for `template_path` it holds every template
/// the caller loaded, and the template to render is chosen by name for each call.
pub struct CompiledTemplate {
    tera: Tera,
    inline: bool,
}

#[repr(C)]
pub enum CompileResult {
    Ok(*mut CompiledTemplate),
    Err(*mut c_char),
}

/// Borrows `len` bytes at `ptr` as a `&str`. The caller (DuckDB) only passes valid UTF-8 VARCHAR data;
/// invalid UTF-8 is replaced rather than trusted.
unsafe fn borrow_str<'a>(ptr: *const c_char, len: usize) -> std::borrow::Cow<'a, str> {
    if ptr.is_null() || len == 0 {
        return std::borrow::Cow::Borrowed("");
    }
    // `c_char` is `i8` on some platforms, so the cast is not always a no-op
    #[allow(clippy::unnecessary_cast)]
    let bytes = slice::from_raw_parts(ptr as *const u8, len);
    String::from_utf8_lossy(bytes)
}

fn into_c_string(s: String) -> *mut c_char {
    // Interior NUL bytes cannot be represented in a C string; drop them rather than panic across the FFI.
    let bytes: Vec<u8> = s.into_bytes().into_iter().filter(|&b| b != 0).collect();
    // SAFETY: every NUL byte was removed above.
    unsafe { CString::from_vec_unchecked(bytes) }.into_raw()
}

/// Describes a caught panic so it can be reported as an ordinary error instead of unwinding into C++.
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    let detail = if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    };
    format!("Tera internal error: {}", detail)
}

/// Formats a Tera error together with its chain of causes.
fn format_tera_error(prefix: &str, error: &tera::Error) -> String {
    let mut messages = vec![format!("{}: {}", prefix, error)];
    let mut source_opt = error.source();
    while let Some(source) = source_opt {
        messages.push(format!("Caused by: {}", source));
        source_opt = source.source();
    }
    messages.join("\n")
}

/// Returns a `'static` copy of `s`. Tera's `autoescape_on` only accepts `&'static str`; the distinct
/// suffixes a session uses are few, so each is leaked once and shared from then on.
fn intern(s: &str) -> &'static str {
    static POOL: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let mut pool = POOL
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(&existing) = pool.get(s) {
        return existing;
    }
    let leaked: &'static str = Box::leak(s.to_owned().into_boxed_str());
    pool.insert(leaked);
    leaked
}

/// Replaces Tera's built-in `get_env()`, which would let any SQL user read the server's environment
/// (cloud credentials, tokens, ...).
fn disabled_get_env(_args: &HashMap<String, Value>) -> tera::Result<Value> {
    Err(tera::Error::msg(
        "get_env() is disabled; enable it with SET tera_allow_get_env = true (requires enable_external_access)",
    ))
}

/// The longest chain of `{% include %}`s followed while rendering: Tera has no limit of its own, and a
/// template that (indirectly) includes itself would otherwise recurse until the stack overflows.
const MAX_INCLUDE_DEPTH: usize = 32;

fn collect_includes(nodes: &[tera::ast::Node], out: &mut Vec<String>) {
    use tera::ast::Node;
    for node in nodes {
        match node {
            Node::Include(_, names, _) => out.extend(names.iter().cloned()),
            Node::Block(_, block, _) => collect_includes(&block.body, out),
            Node::Forloop(_, forloop, _) => {
                collect_includes(&forloop.body, out);
                if let Some(empty) = &forloop.empty_body {
                    collect_includes(empty, out);
                }
            }
            Node::If(cond, _) => {
                for (_, _, body) in &cond.conditions {
                    collect_includes(body, out);
                }
                if let Some((_, body)) = &cond.otherwise {
                    collect_includes(body, out);
                }
            }
            Node::FilterSection(_, section, _) => collect_includes(&section.body, out),
            Node::MacroDefinition(_, def, _) => collect_includes(&def.body, out),
            _ => {}
        }
    }
}

/// Rejects template sets whose `{% include %}` / `{% import %}` graph has a cycle (or is deeper than MAX_INCLUDE_DEPTH),
/// since rendering them would overflow the stack and abort the process.
fn check_includes(tera: &Tera) -> Result<(), String> {
    let mut graph: HashMap<&str, Vec<String>> = HashMap::new();
    for (name, template) in &tera.templates {
        let mut includes = Vec::new();
        collect_includes(&template.ast, &mut includes);
        for macro_def in template.macros.values() {
            collect_includes(&macro_def.body, &mut includes);
        }
        // Tera loads imported macro files recursively, without a cycle check of its own
        includes.extend(
            template
                .imported_macro_files
                .iter()
                .map(|(file, _)| file.clone()),
        );
        graph.insert(name.as_str(), includes);
    }

    fn visit<'a>(
        name: &'a str,
        graph: &'a HashMap<&'a str, Vec<String>>,
        stack: &mut Vec<&'a str>,
        done: &mut HashSet<&'a str>,
    ) -> Result<(), String> {
        if done.contains(name) {
            return Ok(());
        }
        if stack.contains(&name) {
            return Err(format!(
                "Template '{}' includes itself (include/import chain: {} -> {})",
                name,
                stack.join(" -> "),
                name
            ));
        }
        if stack.len() >= MAX_INCLUDE_DEPTH {
            return Err(format!(
                "Templates are included more than {} levels deep",
                MAX_INCLUDE_DEPTH
            ));
        }
        if let Some((key, children)) = graph.get_key_value(name) {
            stack.push(key);
            for child in children {
                visit(child.as_str(), graph, stack, done)?;
            }
            stack.pop();
            done.insert(key);
        }
        Ok(())
    }

    let mut done = HashSet::new();
    for name in graph.keys() {
        visit(name, &graph, &mut Vec::new(), &mut done)?;
    }
    Ok(())
}

/// Tera's parser and renderer are recursive, so a template that nests deeply enough overflows the
/// stack and aborts the whole process (DuckDB's worker threads can have stacks as small as 512 KiB).
/// These limits are checked on the template text before Tera sees it; they are far above what real
/// templates use.
const MAX_BLOCK_NESTING: usize = 64;
const MAX_BRACKET_NESTING: usize = 64;
const MAX_OPERATORS_PER_TAG: usize = 256;

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Checks the nesting of one `{{ ... }}` / `{% ... %}` tag body and returns its first word.
fn check_tag(name: &str, body: &[u8]) -> Result<String, String> {
    let mut depth = 0usize;
    let mut operators = 0usize;
    let mut i = 0;
    let mut first_word = String::new();
    while i < body.len() {
        let b = body[i];
        match b {
            b'"' | b'\'' | b'`' => {
                // Skip a string literal
                i += 1;
                while i < body.len() && body[i] != b {
                    i += 1;
                }
            }
            b'(' | b'[' | b'{' => {
                depth += 1;
                if depth > MAX_BRACKET_NESTING {
                    return Err(format!(
                        "Template '{}' nests brackets more than {} levels deep",
                        name, MAX_BRACKET_NESTING
                    ));
                }
            }
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b'+' | b'-' | b'*' | b'/' | b'%' | b'~' | b'<' | b'>' | b'=' | b'!' | b'|' | b',' => {
                operators += 1
            }
            _ if is_ident_byte(b) => {
                let start = i;
                while i < body.len() && is_ident_byte(body[i]) {
                    i += 1;
                }
                let word = &body[start..i];
                if first_word.is_empty()
                    && start
                        == body
                            .iter()
                            .position(|c| !c.is_ascii_whitespace())
                            .unwrap_or(0)
                {
                    first_word = String::from_utf8_lossy(word).into_owned();
                }
                if matches!(word, b"and" | b"or" | b"not" | b"in" | b"is") {
                    operators += 1;
                }
                continue;
            }
            _ => {}
        }
        if operators > MAX_OPERATORS_PER_TAG {
            return Err(format!(
                "Template '{}' has an expression with more than {} operators",
                name, MAX_OPERATORS_PER_TAG
            ));
        }
        i += 1;
    }
    Ok(first_word)
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// Rejects templates nested deeply enough to overflow the stack while Tera parses or renders them.
fn check_template_complexity(name: &str, source: &str) -> Result<(), String> {
    let src = source.as_bytes();
    let mut block_depth = 0usize;
    let mut i = 0;
    while let Some(open) = find(src, b"{", i) {
        let next = src.get(open + 1).copied();
        let close: &[u8] = match next {
            Some(b'{') => b"}}",
            Some(b'%') => b"%}",
            Some(b'#') => b"#}",
            _ => {
                i = open + 1;
                continue;
            }
        };
        let body_start = open + 2;
        // Find the end of the tag, skipping string literals inside it
        let mut j = body_start;
        let mut end = None;
        while j + 1 < src.len() {
            let b = src[j];
            if next != Some(b'#') && (b == b'"' || b == b'\'' || b == b'`') {
                j += 1;
                while j < src.len() && src[j] != b {
                    j += 1;
                }
                j += 1;
                continue;
            }
            if src[j..].starts_with(close) {
                end = Some(j);
                break;
            }
            j += 1;
        }
        // An unterminated tag is left for Tera to report
        let Some(body_end) = end else { break };
        i = body_end + 2;
        if next == Some(b'#') {
            continue;
        }
        let body = &src[body_start..body_end];
        let body = body.strip_prefix(b"-").unwrap_or(body);
        let body = body.strip_suffix(b"-").unwrap_or(body);
        let first_word = check_tag(name, body)?;
        if next == Some(b'%') {
            match first_word.as_str() {
                "if" | "for" | "block" | "macro" | "filter" => {
                    block_depth += 1;
                    if block_depth > MAX_BLOCK_NESTING {
                        return Err(format!(
                            "Template '{}' nests tags more than {} levels deep",
                            name, MAX_BLOCK_NESTING
                        ));
                    }
                }
                "endif" | "endfor" | "endblock" | "endmacro" | "endfilter" => {
                    block_depth = block_depth.saturating_sub(1);
                }
                "raw" => {
                    // The content of a raw block is literal text
                    match find(src, b"endraw", i) {
                        Some(p) => i = p,
                        None => break,
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// Collects the macro calls in an expression.
fn collect_macro_calls_expr<'a>(
    expr: &'a tera::ast::Expr,
    out: &mut Vec<&'a tera::ast::MacroCall>,
) {
    collect_macro_calls_val(&expr.val, out);
    for filter in &expr.filters {
        for arg in filter.args.values() {
            collect_macro_calls_expr(arg, out);
        }
    }
}

fn collect_macro_calls_val<'a>(
    val: &'a tera::ast::ExprVal,
    out: &mut Vec<&'a tera::ast::MacroCall>,
) {
    use tera::ast::ExprVal;
    match val {
        ExprVal::Math(e) => {
            collect_macro_calls_expr(&e.lhs, out);
            collect_macro_calls_expr(&e.rhs, out);
        }
        ExprVal::Logic(e) => {
            collect_macro_calls_expr(&e.lhs, out);
            collect_macro_calls_expr(&e.rhs, out);
        }
        ExprVal::In(e) => {
            collect_macro_calls_expr(&e.lhs, out);
            collect_macro_calls_expr(&e.rhs, out);
        }
        ExprVal::Test(t) => t.args.iter().for_each(|a| collect_macro_calls_expr(a, out)),
        ExprVal::MacroCall(call) => {
            out.push(call);
            call.args
                .values()
                .for_each(|a| collect_macro_calls_expr(a, out));
        }
        ExprVal::FunctionCall(call) => call
            .args
            .values()
            .for_each(|a| collect_macro_calls_expr(a, out)),
        ExprVal::Array(items) => items.iter().for_each(|a| collect_macro_calls_expr(a, out)),
        ExprVal::StringConcat(concat) => concat
            .values
            .iter()
            .for_each(|v| collect_macro_calls_val(v, out)),
        _ => {}
    }
}

fn collect_macro_calls<'a>(nodes: &'a [tera::ast::Node], out: &mut Vec<&'a tera::ast::MacroCall>) {
    use tera::ast::Node;
    for node in nodes {
        match node {
            Node::VariableBlock(_, expr) => collect_macro_calls_expr(expr, out),
            Node::Set(_, set) => collect_macro_calls_expr(&set.value, out),
            Node::FilterSection(_, section, _) => {
                section
                    .filter
                    .args
                    .values()
                    .for_each(|a| collect_macro_calls_expr(a, out));
                collect_macro_calls(&section.body, out);
            }
            Node::Block(_, block, _) => collect_macro_calls(&block.body, out),
            Node::Forloop(_, forloop, _) => {
                collect_macro_calls_expr(&forloop.container, out);
                collect_macro_calls(&forloop.body, out);
                if let Some(empty) = &forloop.empty_body {
                    collect_macro_calls(empty, out);
                }
            }
            Node::If(cond, _) => {
                for (_, expr, body) in &cond.conditions {
                    collect_macro_calls_expr(expr, out);
                    collect_macro_calls(body, out);
                }
                if let Some((_, body)) = &cond.otherwise {
                    collect_macro_calls(body, out);
                }
            }
            Node::MacroDefinition(_, def, _) => {
                def.args
                    .values()
                    .flatten()
                    .for_each(|a| collect_macro_calls_expr(a, out));
                collect_macro_calls(&def.body, out);
            }
            _ => {}
        }
    }
}

/// Rejects recursive macros (directly or through other macros). Tera puts no limit on macro
/// recursion, so a macro that calls itself without terminating overflows the stack and aborts the
/// process.
fn check_macro_recursion(tera: &Tera) -> Result<(), String> {
    // Nodes are (template name, macro name)
    let mut graph: HashMap<(&str, &str), Vec<(&str, &str)>> = HashMap::new();
    for (tpl_name, template) in &tera.templates {
        for (macro_name, def) in &template.macros {
            let mut calls = Vec::new();
            collect_macro_calls(&def.body, &mut calls);
            def.args
                .values()
                .flatten()
                .for_each(|a| collect_macro_calls_expr(a, &mut calls));
            let mut targets = Vec::new();
            for call in calls {
                if call.namespace == "self" {
                    targets.push((tpl_name.as_str(), call.name.as_str()));
                    continue;
                }
                // The templates imported under this namespace (by the macro's own template, or, as
                // Tera also resolves namespaces through the calling template, by any template)
                let mut files: Vec<&str> = template
                    .imported_macro_files
                    .iter()
                    .filter(|(_, ns)| *ns == call.namespace)
                    .map(|(file, _)| file.as_str())
                    .collect();
                if files.is_empty() {
                    files = tera
                        .templates
                        .values()
                        .flat_map(|t| t.imported_macro_files.iter())
                        .filter(|(_, ns)| *ns == call.namespace)
                        .map(|(file, _)| file.as_str())
                        .collect();
                }
                for file in files {
                    targets.push((file, call.name.as_str()));
                }
            }
            graph.insert((tpl_name.as_str(), macro_name.as_str()), targets);
        }
    }

    fn visit<'a>(
        node: (&'a str, &'a str),
        graph: &HashMap<(&'a str, &'a str), Vec<(&'a str, &'a str)>>,
        stack: &mut Vec<(&'a str, &'a str)>,
        done: &mut HashSet<(&'a str, &'a str)>,
    ) -> Result<(), String> {
        if done.contains(&node) {
            return Ok(());
        }
        if stack.contains(&node) {
            return Err(format!(
                "Recursive macros are not supported: macro '{}' in template '{}' calls itself",
                node.1, node.0
            ));
        }
        let Some(children) = graph.get(&node) else {
            return Ok(());
        };
        stack.push(node);
        for &child in children {
            visit(child, graph, stack, done)?;
        }
        stack.pop();
        done.insert(node);
        Ok(())
    }

    let mut done = HashSet::new();
    for &node in graph.keys() {
        visit(node, &graph, &mut Vec::new(), &mut done)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn compile_inner(
    template_source: *const c_char,
    template_source_len: usize,
    templates: *const TemplateSource,
    template_count: usize,
    autoescape: bool,
    autoescape_on: *const *const c_char,
    autoescape_on_count: usize,
    allow_get_env: bool,
) -> Result<CompiledTemplate, String> {
    let mut tera = Tera::default();
    if !allow_get_env {
        tera.register_function("get_env", disabled_get_env);
    }

    let inline = template_count == 0 || templates.is_null();
    if inline {
        let source = unsafe { borrow_str(template_source, template_source_len) };
        check_template_complexity(INLINE_TEMPLATE_NAME, &source)?;
        tera.add_raw_template(INLINE_TEMPLATE_NAME, &source)
            .map_err(|e| format_tera_error("Tera render error", &e))?;
    } else {
        let entries = unsafe { slice::from_raw_parts(templates, template_count) };
        let mut sources = Vec::with_capacity(entries.len());
        for entry in entries {
            let name = unsafe { borrow_str(entry.name, entry.name_len) }.into_owned();
            let source = unsafe { borrow_str(entry.source, entry.source_len) }.into_owned();
            check_template_complexity(&name, &source)?;
            sources.push((name, source));
        }
        tera.add_raw_templates(sources)
            .map_err(|e| format_tera_error("Template loading error", &e))?;
    }
    check_includes(&tera)?;
    check_macro_recursion(&tera)?;

    // Tera decides whether to escape a template by matching its name against a list of suffixes. The
    // empty suffix matches every name.
    if !autoescape {
        tera.autoescape_on(vec![]);
    } else if inline || autoescape_on_count == 0 || autoescape_on.is_null() {
        tera.autoescape_on(vec![""]);
    } else {
        let ptrs: &[*const c_char] =
            unsafe { slice::from_raw_parts(autoescape_on, autoescape_on_count) };
        let suffixes: Vec<&'static str> = ptrs
            .iter()
            .filter(|p| !p.is_null())
            .map(|&p| intern(&unsafe { CStr::from_ptr(p) }.to_string_lossy()))
            .collect();
        tera.autoescape_on(suffixes);
    }

    Ok(CompiledTemplate { tera, inline })
}

/// Parses a template (or a set of templates) once so it can be rendered for many rows.
///
/// # Arguments
/// * `template_source` - The inline template text. Ignored when `template_count` is non-zero.
/// * `templates` / `template_count` - Named templates loaded by the caller for `template_path`; the
///   template to render is then chosen by name in `render_compiled_template`.
/// * `autoescape` - Whether to HTML-escape the output of `{{ ... }}` expressions.
/// * `autoescape_on` - Suffixes that restrict escaping to named templates ending with one of them. When
///   empty, `autoescape` escapes every template. Inline templates have no name, so they do not apply.
/// * `allow_get_env` - Whether Tera's `get_env()` function may read environment variables.
///
/// # Returns
/// * `Ok(handle)` to pass to `render_compiled_template` and finally `free_compiled_template`, or
///   `Err(message)` to free with `free_compile_error`.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn compile_template(
    template_source: *const c_char,
    template_source_len: usize,
    templates: *const TemplateSource,
    template_count: usize,
    autoescape: bool,
    autoescape_on: *const *const c_char,
    autoescape_on_count: usize,
    allow_get_env: bool,
) -> CompileResult {
    let result = catch_unwind(AssertUnwindSafe(|| {
        compile_inner(
            template_source,
            template_source_len,
            templates,
            template_count,
            autoescape,
            autoescape_on,
            autoescape_on_count,
            allow_get_env,
        )
    }));
    match result {
        Ok(Ok(compiled)) => CompileResult::Ok(Box::into_raw(Box::new(compiled))),
        Ok(Err(message)) => CompileResult::Err(into_c_string(message)),
        Err(payload) => CompileResult::Err(into_c_string(panic_message(payload))),
    }
}

fn render_inner(
    compiled: *const CompiledTemplate,
    template_name: *const c_char,
    template_name_len: usize,
    json_context: *const c_char,
    json_context_len: usize,
) -> Result<String, String> {
    if compiled.is_null() {
        return Err("Tera internal error: no compiled template".to_string());
    }
    let compiled = unsafe { &*compiled };
    let json_str = unsafe { borrow_str(json_context, json_context_len) };

    let context_value: Value =
        serde_json::from_str(&json_str).map_err(|e| format!("Invalid JSON: {}", e))?;
    let map = match context_value {
        Value::Object(map) => map,
        other => {
            let kind = match other {
                Value::Null => "null",
                Value::Bool(_) => "a boolean",
                Value::Number(_) => "a number",
                Value::String(_) => "a string",
                Value::Array(_) => "an array",
                Value::Object(_) => unreachable!(),
            };
            return Err(format!(
                "Invalid context: expected a JSON object, got {}",
                kind
            ));
        }
    };
    let mut context = Context::new();
    for (key, value) in map {
        context.insert(key, &value);
    }

    let name_buf;
    let name = if compiled.inline {
        INLINE_TEMPLATE_NAME
    } else {
        name_buf = unsafe { borrow_str(template_name, template_name_len) };
        &name_buf
    };

    compiled
        .tera
        .render(name, &context)
        .map_err(|e| format_tera_error("Tera render error", &e))
}

/// Renders a compiled template with context variables provided as a JSON object.
///
/// # Arguments
/// * `compiled` - A handle returned by `compile_template`.
/// * `template_name` - For `template_path` templates, the name of the template to render. Ignored for
///   inline templates.
/// * `json_context` - JSON object text containing the context variables.
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
    let result = catch_unwind(AssertUnwindSafe(|| {
        render_inner(
            compiled,
            template_name,
            template_name_len,
            json_context,
            json_context_len,
        )
    }));
    match result {
        Ok(Ok(output)) => ResultCString::Ok(into_c_string(output)),
        Ok(Err(message)) => ResultCString::Err(into_c_string(message)),
        Err(payload) => ResultCString::Err(into_c_string(panic_message(payload))),
    }
}

/// Frees a template returned by `compile_template`.
///
/// # Safety
/// `compiled` must have been returned by `compile_template` and must not be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn free_compiled_template(compiled: *mut CompiledTemplate) {
    if !compiled.is_null() {
        let _ = catch_unwind(AssertUnwindSafe(|| drop(Box::from_raw(compiled))));
    }
}

/// Frees the error message of a failed `compile_template`.
///
/// # Safety
/// `message` must be the `Err` payload of a `CompileResult` and must not be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn free_compile_error(message: *mut c_char) {
    if !message.is_null() {
        let _ = catch_unwind(AssertUnwindSafe(|| drop(CString::from_raw(message))));
    }
}

/// Frees the memory allocated for a ResultCString.
///
/// # Safety
/// The ResultCString must have been created by this library, must not be used afterwards, and this
/// function must be called exactly once for it.
#[no_mangle]
pub unsafe extern "C" fn free_result_cstring(result: ResultCString) {
    let ptr = match result {
        ResultCString::Ok(ptr) => ptr,
        ResultCString::Err(ptr) => ptr,
    };
    if !ptr.is_null() {
        let _ = catch_unwind(AssertUnwindSafe(|| drop(CString::from_raw(ptr))));
    }
}
