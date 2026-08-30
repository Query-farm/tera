<p align="center">
  <a href="https://query.farm">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="https://query.farm/media-kit/logo/wordmark-dark.svg">
      <img alt="Query.Farm" src="https://query.farm/media-kit/logo/wordmark-light.svg" height="64">
    </picture>
  </a>
</p>

# DuckDB [Tera](https://keats.github.io/tera/) Extension by [Query.Farm](https://query.farm)

[![DuckDB](https://img.shields.io/badge/DuckDB-community_extension-fdf1e0?logo=duckdb&logoColor=fff000)](https://duckdb.org/community_extensions/extensions/tera.html)
[![v1.5 build](https://github.com/Query-farm/tera/actions/workflows/MainDistributionPipeline.yml/badge.svg?branch=v1.5)](https://github.com/Query-farm/tera/actions/workflows/MainDistributionPipeline.yml?query=branch%3Av1.5)

The **Tera** extension, developed by **[Query.Farm](https://query.farm)**, brings powerful template rendering capabilities directly to your SQL queries in DuckDB. Generate dynamic text, HTML, configuration files, and reports using the robust [Tera](https://keats.github.io/tera/) templating engine without leaving your database environment.

## Documentation

Full documentation, including installation, usage, the function reference, and cookbook examples, is available at:

**[https://query.farm/products/extensions/tera](https://query.farm/products/extensions/tera)**

## Installation

```sql
INSTALL tera FROM community;
LOAD tera;
```
