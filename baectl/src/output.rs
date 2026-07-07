//! Human-readable vs `--json` rendering.
//!
//! Conventions (from `aspec/uxui/cli.md`): **stdout carries command results
//! only** (scriptable); diagnostics and one-off warnings go to stderr. `--json`
//! prints the exact JSON document the admin API returned (an array for an
//! auto-paginated list); the default is a compact human-readable summary/table.

use serde_json::Value;

/// Print a JSON value to stdout, pretty-printed with a trailing newline.
pub fn print_json(value: &Value) {
    // Pretty output is friendlier for a human eyeballing `--json`; it is still
    // valid JSON for a downstream `jq`.
    match serde_json::to_string_pretty(value) {
        Ok(s) => println!("{s}"),
        // Serializing a `Value` we just decoded cannot fail; fall back defensively.
        Err(_) => println!("{value}"),
    }
}

/// Read a string field from a JSON object, or `"-"` if absent/null.
fn field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("-")
}

/// Render a just-created profile (the `{id, name, created_at}` summary).
pub fn print_profile_created(v: &Value) {
    println!("created profile");
    println!("  id:         {}", field(v, "id"));
    println!("  name:       {}", field(v, "name"));
    println!("  created_at: {}", field(v, "created_at"));
}

/// Render a full profile object (get/update response).
pub fn print_profile(v: &Value) {
    println!("id:         {}", field(v, "id"));
    println!("name:       {}", field(v, "name"));
    if let Some(pc) = v.get("provider_config") {
        println!("provider:   {}", field(pc, "provider"));
        println!("model:      {}", field(pc, "model"));
        println!("base_url:   {}", field(pc, "base_url"));
        println!("auth_token: {}", field(pc, "auth_token"));
        if let Some(mt) = pc.get("max_tokens") {
            println!("max_tokens: {mt}");
        }
    }
    print_str_list("fallbacks", providers_summary(v.get("fallback_configs")));
    print_str_list("mcp_servers", string_array(v.get("mcp_servers")));
    print_str_list("allowed_tools", string_array(v.get("allowed_tools")));
    println!("created_at: {}", field(v, "created_at"));
    println!("updated_at: {}", field(v, "updated_at"));
}

fn print_str_list(label: &str, items: Vec<String>) {
    if items.is_empty() {
        println!("{label}: (none)");
    } else {
        println!("{label}: {}", items.join(", "));
    }
}

fn string_array(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Summarize a `fallback_configs` array as `provider:model` entries.
fn providers_summary(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|c| format!("{}:{}", field(c, "provider"), field(c, "model")))
                .collect()
        })
        .unwrap_or_default()
}

/// Render a list of profiles as a table, or an empty-state line.
pub fn print_profiles_table(items: &[Value]) {
    if items.is_empty() {
        println!("no profiles found");
        return;
    }
    let rows: Vec<[String; 4]> = items
        .iter()
        .map(|p| {
            let pc = p.get("provider_config");
            let provider = pc.map(|c| field(c, "provider")).unwrap_or("-").to_string();
            let model = pc.map(|c| field(c, "model")).unwrap_or("-").to_string();
            [
                field(p, "id").to_string(),
                field(p, "name").to_string(),
                provider,
                model,
            ]
        })
        .collect();
    print_table(&["ID", "NAME", "PROVIDER", "MODEL"], &rows);
}

/// Render a list of keys as a table, or an empty-state line.
pub fn print_keys_table(items: &[Value]) {
    if items.is_empty() {
        println!("no keys found");
        return;
    }
    let rows: Vec<[String; 4]> = items
        .iter()
        .map(|k| {
            [
                field(k, "id").to_string(),
                field(k, "name").to_string(),
                field(k, "prefix").to_string(),
                field(k, "profile_id").to_string(),
            ]
        })
        .collect();
    print_table(&["ID", "NAME", "PREFIX", "PROFILE_ID"], &rows);
}

/// Render a just-created key. The plaintext `key` is shown exactly once here;
/// the caller emits the "copy this now" warning to stderr.
pub fn print_key_created(v: &Value) {
    println!("created key");
    println!("  id:         {}", field(v, "id"));
    println!("  name:       {}", field(v, "name"));
    println!("  key:        {}", field(v, "key"));
    println!("  prefix:     {}", field(v, "prefix"));
    println!("  profile_id: {}", field(v, "profile_id"));
    println!("  created_at: {}", field(v, "created_at"));
}

/// Minimal fixed-column table printer with a header row.
fn print_table<const N: usize>(headers: &[&str; N], rows: &[[String; N]]) {
    let mut widths = [0usize; N];
    for (i, h) in headers.iter().enumerate() {
        widths[i] = h.len();
    }
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    // Header.
    let header_line: Vec<String> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| format!("{:<width$}", h, width = widths[i]))
        .collect();
    println!("{}", header_line.join("  ").trim_end());
    // Rows.
    for row in rows {
        let line: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{:<width$}", c, width = widths[i]))
            .collect();
        println!("{}", line.join("  ").trim_end());
    }
}
