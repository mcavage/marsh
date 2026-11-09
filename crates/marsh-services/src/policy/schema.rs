//! Closed, table-driven body schemas (B2).
//!
//! A schema accepts a value only if every key is a known field (matched by
//! exact name, or by ASCII case-folding, as Go's decoder does), no two keys map
//! to one field, and every value has the declared shape. The result is a
//! canonical tree with Docker's key spellings; only that is ever re-serialized.

use super::json::Json;
use crate::breaks::break_mode;

pub type Check = fn(&str) -> bool;

#[derive(Debug)]
pub enum Ty {
    Bool,
    Str,
    /// A string accepted by the function; the text names the expectation.
    StrIf(Check, &'static str),
    /// Integer in `[min, max]`.
    Int(i64, i64),
    StrEnum(&'static [&'static str]),
    /// A string or an array of strings (Docker's `StrSlice`).
    StrOrArr,
    Arr(&'static Ty),
    /// Arbitrary string keys (Labels, `PortBindings`, ...); duplicate keys are
    /// already rejected by the parser.
    StrMap(&'static Ty),
    Obj(&'static [Field]),
    /// `{}` only.
    EmptyObj,
    /// Docker's CLI sends zero values for fields marsh refuses: null, false,
    /// 0, "", [] or {} (recursively).
    Zero,
    /// The boolean `false` only (not `null`, `0`, `""`).
    False,
    /// `null` only. An empty array is a different statement to Docker: for
    /// `MaskedPaths` and `ReadonlyPaths` it means "unconfined".
    Null,
    /// Any JSON; the route's semantic pass must consume and re-validate it.
    Raw,
}

#[derive(Debug)]
pub struct Field {
    pub name: &'static str,
    pub ty: Ty,
    /// JSON `null` is accepted (Go slices, maps and pointers).
    pub null: bool,
}

#[must_use]
pub const fn f(name: &'static str, ty: Ty) -> Field {
    Field {
        name,
        ty,
        null: false,
    }
}

#[must_use]
pub const fn fnull(name: &'static str, ty: Ty) -> Field {
    Field {
        name,
        ty,
        null: true,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaError(pub String);

fn err<T>(path: &str, what: &str) -> Result<T, SchemaError> {
    Err(SchemaError(format!("{path}: {what}")))
}

fn is_zero(v: &Json) -> bool {
    match v {
        Json::Null | Json::Bool(false) => true,
        Json::Num(n) => n.as_f64() == Some(0.0),
        Json::Str(s) => s.is_empty(),
        Json::Arr(a) => a.iter().all(is_zero),
        Json::Obj(m) => m.iter().all(|(_, v)| is_zero(v)),
        Json::Bool(true) => false,
    }
}

/// Validate and canonicalize.
pub fn check(ty: &Ty, v: &Json, path: &str) -> Result<Json, SchemaError> {
    match ty {
        Ty::Bool => match v {
            Json::Bool(_) => Ok(v.clone()),
            _ => err(path, "expected a boolean"),
        },
        Ty::Str => match v {
            Json::Str(_) => Ok(v.clone()),
            _ => err(path, "expected a string"),
        },
        Ty::StrIf(ok, what) => match v {
            Json::Str(s) if ok(s) => Ok(v.clone()),
            Json::Str(_) => err(path, &format!("value is not allowed ({what})")),
            _ => err(path, "expected a string"),
        },
        Ty::Int(lo, hi) => match v {
            Json::Num(n) => match n.as_i64() {
                Some(i) if (*lo..=*hi).contains(&i) => Ok(v.clone()),
                Some(_) => err(path, "number is out of the allowed range"),
                None => err(path, "expected an integer"),
            },
            _ => err(path, "expected an integer"),
        },
        Ty::StrEnum(allowed) => match v {
            Json::Str(s) if allowed.contains(&s.as_str()) => Ok(v.clone()),
            Json::Str(_) => err(path, "value is not allowed"),
            _ => err(path, "expected a string"),
        },
        Ty::StrOrArr => match v {
            Json::Str(_) => Ok(v.clone()),
            Json::Arr(a) if a.iter().all(|x| matches!(x, Json::Str(_))) => Ok(v.clone()),
            _ => err(path, "expected a string or an array of strings"),
        },
        Ty::Arr(inner) => match v {
            Json::Arr(a) => {
                let mut out = Vec::with_capacity(a.len());
                for (i, x) in a.iter().enumerate() {
                    out.push(check(inner, x, &format!("{path}[{i}]"))?);
                }
                Ok(Json::Arr(out))
            }
            _ => err(path, "expected an array"),
        },
        Ty::StrMap(inner) => match v {
            Json::Obj(m) => {
                let mut out = Vec::with_capacity(m.len());
                for (k, x) in m {
                    out.push((k.clone(), check(inner, x, &format!("{path}.{k}"))?));
                }
                Ok(Json::Obj(out))
            }
            _ => err(path, "expected an object"),
        },
        Ty::EmptyObj => match v {
            Json::Obj(m) if m.is_empty() => Ok(v.clone()),
            _ => err(path, "expected {}"),
        },
        Ty::Zero => {
            if is_zero(v) {
                Ok(v.clone())
            } else {
                err(path, "this setting is not allowed")
            }
        }
        Ty::False => match v {
            Json::Bool(false) => Ok(Json::Bool(false)),
            _ => err(path, "this setting is not allowed"),
        },
        Ty::Null => match v {
            Json::Null => Ok(Json::Null),
            _ => err(path, "this setting is not allowed"),
        },
        Ty::Raw => Ok(v.clone()),
        Ty::Obj(fields) => check_obj(fields, v, path),
    }
}

fn check_obj(fields: &'static [Field], v: &Json, path: &str) -> Result<Json, SchemaError> {
    let Json::Obj(members) = v else {
        return err(path, "expected an object");
    };
    let mut out: Vec<(String, Json)> = Vec::with_capacity(members.len());
    let mut used: Vec<&'static str> = Vec::new();
    for (k, x) in members {
        let (field, exact) = match fields.iter().find(|f| f.name == k) {
            Some(f) => (f, true),
            None => match fields.iter().find(|f| f.name.eq_ignore_ascii_case(k)) {
                Some(f) => (f, false),
                None => return err(path, &format!("unknown field {}", show(k))),
            },
        };
        if used.contains(&field.name) {
            return err(
                path,
                &format!(
                    "field {} is given twice (keys differing in case)",
                    field.name
                ),
            );
        }
        used.push(field.name);
        if !exact && break_mode("filter-case-sensitive") {
            // Defect: a case-sensitive filter that forwards the key it did
            // not recognise, which Go's decoder then matches anyway.
            out.push((k.clone(), x.clone()));
            continue;
        }
        let child = format!("{path}.{}", field.name);
        let val = if x.is_null() && field.null {
            Json::Null
        } else {
            check(&field.ty, x, &child)?
        };
        out.push((field.name.to_string(), val));
    }
    Ok(Json::Obj(out))
}

fn show(k: &str) -> String {
    let t: String = k.chars().filter(|c| !c.is_control()).take(40).collect();
    format!("\"{t}\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::json::parse;

    static HC: [Field; 2] = [f("Privileged", Ty::Zero), fnull("Binds", Ty::Arr(&Ty::Str))];
    static TOP: [Field; 1] = [f("HostConfig", Ty::Obj(&HC))];

    fn run(body: &str) -> Result<String, SchemaError> {
        let j = parse(body.as_bytes()).map_err(|e| SchemaError(e.0.into()))?;
        check(&Ty::Obj(&TOP), &j, "$").map(|c| c.to_string_json())
    }

    #[test]
    fn schema_table() {
        assert_eq!(
            run(r#"{"hostconfig":{"binds":null}}"#).unwrap(),
            r#"{"HostConfig":{"Binds":null}}"#
        );
        for (name, body) in [
            ("privileged", r#"{"HostConfig":{"Privileged":true}}"#),
            (
                "privileged, lower case",
                r#"{"HostConfig":{"privileged":true}}"#,
            ),
            (
                "hostconfig lower case",
                r#"{"hostconfig":{"PRIVILEGED":true}}"#,
            ),
            (
                "same key twice by case",
                r#"{"HostConfig":{},"hostconfig":{}}"#,
            ),
            ("unknown", r#"{"HostConfig":{"Devices":[]}}"#),
            ("non-ascii fold of s", "{\"Ho\u{17f}tConfig\":{}}"),
            ("string for bool", r#"{"HostConfig":{"Privileged":"true"}}"#),
            ("array", r#"[{"HostConfig":{}}]"#),
            ("null for non-null field", r#"{"HostConfig":null}"#),
        ] {
            assert!(run(body).is_err(), "{name}");
        }
    }
}
