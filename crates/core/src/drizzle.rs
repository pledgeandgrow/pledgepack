// Drizzle ORM plugin — schema analysis and migration helpers.
//
// Features:
//   - Parse Drizzle schema files (TypeScript table definitions)
//   - Extract table names, columns, types, and constraints
//   - Generate SQL migration files from schema changes
//   - Schema diffing for incremental migrations

use anyhow::Result;
use std::collections::HashMap;

/// Parsed Drizzle schema
#[derive(Debug, Clone, Default)]
pub struct DrizzleSchema {
    pub tables: Vec<DrizzleTable>,
}

#[derive(Debug, Clone, Default)]
pub struct DrizzleTable {
    pub name: String,
    pub columns: Vec<DrizzleColumn>,
}

#[derive(Debug, Clone, Default)]
pub struct DrizzleColumn {
    pub name: String,
    pub type_name: String,
    pub is_primary_key: bool,
    pub is_nullable: bool,
    pub is_unique: bool,
    pub has_default: bool,
    pub default_value: Option<String>,
    pub references: Option<String>,
}

/// Parse a Drizzle schema file (TypeSQL-style table definitions)
/// Recognizes patterns like:
///   export const users = pgTable('users', { id: serial('id').primaryKey(), ... })
///   export const posts = sqliteTable('posts', { ... })
///   export const myTable = mysqlTable('my_table', { ... })
pub fn parse_schema(source: &str) -> Result<DrizzleSchema> {
    let mut schema = DrizzleSchema::default();

    // Find table definitions: pgTable('name', { ... }) / sqliteTable / mysqlTable
    let table_patterns = ["pgTable", "sqliteTable", "mysqlTable"];

    for pattern in &table_patterns {
        let search = format!("{}(", pattern);
        let mut pos = 0;

        while let Some(start) = source[pos..].find(&search) {
            let abs_start = pos + start + search.len();
            // Extract table name (first quoted string argument)
            if let Some(name) = extract_quoted_string(&source[abs_start..]) {
                let mut table = DrizzleTable {
                    name: name.clone(),
                    columns: Vec::new(),
                };

                // Find the column definition object: { ... }
                if let Some(brace_start) = source[abs_start..].find('{') {
                    let brace_abs = abs_start + brace_start;
                    if let Some(brace_end) = find_matching_brace(&source[brace_abs..]) {
                        let columns_src = &source[brace_abs + 1..brace_abs + brace_end];

                        // Parse column definitions: name: type('name').modifier()
                        for line in columns_src.lines() {
                            let line = line.trim().trim_end_matches(',');
                            if line.is_empty() || line.starts_with("//") {
                                continue;
                            }

                            if let Some(col) = parse_column_line(line) {
                                table.columns.push(col);
                            }
                        }
                    }
                }

                schema.tables.push(table);
            }

            pos = abs_start;
        }
    }

    Ok(schema)
}

fn extract_quoted_string(s: &str) -> Option<String> {
    let s = s.trim_start();
    let quote = s.chars().next().filter(|c| *c == '\'' || *c == '"')?;
    let end = s[1..].find(quote)?;
    Some(s[1..1 + end].to_string())
}

fn find_matching_brace(s: &str) -> Option<usize> {
    let mut depth = 0;
    for (i, c) in s.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn parse_column_line(line: &str) -> Option<DrizzleColumn> {
    // Pattern: name: type('name').modifier().modifier()
    let colon_pos = line.find(':')?;
    let col_name = line[..colon_pos].trim().to_string();

    let rest = line[colon_pos + 1..].trim();

    // Extract the type function name (serial, integer, text, varchar, etc.)
    let type_name = rest
        .find('(')
        .map(|p| rest[..p].trim().to_string())
        .unwrap_or_else(|| {
            rest.split('.')
                .next()
                .unwrap_or("unknown")
                .trim()
                .to_string()
        });

    let mut col = DrizzleColumn {
        name: col_name,
        type_name,
        is_nullable: true, // Drizzle defaults to nullable unless .notNull()
        ..Default::default()
    };

    // Check for modifiers
    if rest.contains(".primaryKey()") {
        col.is_primary_key = true;
        col.is_nullable = false;
    }
    if rest.contains(".notNull()") {
        col.is_nullable = false;
    }
    if rest.contains(".unique()") {
        col.is_unique = true;
    }
    if rest.contains(".default(") {
        col.has_default = true;
        if let Some(start) = rest.find(".default(") {
            let after = &rest[start + 9..];
            if let Some(end) = after.find(')') {
                col.default_value = Some(after[..end].trim().to_string());
            }
        }
    }
    if rest.contains(".references(")
        && let Some(start) = rest.find(".references(")
    {
        let after = &rest[start + 12..];
        if let Some(end) = after.find(')') {
            col.references = Some(after[..end].trim().to_string());
        }
    }

    Some(col)
}

/// Generate a SQL CREATE TABLE migration from a Drizzle schema
pub fn generate_migration(schema: &DrizzleSchema) -> String {
    let mut sql = String::new();

    for table in &schema.tables {
        sql.push_str(&format!(
            "CREATE TABLE IF NOT EXISTS \"{}\" (\n",
            table.name
        ));

        let mut columns_sql = Vec::new();
        let mut primary_keys = Vec::new();

        for col in &table.columns {
            let sql_type = drizzle_type_to_sql(&col.type_name);
            let mut col_sql = format!("  \"{}\" {}", col.name, sql_type);

            if col.is_primary_key {
                primary_keys.push(col.name.clone());
                if col.type_name == "serial" || col.type_name == "bigserial" {
                    col_sql.push_str(" AUTOINCREMENT");
                }
            }
            if !col.is_nullable {
                col_sql.push_str(" NOT NULL");
            }
            if col.is_unique {
                col_sql.push_str(" UNIQUE");
            }
            if col.has_default
                && let Some(ref default) = col.default_value
            {
                col_sql.push_str(&format!(" DEFAULT {}", default));
            }

            columns_sql.push(col_sql);
        }

        if !primary_keys.is_empty() {
            columns_sql.push(format!(
                "  PRIMARY KEY ({})",
                primary_keys
                    .iter()
                    .map(|k| format!("\"{}\"", k))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }

        sql.push_str(&columns_sql.join(",\n"));
        sql.push_str("\n);\n\n");
    }

    sql
}

fn drizzle_type_to_sql(type_name: &str) -> &'static str {
    match type_name {
        "serial" => "INTEGER",
        "bigserial" => "BIGINT",
        "integer" | "int" | "int4" => "INTEGER",
        "bigint" | "int8" => "BIGINT",
        "smallint" | "int2" => "SMALLINT",
        "text" => "TEXT",
        "varchar" => "VARCHAR(255)",
        "char" => "CHAR(1)",
        "boolean" | "bool" => "BOOLEAN",
        "real" | "float4" => "REAL",
        "doublePrecision" | "float8" => "DOUBLE PRECISION",
        "numeric" | "decimal" => "NUMERIC",
        "timestamp" => "TIMESTAMP",
        "date" => "DATE",
        "time" => "TIME",
        "json" | "jsonb" => "JSON",
        "uuid" => "UUID",
        "blob" | "bytea" => "BLOB",
        _ => "TEXT",
    }
}

/// Diff two schemas and generate ALTER TABLE statements
pub fn diff_schemas(old: &DrizzleSchema, new: &DrizzleSchema) -> String {
    let mut sql = String::new();
    let old_tables: HashMap<&str, &DrizzleTable> =
        old.tables.iter().map(|t| (t.name.as_str(), t)).collect();
    let new_tables: HashMap<&str, &DrizzleTable> =
        new.tables.iter().map(|t| (t.name.as_str(), t)).collect();

    // New tables
    for table in &new.tables {
        if !old_tables.contains_key(table.name.as_str()) {
            sql.push_str(&generate_migration(&DrizzleSchema {
                tables: vec![table.clone()],
            }));
        }
    }

    // Dropped tables
    for table in &old.tables {
        if !new_tables.contains_key(table.name.as_str()) {
            sql.push_str(&format!("DROP TABLE IF EXISTS \"{}\";\n", table.name));
        }
    }

    // Modified tables
    for new_table in &new.tables {
        if let Some(old_table) = old_tables.get(new_table.name.as_str()) {
            let old_cols: HashMap<&str, &DrizzleColumn> = old_table
                .columns
                .iter()
                .map(|c| (c.name.as_str(), c))
                .collect();

            // Added columns
            for col in &new_table.columns {
                if !old_cols.contains_key(col.name.as_str()) {
                    let sql_type = drizzle_type_to_sql(&col.type_name);
                    let mut col_def = format!(
                        "ALTER TABLE \"{}\" ADD COLUMN \"{}\" {}",
                        new_table.name, col.name, sql_type
                    );
                    if !col.is_nullable {
                        col_def.push_str(" NOT NULL");
                    }
                    if col.is_unique {
                        col_def.push_str(" UNIQUE");
                    }
                    col_def.push(';');
                    sql.push_str(&col_def);
                    sql.push('\n');
                }
            }

            // Dropped columns
            for old_col in &old_table.columns {
                if !new_table.columns.iter().any(|c| c.name == old_col.name) {
                    sql.push_str(&format!(
                        "ALTER TABLE \"{}\" DROP COLUMN \"{}\";\n",
                        new_table.name, old_col.name
                    ));
                }
            }
        }
    }

    sql
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_drizzle_schema() {
        let source = r#"
import { pgTable, serial, varchar, boolean, timestamp } from 'drizzle-orm/pg-core';

export const users = pgTable('users', {
  id: serial('id').primaryKey(),
  email: varchar('email', { length: 255 }).notNull().unique(),
  active: boolean('active').default(true),
  createdAt: timestamp('created_at').defaultNow(),
});
"#;
        let result = parse_schema(source);
        assert!(result.is_ok());
        let schema = result.unwrap();
        assert_eq!(schema.tables.len(), 1);
        assert_eq!(schema.tables[0].name, "users");
        assert_eq!(schema.tables[0].columns.len(), 4);

        let id_col = &schema.tables[0].columns[0];
        assert_eq!(id_col.name, "id");
        assert!(id_col.is_primary_key);
        assert!(!id_col.is_nullable);

        let email_col = &schema.tables[0].columns[1];
        assert!(email_col.is_unique);
        assert!(!email_col.is_nullable);
    }

    #[test]
    fn test_generate_migration() {
        let schema = DrizzleSchema {
            tables: vec![DrizzleTable {
                name: "users".to_string(),
                columns: vec![
                    DrizzleColumn {
                        name: "id".to_string(),
                        type_name: "serial".to_string(),
                        is_primary_key: true,
                        is_nullable: false,
                        ..Default::default()
                    },
                    DrizzleColumn {
                        name: "email".to_string(),
                        type_name: "varchar".to_string(),
                        is_nullable: false,
                        is_unique: true,
                        ..Default::default()
                    },
                ],
            }],
        };

        let sql = generate_migration(&schema);
        assert!(sql.contains("CREATE TABLE"));
        assert!(sql.contains("PRIMARY KEY"));
        assert!(sql.contains("UNIQUE"));
        assert!(sql.contains("NOT NULL"));
    }

    #[test]
    fn test_diff_schemas() {
        let old = DrizzleSchema {
            tables: vec![DrizzleTable {
                name: "users".to_string(),
                columns: vec![
                    DrizzleColumn {
                        name: "id".to_string(),
                        type_name: "serial".to_string(),
                        is_primary_key: true,
                        is_nullable: false,
                        ..Default::default()
                    },
                    DrizzleColumn {
                        name: "email".to_string(),
                        type_name: "varchar".to_string(),
                        is_nullable: false,
                        ..Default::default()
                    },
                ],
            }],
        };

        let new = DrizzleSchema {
            tables: vec![DrizzleTable {
                name: "users".to_string(),
                columns: vec![
                    DrizzleColumn {
                        name: "id".to_string(),
                        type_name: "serial".to_string(),
                        is_primary_key: true,
                        is_nullable: false,
                        ..Default::default()
                    },
                    DrizzleColumn {
                        name: "email".to_string(),
                        type_name: "varchar".to_string(),
                        is_nullable: false,
                        ..Default::default()
                    },
                    DrizzleColumn {
                        name: "name".to_string(),
                        type_name: "text".to_string(),
                        is_nullable: true,
                        ..Default::default()
                    },
                ],
            }],
        };

        let diff = diff_schemas(&old, &new);
        assert!(diff.contains("ADD COLUMN"));
        assert!(diff.contains("name"));
    }
}
