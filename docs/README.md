# DuckDB Tera Extension by [Query.Farm](https://query.farm)

The **Tera** extension, developed by **[Query.Farm](https://query.farm)**, brings powerful template rendering capabilities directly to your SQL queries in DuckDB. Generate dynamic text, HTML, configuration files, and reports using the robust Tera templating engine without leaving your database environment.

## Use Cases

The Tera extension is perfect for:
- **Dynamic report generation**: Create custom formatted reports with data from your database
- **Configuration file generation**: Generate config files, scripts, and infrastructure-as-code templates
- **HTML/XML generation**: Build web pages, emails, and XML documents with database content
- **Data transformation**: Convert structured data into various text formats
- **Notification templates**: Create personalized messages, alerts, and communications
- **ETL pipeline outputs**: Transform data into specific formats required by downstream systems
- **Dynamic SQL generation**: Create parameterized queries and DDL statements
- **Internationalization**: Generate localized content using template variables

## Installation

**`tera` is a [DuckDB Community Extension](https://github.com/duckdb/community-extensions).**

You can now use this by using this SQL:

```sql
INSTALL tera FROM community;
LOAD tera;
```

## Functions

### `tera_render(template, context, ...options)`
Renders Tera templates with JSON context data and customizable options.

**Basic Usage:**
```sql
-- Simple variable substitution
SELECT tera_render('{{ foo }}', '{"foo": "bar"}');
┌────────────────────────────────────────────┐
│ tera_render('{{ foo }}', '{"foo": "bar"}') │
│                  varchar                   │
├────────────────────────────────────────────┤
│ bar                                        │
└────────────────────────────────────────────┘

-- Template without context (will error if variables are referenced)
SELECT tera_render('Hello, World!');
┌──────────────────────────────┐
│ tera_render('Hello, World!') │
│           varchar            │
├──────────────────────────────┤
│ Hello, World!                │
└──────────────────────────────┘
```

**Advanced Context Usage:**
```sql
-- Complex JSON context with nested objects
SELECT tera_render(
    'Hello {{ user.name }}, you have {{ user.messages }} new messages!',
    '{"user": {"name": "Alice", "messages": 5}}'
) as output;
┌───────────────────────────────────────┐
│                output                 │
│                varchar                │
├───────────────────────────────────────┤
│ Hello Alice, you have 5 new messages! │
└───────────────────────────────────────┘

-- Using arrays and loops
SELECT tera_render(
    'Items: {% for item in items %}{{ item.name }}{% if not loop.last %}, {% endif %}{% endfor %}',
    '{"items": [{"name": "Apple"}, {"name": "Banana"}, {"name": "Cherry"}]}'
) as output;
┌──────────────────────────────┐
│            output            │
│           varchar            │
├──────────────────────────────┤
│ Items: Apple, Banana, Cherry │
└──────────────────────────────┘
```

**HTML Generation with Autoescaping:**
```sql
-- Default autoescaping (enabled)
SELECT tera_render('{{ v }}', '{"v": "B&O"}') as output;
┌─────────┐
│ output  │
│ varchar │
├─────────┤
│ B&amp;O │
└─────────┘

-- Disable autoescaping for raw output
SELECT tera_render('{{ v }}', '{"v": "B&O"}', autoescape := false) as output;
┌─────────┐
│ output  │
│ varchar │
├─────────┤
│ B&O     │
└─────────┘
```

**Template File Rendering:**
```sql
-- Render from template files with custom path
SELECT tera_render(
    'index.html',
    '{"v": "B&O"}',
    autoescape := false,
    template_path := './templates/*.html'
) as output;
┌─────────┐
│ output  │
│ varchar │
├─────────┤
│ B&O     │
└─────────┘
```

**Error Handling:**
```sql
-- Template errors are reported clearly
SELECT tera_render('{{ missing_var }}');
--- Error rendering template: Tera render error: Failed to render '__tera_one_off'
--Caused by: Variable `missing_var` not found in context while rendering '__tera_one_off'

-- File not found errors
SELECT tera_render('nonexistent.html', '{}', template_path := './templates/*.html');
-- Error: Template 'nonexistent.html' not found
```

**Parameters:**
- `template`: Template string or filename (when using `template_path`)
- `context`: Any object that can be coerced to JSON, most often should be a JSON map.
- `autoescape`: Boolean, enable/disable HTML autoescaping (default: `true`)
- `autoescape_on`: `VARCHAR[]`, A list of file extensions where autoescaping should be applied.
- `template_path`: File glob pattern for template files (enables file mode)

**Template Syntax:**
The Tera extension uses the full Tera templating language, which includes:

- **Variables**: `{{ variable_name }}`
- **Filters**: `{{ name | upper }}`, `{{ price | round(precision=2) }}`
- **Control structures**:
  ```
  {% if condition %}...{% endif %}
  {% for item in list %}...{% endfor %}
  {% set var = value %}
  ```
- **Comments**: `{# This is a comment #}`
- **Template inheritance**: `{% extends "base.html" %}`, `{% block content %}...{% endblock %}`
- **Macros**: `{% macro button(text) %}...{% endmacro %}`

## Real-World Examples

### Generate HTML Reports
```sql
-- Create a sales report
SELECT tera_render('
<h1>Sales Report - {{ report_date }}</h1>
<table>
  <tr><th>Product</th><th>Sales</th><th>Revenue</th></tr>
  {% for item in sales %}
  <tr>
    <td>{{ item.product }}</td>
    <td>{{ item.quantity }}</td>
    <td>${{ item.revenue | round(precision=2) }}</td>
  </tr>
  {% endfor %}
</table>
<p>Total Revenue: ${{ total_revenue | round(precision=2) }}</p>
', '{"report_date":"2024-10-19","sales":[{"product":"Widget A","quantity":150,"revenue":1500.0},{"product":"Widget B","quantity":200,"revenue":3000.0}],"total_revenue":4500.0}');
```

### Configuration File Generation
```sql
-- Generate application config
SELECT tera_render('
[database]
host = "{{ db.host }}"
port = {{ db.port }}
name = "{{ db.name }}"

[features]
{% for feature in features %}
{{ feature.name }} = {{ feature.enabled | lower }}
{% endfor %}

[logging]
level = "{{ log_level | upper }}"
', json_object(
    'db', json_object('host', 'localhost', 'port', 5432, 'name', 'myapp'),
    'features', json_array(
        json_object('name', 'analytics', 'enabled', true),
        json_object('name', 'debug_mode', 'enabled', false)
    ),
    'log_level', 'info'
));
```

### Dynamic Email Templates
```sql
-- Personalized email generation
SELECT
    user_email,
    tera_render('
Subject: Welcome {{ user.first_name }}!

Dear {{ user.first_name }} {{ user.last_name }},

Welcome to our platform! Here are your account details:
- Username: {{ user.username }}
- Account Type: {{ user.account_type | title }}
- Sign-up Date: {{ signup_date }}

{% if user.account_type == "premium" %}
As a premium member, you have access to:
{% for feature in premium_features %}
- {{ feature }}
{% endfor %}
{% endif %}

Best regards,
The Team
    ', json_object(
        'user', json_object(
            'first_name', first_name,
            'last_name', last_name,
            'username', username,
            'account_type', account_type
        ),
        'signup_date', signup_date::text,
        'premium_features', CASE
            WHEN account_type = 'premium'
            THEN json_array('Advanced Analytics', 'Priority Support', 'Custom Integrations')
            ELSE json_array()
        END
    )) as email_content
FROM users
WHERE created_date >= current_date - interval '1 day';
```

### JSON/API Response Generation
```sql
-- Generate API responses
SELECT tera_render('
{
  "status": "{{ status }}",
  "timestamp": "{{ timestamp }}",
  "data": {
    "user_count": {{ metrics.users }},
    "active_sessions": {{ metrics.sessions }},
    "success_rate": {{ metrics.success_rate }}
  },
  "alerts": [
    {% for alert in alerts %}
    {
      "level": "{{ alert.level }}",
      "message": "{{ alert.message }}",
      "count": {{ alert.count }}
    }{% if not loop.last %},{% endif %}
    {% endfor %}
  ]
}', json_object(
    'status', 'ok',
    'timestamp', now()::text,
    'metrics', json_object(
        'users', 1250,
        'sessions', 89,
        'success_rate', 0.997
    ),
    'alerts', json_array(
        json_object('level', 'warning', 'message', 'High CPU usage', 'count', 3),
        json_object('level', 'info', 'message', 'Scheduled maintenance', 'count', 1)
    )
));
```

## Available Filters

Tera includes many built-in filters for data transformation:

- **String filters**: `upper`, `lower`, `title`, `trim`, `replace`, `split`
- **Number filters**: `round`, `abs`, `ceil`, `floor`
- **Array filters**: `first`, `last`, `length`, `join`, `reverse`, `sort`
- **Date filters**: `date`, `strftime`
- **Formatting**: `format`, `pluralize`, `truncate`
- **Encoding**: `urlencode`, `escape`, `safe`

Example usage:
```sql
SELECT tera_render('
Name: {{ name | title }}
Email: {{ email | lower }}
Price: ${{ price | round(precision=2) }}
Tags: {{ tags | join(sep=", ") }}
', '{"name": "john doe", "email": "JOHN@EXAMPLE.COM", "price": 19.999, "tags": ["new", "featured", "sale"]}');
```

## Tips and Best Practices

1. **Use JSON context effectively**: Structure your context data to match your template needs
2. **Enable autoescaping for HTML**: Keep the default `autoescape := true` when generating HTML to prevent XSS
3. **Organize templates in files**: Use `template_path` for complex templates and template inheritance
4. **Handle errors gracefully**: Tera provides clear error messages for debugging template issues
5. **Leverage filters**: Use built-in filters to transform data within templates instead of preprocessing
6. **Test with small datasets**: Develop templates with simple test data before applying to large result sets
7. **Consider performance**: Complex templates with large datasets may impact query performance

## Error Messages

The Tera extension provides detailed error messages to help debug template issues:

- **Variable not found**: Clear indication of missing variables in context
- **Template syntax errors**: Line numbers and descriptions of syntax issues
- **File not found**: Specific error when template files can't be located
- **Filter errors**: Information about incorrect filter usage or arguments

## Contributing

The Tera extension is open source and developed by [Query.Farm](https://query.farm). Contributions are welcome!

## License

[MIT License](LICENSE)