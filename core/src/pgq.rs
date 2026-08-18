//! Postgres query helpers.
//!
//! The whole codebase was written against SQLite's `?` positional placeholders.
//! Postgres uses `$1, $2, …`. Rather than renumber every one of the ~180 queries
//! by hand (error-prone), these thin wrappers convert `?` → `$N` at call time and
//! delegate to `sqlx`. Conversion is done once per distinct SQL string and the
//! result is interned (leaked) so the returned query borrows a `'static` str —
//! the set of query shapes is fixed, so the leak is bounded.
//!
//! Call sites use `pgq::query` / `pgq::query_as::<T>` / `pgq::query_scalar::<T>`
//! exactly like the `sqlx::` equivalents (minus the `_,` DB type param).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use sqlx::postgres::{PgArguments, PgRow};
use sqlx::query::{Query, QueryAs, QueryScalar};
use sqlx::{FromRow, Postgres};

fn cache() -> &'static Mutex<HashMap<String, &'static str>> {
    static C: OnceLock<Mutex<HashMap<String, &'static str>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Rewrite SQLite `?` placeholders to Postgres `$1, $2, …`. Quote-aware: a `?`
/// inside a single-quoted SQL string literal is left alone. Interned per distinct
/// input so the result is `'static`.
pub fn convert(sql: &str) -> &'static str {
    if let Some(s) = cache().lock().unwrap().get(sql) {
        return s;
    }
    let mut out = String::with_capacity(sql.len() + 8);
    let mut n = 0u32;
    let mut in_str = false;
    for c in sql.chars() {
        match c {
            // Doubled '' inside a literal toggles twice → net no-op, and no
            // placeholder can sit between the two quotes, so this stays correct.
            '\'' => {
                in_str = !in_str;
                out.push(c);
            }
            '?' if !in_str => {
                n += 1;
                out.push('$');
                out.push_str(&n.to_string());
            }
            _ => out.push(c),
        }
    }
    let leaked: &'static str = Box::leak(out.into_boxed_str());
    cache().lock().unwrap().insert(sql.to_string(), leaked);
    leaked
}

// The SQL is interned to `'static`, but the returned query is generic over `'q`
// so `.bind(&local)` can supply borrowed arguments with their own (shorter)
// lifetime — the `'static` SQL str coerces into any `'q`.
pub fn query<'q>(sql: &str) -> Query<'q, Postgres, PgArguments> {
    sqlx::query::<Postgres>(convert(sql))
}

pub fn query_as<'q, O>(sql: &str) -> QueryAs<'q, Postgres, O, PgArguments>
where
    O: for<'r> FromRow<'r, PgRow>,
{
    sqlx::query_as::<Postgres, O>(convert(sql))
}

pub fn query_scalar<'q, O>(sql: &str) -> QueryScalar<'q, Postgres, O, PgArguments>
where
    (O,): for<'r> FromRow<'r, PgRow>,
{
    sqlx::query_scalar::<Postgres, O>(convert(sql))
}

#[cfg(test)]
mod tests {
    use super::convert;

    #[test]
    fn converts_positional_placeholders() {
        assert_eq!(
            convert("INSERT INTO t (a, b, c) VALUES (?, ?, ?)"),
            "INSERT INTO t (a, b, c) VALUES ($1, $2, $3)"
        );
    }

    #[test]
    fn leaves_question_marks_inside_string_literals() {
        assert_eq!(
            convert("SELECT * FROM t WHERE note = 'why?' AND id = ?"),
            "SELECT * FROM t WHERE note = 'why?' AND id = $1"
        );
    }

    #[test]
    fn no_placeholders_is_unchanged() {
        assert_eq!(convert("SELECT 1"), "SELECT 1");
    }
}
