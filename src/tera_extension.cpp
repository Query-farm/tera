#define DUCKDB_EXTENSION_MAIN

#include "tera_extension.hpp"
#include "duckdb.hpp"
#include "duckdb/common/exception.hpp"
#include "duckdb/function/scalar_function.hpp"
#include "duckdb/parser/parsed_data/create_scalar_function_info.hpp"
#include "duckdb/parser/parsed_data/create_function_info.hpp"
#include "duckdb/planner/expression/bound_function_expression.hpp"
#include "duckdb/execution/expression_executor_state.hpp"

#include "rust.h"
#include "query_farm_telemetry.hpp"

namespace duckdb {

// Extension version - update this when releasing new versions
constexpr const char *TERA_EXTENSION_VERSION = "2026102002";

// Tera render bind data structure
struct TeraRenderBindData : public FunctionData {
	string template_path;
	bool autoescape = true;
	vector<string> autoescape_on;
	int optional_args = 0;

	TeraRenderBindData(string template_path_p, bool autoescape_p, vector<string> autoescape_on_p, int optional_args_p)
	    : template_path(std::move(template_path_p)), autoescape(autoescape_p),
	      autoescape_on(std::move(autoescape_on_p)), optional_args(optional_args_p) {
	}

	unique_ptr<FunctionData> Copy() const override;
	bool Equals(const FunctionData &other_p) const override;
};

unique_ptr<FunctionData> TeraRenderBindData::Copy() const {
	return make_uniq<TeraRenderBindData>(template_path, autoescape, autoescape_on, optional_args);
}

bool TeraRenderBindData::Equals(const FunctionData &other_p) const {
	auto &other = (const TeraRenderBindData &)other_p;
	return template_path == other.template_path && autoescape == other.autoescape &&
	       autoescape_on == other.autoescape_on && optional_args == other.optional_args;
}

unique_ptr<FunctionData> TeraRenderBind(ClientContext &context, ScalarFunction &bound_function,
                                        vector<unique_ptr<Expression>> &arguments) {
	if (arguments.empty()) {
		throw BinderException("tera_render takes at least one argument");
	}

	// Optional arguments
	string template_path;
	bool autoescape = true;
	vector<string> autoescape_on;
	int optional_args = 0;

	for (idx_t i = bound_function.arguments.size(); i < arguments.size(); i++) {
		const auto &arg = arguments[i];
		const auto &alias = arg->GetAlias();
		if (alias == "") {
			continue;
		}
		if (arg->HasParameter()) {
			throw ParameterNotResolvedException();
		}
		if (!arg->IsFoldable()) {
			throw BinderException("tera_render: arguments must be constant");
		}
		if (alias == "autoescape") {
			optional_args++;
			if (arg->return_type.id() != LogicalTypeId::BOOLEAN) {
				throw BinderException("tera_render: 'autoescape' argument must be a BOOLEAN");
			}
			autoescape = BooleanValue::Get(ExpressionExecutor::EvaluateScalar(context, *arg));
		} else if (alias == "template_path") {
			optional_args++;

			if (arg->return_type.id() != LogicalTypeId::VARCHAR) {
				throw BinderException("tera_render: 'template_path' argument must be a VARCHAR");
			}
			template_path = StringValue::Get(ExpressionExecutor::EvaluateScalar(context, *arg));
		} else if (alias == "autoescape_extensions") {
			optional_args++;

			if (arg->return_type.InternalType() != PhysicalType::LIST) {
				throw BinderException(StringUtil::Format(
				    "tera_render: 'autoescape_extensions' argument must be a list of strings it is %s",
				    arg->return_type.ToString()));
			}

			const auto list_children = ListValue::GetChildren(ExpressionExecutor::EvaluateScalar(context, *arg));
			for (const auto &list_item : list_children) {
				if (list_item.type() != LogicalType::VARCHAR) {
					throw BinderException(StringUtil::Format(
					    "tera_render: 'autoescape_extensions' elements must be strings, got %s with value %s",
					    list_item.type().ToString(), list_item.ToString()));
				}

				autoescape_on.push_back(list_item.GetValue<string>());
			}
		} else {
			throw BinderException(StringUtil::Format("tera_render: Unknown argument '%s'", alias));
		}
	}

	return make_uniq<TeraRenderBindData>(template_path, autoescape, autoescape_on, optional_args);
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
	const auto &bind_data = func_expr.bind_info->Cast<TeraRenderBindData>();
	auto &lstate = ExecuteFunctionState::GetFunctionState(state)->Cast<TeraRenderLocalState>();

	auto &expression_vector = args.data[0];

	if (args.ColumnCount() - bind_data.optional_args == 2) {
		// There can be a context column.
		auto &context_json_vector = args.data[1];
		BinaryExecutor::Execute<string_t, string_t, string_t>(
		    expression_vector, context_json_vector, result, args.size(),
		    [&](string_t expression, string_t context_json) {
			    return RenderToVector(result, lstate, bind_data, expression, context_json.GetData(),
			                          context_json.GetSize());
		    });
	} else if (args.ColumnCount() - bind_data.optional_args == 1) {
		// No context column.
		UnaryExecutor::Execute<string_t, string_t>(expression_vector, result, args.size(), [&](string_t expression) {
			return RenderToVector(result, lstate, bind_data, expression, "{}", 2);
		});
	} else {
		throw InvalidInputException("Invalid number of arguments to tera_render");
	}
}

// Extension initalization.
static void LoadInternal(ExtensionLoader &loader) {
	{
		ScalarFunctionSet render("tera_render");

		auto render_with_context =
		    ScalarFunction({LogicalType::VARCHAR, LogicalType::JSON()}, LogicalType::VARCHAR, TeraRenderFunc,
		                   TeraRenderBind, nullptr, nullptr, nullptr, LogicalType(LogicalTypeId::ANY));
		render_with_context.null_handling = FunctionNullHandling::DEFAULT_NULL_HANDLING;
		render_with_context.stability = FunctionStability::VOLATILE;
		render_with_context.init_local_state = TeraRenderInitLocalState;
		render.AddFunction(render_with_context);

		auto render_no_context =
		    ScalarFunction({LogicalType::VARCHAR}, LogicalType::VARCHAR, TeraRenderFunc, TeraRenderBind, nullptr,
		                   nullptr, nullptr, LogicalType(LogicalTypeId::ANY));
		render_no_context.null_handling = FunctionNullHandling::DEFAULT_NULL_HANDLING;
		render_no_context.stability = FunctionStability::VOLATILE;
		render_no_context.init_local_state = TeraRenderInitLocalState;
		render.AddFunction(render_no_context);

		CreateScalarFunctionInfo info(render);

		// Documentation for tera_render(template, context) variant
		FunctionDescription desc_with_context;
		desc_with_context.parameter_names = {"template", "context"};
		desc_with_context.parameter_types = {LogicalType::VARCHAR, LogicalType::JSON()};
		desc_with_context.description = "Renders a Tera template string with JSON context variables";
		desc_with_context.examples = {"tera_render('Hello {{ name }}!', '{\"name\": \"World\"}')",
		                              "tera_render('{{ value | upper }}', '{\"value\": \"hello\"}')"};
		desc_with_context.categories = {"text", "template"};
		info.descriptions.push_back(desc_with_context);

		// Documentation for tera_render(template) variant
		FunctionDescription desc_no_context;
		desc_no_context.parameter_names = {"template"};
		desc_no_context.parameter_types = {LogicalType::VARCHAR};
		desc_no_context.description = "Renders a Tera template string without context variables";
		desc_no_context.examples = {"tera_render('Hello World!')"};
		desc_no_context.categories = {"text", "template"};
		info.descriptions.push_back(desc_no_context);

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
