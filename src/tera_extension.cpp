#define DUCKDB_EXTENSION_MAIN

#include "tera_extension.hpp"
#include "duckdb.hpp"
#include "duckdb/common/exception.hpp"
#include "duckdb/function/scalar_function.hpp"
#include "duckdb/parser/parsed_data/create_scalar_function_info.hpp"
#include "duckdb/parser/parsed_data/create_function_info.hpp"
#include "duckdb/planner/expression/bound_function_expression.hpp"
#include "duckdb/common/vector/string_vector.hpp"
#include "duckdb/execution/expression_executor_state.hpp"

#include "rust.h"
#include "query_farm_telemetry.hpp"

namespace duckdb {

// Extension version - update this when releasing new versions
constexpr const char *TERA_EXTENSION_VERSION = "2026102003";

// The options of both tera_render overloads. They are keyword-only parameters, so DuckDB matches the names,
// casts the values to the declared types and fills in the defaults before the bind callback runs.
static void AddRenderOptions(ScalarFunction &function) {
	function.GetSignature()
	    .AddKeywordOnly("autoescape", LogicalType::BOOLEAN, Value::BOOLEAN(true))
	    .AddKeywordOnly("template_path", LogicalType::VARCHAR, Value(""))
	    .AddKeywordOnly("autoescape_extensions", LogicalType::LIST(LogicalType::VARCHAR),
	                    Value::LIST(LogicalType::VARCHAR, vector<Value>()));
}

struct TeraRenderBindData : public FunctionData {
	bool has_context;
	string template_path;
	bool autoescape = true;
	vector<string> autoescape_on;

	TeraRenderBindData(bool has_context_p, string template_path_p, bool autoescape_p, vector<string> autoescape_on_p)
	    : has_context(has_context_p), template_path(std::move(template_path_p)), autoescape(autoescape_p),
	      autoescape_on(std::move(autoescape_on_p)) {
	}

	unique_ptr<FunctionData> Copy() const override {
		return make_uniq<TeraRenderBindData>(has_context, template_path, autoescape, autoescape_on);
	}

	bool Equals(const FunctionData &other_p) const override {
		auto &other = other_p.Cast<TeraRenderBindData>();
		return has_context == other.has_context && template_path == other.template_path &&
		       autoescape == other.autoescape && autoescape_on == other.autoescape_on;
	}
};

static unique_ptr<FunctionData> TeraRenderBindInternal(BindScalarFunctionInput &input, bool has_context) {
	const auto autoescape = BooleanValue::Get(input.GetNonNullConstant("autoescape"));
	auto template_path = StringValue::Get(input.GetNonNullConstant("template_path"));

	// Keep the list value alive while iterating: ListValue::GetChildren returns a reference into it.
	const auto extensions = input.GetNonNullConstant("autoescape_extensions");
	vector<string> autoescape_on;
	for (const auto &extension : ListValue::GetChildren(extensions)) {
		if (extension.IsNull()) {
			throw BinderException("tera_render: 'autoescape_extensions' must not contain NULL");
		}
		autoescape_on.push_back(StringValue::Get(extension));
	}

	return make_uniq<TeraRenderBindData>(has_context, std::move(template_path), autoescape, std::move(autoescape_on));
}

static unique_ptr<FunctionData> TeraRenderBind(BindScalarFunctionInput &input) {
	return TeraRenderBindInternal(input, false);
}

static unique_ptr<FunctionData> TeraRenderWithContextBind(BindScalarFunctionInput &input) {
	return TeraRenderBindInternal(input, true);
}

struct CompiledTemplateDeleter {
	void operator()(CompiledTemplate *compiled) const {
		free_compiled_template(compiled);
	}
};
using CompiledTemplatePtr = std::unique_ptr<CompiledTemplate, CompiledTemplateDeleter>;

// Parsing a template (and, for template_path, loading the files) costs far more than rendering it, so each
// thread keeps the templates it has parsed for the duration of the query and renders every row from them.
struct TeraRenderLocalState : public FunctionLocalState {
	//! Inline templates, keyed by template text. A constant template argument is parsed once; a template
	//! that differs between rows is cached up to MAX_INLINE_TEMPLATES distinct texts at a time.
	static constexpr idx_t MAX_INLINE_TEMPLATES = 64;
	unordered_map<string, CompiledTemplatePtr> inline_templates;
	//! The most recently used inline template, checked before the map
	string last_template;
	optional_ptr<CompiledTemplate> last_compiled;
	//! The templates loaded from template_path; loaded on the first non-NULL row
	CompiledTemplatePtr path_templates;
};

static unique_ptr<FunctionLocalState>
TeraRenderInitLocalState(ExpressionState &state, const BoundFunctionExpression &expr, FunctionData *bind_data) {
	return make_uniq<TeraRenderLocalState>();
}

static CompiledTemplatePtr CompileTemplate(const TeraRenderBindData &bind_data, const char *source, idx_t source_len) {
	vector<const char *> autoescape_on_ptrs;
	for (const auto &s : bind_data.autoescape_on) {
		autoescape_on_ptrs.push_back(s.c_str());
	}
	CompileResult compiled = compile_template(source, source_len, bind_data.template_path.c_str(), bind_data.autoescape,
	                                          autoescape_on_ptrs.data(), autoescape_on_ptrs.size());
	if (compiled.tag == CompileResult::Tag::Err) {
		string err_str = string(compiled.err._0);
		free_compile_error(compiled.err._0);
		throw InvalidInputException("Error rendering template: " + err_str);
	}
	return CompiledTemplatePtr(compiled.ok._0);
}

static CompiledTemplate &GetCompiledTemplate(TeraRenderLocalState &lstate, const TeraRenderBindData &bind_data,
                                             string_t expression) {
	if (!bind_data.template_path.empty()) {
		// The template argument names a template within the loaded set
		if (!lstate.path_templates) {
			lstate.path_templates = CompileTemplate(bind_data, "", 0);
		}
		return *lstate.path_templates;
	}

	if (lstate.last_compiled && expression.GetSize() == lstate.last_template.size() &&
	    memcmp(expression.GetData(), lstate.last_template.data(), expression.GetSize()) == 0) {
		return *lstate.last_compiled;
	}
	auto key = expression.GetString();
	auto entry = lstate.inline_templates.find(key);
	if (entry == lstate.inline_templates.end()) {
		auto compiled = CompileTemplate(bind_data, expression.GetData(), expression.GetSize());
		if (lstate.inline_templates.size() >= TeraRenderLocalState::MAX_INLINE_TEMPLATES) {
			lstate.inline_templates.clear();
		}
		entry = lstate.inline_templates.emplace(key, std::move(compiled)).first;
	}
	lstate.last_template = std::move(key);
	lstate.last_compiled = entry->second.get();
	return *lstate.last_compiled;
}

static string_t RenderToVector(Vector &result, TeraRenderLocalState &lstate, const TeraRenderBindData &bind_data,
                               string_t expression, const char *context_json, idx_t context_json_len) {
	auto &compiled = GetCompiledTemplate(lstate, bind_data, expression);
	ResultCString eval_result =
	    render_compiled_template(&compiled, expression.GetData(), expression.GetSize(), context_json, context_json_len);
	if (eval_result.tag == ResultCString::Tag::Err) {
		string err_str = string(eval_result.err._0);
		free_result_cstring(eval_result);
		throw InvalidInputException("Error rendering template: " + err_str);
	}
	auto vector_result = StringVector::AddString(result, eval_result.ok._0);
	free_result_cstring(eval_result);
	return vector_result;
}

inline void TeraRenderFunc(DataChunk &args, ExpressionState &state, Vector &result) {
	const auto &func_expr = state.expr.Cast<BoundFunctionExpression>();
	const auto &bind_data = func_expr.BindInfo()->Cast<TeraRenderBindData>();
	auto &lstate = ExecuteFunctionState::GetFunctionState(state)->Cast<TeraRenderLocalState>();

	// The keyword-only options follow the positional arguments in the chunk; their values were captured at bind.
	auto &expression_vector = args.data[0];

	if (bind_data.has_context) {
		auto &context_json_vector = args.data[1];
		BinaryExecutor::Execute<string_t, string_t, string_t>(
		    expression_vector, context_json_vector, result, [&](string_t expression, string_t context_json) {
			    return RenderToVector(result, lstate, bind_data, expression, context_json.GetData(),
			                          context_json.GetSize());
		    });
	} else {
		UnaryExecutor::Execute<string_t, string_t>(expression_vector, result, [&](string_t expression) {
			return RenderToVector(result, lstate, bind_data, expression, "{}", 2);
		});
	}
}

static FunctionDescription MakeDescription(vector<string> names, vector<LogicalType> types, string description,
                                           vector<string> examples) {
	FunctionDescription desc;
	// The keyword-only options, in the order AddRenderOptions declares them
	names.insert(names.end(), {"autoescape", "template_path", "autoescape_extensions"});
	types.insert(types.end(), {LogicalType::BOOLEAN, LogicalType::VARCHAR, LogicalType::LIST(LogicalType::VARCHAR)});
	desc.parameter_names = std::move(names);
	desc.parameter_types = std::move(types);
	desc.description = std::move(description);
	desc.examples = std::move(examples);
	desc.categories = {"text", "template"};
	return desc;
}

// Extension initalization.
static void LoadInternal(ExtensionLoader &loader) {
	{
		ScalarFunctionSet render("tera_render");

		// tera_render(template, context, *, autoescape, template_path, autoescape_extensions)
		ScalarFunction render_with_context({}, LogicalType::VARCHAR, TeraRenderFunc, TeraRenderWithContextBind);
		render_with_context.GetSignature()
		    .AddParameter("template", LogicalType::VARCHAR)
		    .AddParameter("context", LogicalType::JSON());
		AddRenderOptions(render_with_context);
		render_with_context.SetNullHandling(FunctionNullHandling::DEFAULT_NULL_HANDLING);
		render_with_context.SetVolatile();
		render_with_context.SetInitStateCallback(TeraRenderInitLocalState);
		render.AddFunction(render_with_context);

		// tera_render(template, *, autoescape, template_path, autoescape_extensions)
		ScalarFunction render_no_context({}, LogicalType::VARCHAR, TeraRenderFunc, TeraRenderBind);
		render_no_context.GetSignature().AddParameter("template", LogicalType::VARCHAR);
		AddRenderOptions(render_no_context);
		render_no_context.SetNullHandling(FunctionNullHandling::DEFAULT_NULL_HANDLING);
		render_no_context.SetVolatile();
		render_no_context.SetInitStateCallback(TeraRenderInitLocalState);
		render.AddFunction(render_no_context);

		render.SetFallible();
		CreateScalarFunctionInfo info(render);

		static constexpr const char *OPTIONS_HELP =
		    " Options are passed by name: autoescape (default true), template_path (a glob of template files to load, "
		    "default none; the template argument is then a template name) and autoescape_extensions (default []: when "
		    "autoescape is on, every template is escaped; a list restricts "
		    "escaping of template_path templates to file names ending in one of the given suffixes)";

		info.descriptions.push_back(MakeDescription(
		    {"template", "context"}, {LogicalType::VARCHAR, LogicalType::JSON()},
		    string("Renders a Tera template string with JSON context variables.") + OPTIONS_HELP,
		    {"tera_render('Hello {{ name }}!', '{\"name\": \"World\"}')",
		     "tera_render('{{ value | upper }}', '{\"value\": \"hello\"}')",
		     "tera_render('{{ v }}', '{\"v\": \"B&O\"}', autoescape := false)"}));

		info.descriptions.push_back(
		    MakeDescription({"template"}, {LogicalType::VARCHAR},
		                    string("Renders a Tera template string without context variables.") + OPTIONS_HELP,
		                    {"tera_render('Hello World!')"}));

		loader.RegisterFunction(info);
	}

	QueryFarmSendTelemetry(loader, "tera", TERA_EXTENSION_VERSION);
}

void TeraExtension::Load(ExtensionLoader &loader) {
	LoadInternal(loader);
}
std::string TeraExtension::Name() {
	return "tera";
}

std::string TeraExtension::Version() const {
	return TERA_EXTENSION_VERSION;
}

} // namespace duckdb

extern "C" {

DUCKDB_CPP_EXTENSION_ENTRY(tera, loader) {
	duckdb::LoadInternal(loader);
}
}
