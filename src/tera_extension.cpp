#define DUCKDB_EXTENSION_MAIN

#include "tera_extension.hpp"
#include "duckdb.hpp"
#include "duckdb/common/exception.hpp"
#include "duckdb/function/scalar_function.hpp"
#include <duckdb/parser/parsed_data/create_scalar_function_info.hpp>
#include "duckdb/planner/expression/bound_function_expression.hpp"

#include "rust.h"
#include "query_farm_telemetry.hpp"

namespace duckdb {

// Bar chart bind data structure
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

	for (idx_t i = 1; i < arguments.size(); i++) {
		const auto &arg = arguments[i];
		if (arg->HasParameter()) {
			throw ParameterNotResolvedException();
		}
		if (!arg->IsFoldable()) {
			throw BinderException("tera_render: arguments must be constant");
		}
		const auto &alias = arg->GetAlias();
		if (alias == "") {
			continue;
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
				throw BinderException(
				    StringUtil::Format("tera_render: 'autoescape_on' argument must be a list of strings it is %s",
				                       arg->return_type.ToString()));
			}

			const auto list_children = ListValue::GetChildren(ExpressionExecutor::EvaluateScalar(context, *arg));
			for (const auto &list_item : list_children) {
				// These should also be lists.
				if (list_item.type() != LogicalType::VARCHAR) {
					throw BinderException(
					    StringUtil::Format("tera_render: 'autoescape_on' child must be a string it is %s value is %s",
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

inline void TeraRenderFunc(DataChunk &args, ExpressionState &state, Vector &result) {
	const auto &func_expr = state.expr.Cast<BoundFunctionExpression>();
	const auto &bind_data = func_expr.bind_info->Cast<TeraRenderBindData>();

	auto &expression_vector = args.data[0];
	const auto count = args.size();

	std::vector<const char *> autoescape_on_ptrs;
	for (const auto &s : bind_data.autoescape_on) {
		autoescape_on_ptrs.push_back(s.c_str());
	}

	if (args.ColumnCount() - bind_data.optional_args == 2) {
		// There can be a context column.
		auto &context_json_vector = args.data[1];
		BinaryExecutor::Execute<string_t, string_t, string_t>(
		    expression_vector, context_json_vector, result, args.size(),
		    [&](string_t expression, string_t context_json) {
			    ResultCString eval_result =
			        render_template(expression.GetData(), expression.GetSize(), context_json.GetData(),
			                        context_json.GetSize(), bind_data.template_path.c_str(), bind_data.autoescape,
			                        autoescape_on_ptrs.data(), static_cast<int32_t>(autoescape_on_ptrs.size()));
			    if (eval_result.tag == ResultCString::Tag::Err) {
				    string err_str = string(eval_result.err._0);
				    free_result_cstring(eval_result);
				    throw InvalidInputException("Error rendering template: " + err_str);
			    } else {
				    auto vector_result = StringVector::AddString(result, eval_result.ok._0);
				    free_result_cstring(eval_result);
				    return vector_result;
			    }
		    });
	} else if (args.ColumnCount() - bind_data.optional_args == 1) {
		// No context column.
		UnaryExecutor::Execute<string_t, string_t>(expression_vector, result, args.size(), [&](string_t expression) {
			ResultCString eval_result = render_template(
			    expression.GetData(), expression.GetSize(), "{}", 2, bind_data.template_path.c_str(),
			    bind_data.autoescape, autoescape_on_ptrs.data(), static_cast<int32_t>(autoescape_on_ptrs.size()));
			if (eval_result.tag == ResultCString::Tag::Err) {
				string err_str = string(eval_result.err._0);
				free_result_cstring(eval_result);
				throw InvalidInputException("Error rendering template: " + err_str);
			} else {
				auto vector_result = StringVector::AddString(result, eval_result.ok._0);
				free_result_cstring(eval_result);
				return vector_result;
			}
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
		render_with_context.null_handling = FunctionNullHandling::SPECIAL_HANDLING;
		render_with_context.stability = FunctionStability::VOLATILE;
		render.AddFunction(render_with_context);

		auto render_no_context =
		    ScalarFunction({LogicalType::VARCHAR}, LogicalType::VARCHAR, TeraRenderFunc, TeraRenderBind, nullptr,
		                   nullptr, nullptr, LogicalType(LogicalTypeId::ANY));
		render_no_context.null_handling = FunctionNullHandling::SPECIAL_HANDLING;
		render_no_context.stability = FunctionStability::VOLATILE;
		render.AddFunction(render_no_context);

		loader.RegisterFunction(render);
	}

	QueryFarmSendTelemetry(loader, "tera", "2025101901");
}

void TeraExtension::Load(ExtensionLoader &loader) {
	LoadInternal(loader);
}
std::string TeraExtension::Name() {
	return "tera";
}

std::string TeraExtension::Version() const {
	return "2025101901";
}

} // namespace duckdb

extern "C" {

DUCKDB_CPP_EXTENSION_ENTRY(tera, loader) {
	duckdb::LoadInternal(loader);
}
}
