# This file is included by DuckDB's build system. It specifies which extension to load

# Extension from this repo
duckdb_extension_load(tera
    SOURCE_DIR ${CMAKE_CURRENT_LIST_DIR}
    LINKED_LIBS "../../cargo/build/wasm32-unknown-emscripten/release/libduckdb_tera_binding.a"
    LOAD_TESTS
)

# Any extra extensions that should be built
# e.g.: duckdb_extension_load(json)