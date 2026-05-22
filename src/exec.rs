//! Shared Luau-execution primitives used by both the runtime stepper
//! ([`crate::walker`]) and the editor simulator ([`crate::sim`]).
//!
//! Nothing here is public. The two consumer modules call into these helpers so
//! a script change (e.g. tightening `read_next` or tweaking the wrapper
//! template) stays in lock-step across the runtime and editor surfaces.

use std::collections::BTreeMap;

use mlua::{Function, Lua, Table, Value};

use crate::blocks::{BlockKind, DialogBlock};
use crate::error::DialogError;
use crate::state::{DialogNext, DialogState};

/// Maximum number of block visits before a walk aborts. Catches goto loops
/// between headings; does *not* catch infinite loops inside a single code
/// block.
pub(crate) const MAX_SIMULATION_STEPS: usize = 1000;

/// Trace previews longer than this get truncated with a trailing ellipsis.
#[cfg(feature = "editor")]
pub(crate) const PREVIEW_MAX_CHARS: usize = 120;

/// Depth cap on Lua↔JSON conversion. Defends against cycles in user tables.
pub(crate) const JSON_MAX_DEPTH: usize = 10;

const MANAGED_KEYS: &[&str] = &["idx", "current_heading", "previous_heading", "next"];

/// Allocate the persistent `state` table for a fresh walk.
pub(crate) fn create_state_table(lua: &Lua) -> Result<Table, DialogError> {
    lua.create_table()
        .map_err(|e| DialogError::StateInit(format!("create_table: {e}")))
}

/// Push every entry of `extras` onto `state_t` as a user-defined field.
/// Used by `walk_with_state` to restore a saved snapshot. Managed keys are
/// silently filtered — they're owned by the runtime, not the caller.
pub(crate) fn seed_extras(
    lua: &Lua,
    state_t: &Table,
    extras: &BTreeMap<String, serde_json::Value>,
) -> Result<(), DialogError> {
    for (k, v) in extras {
        if MANAGED_KEYS.contains(&k.as_str()) {
            continue;
        }
        let lv = json_to_lua(lua, v, 0)
            .map_err(|e| DialogError::StateInit(format!("seed key {k:?}: {e}")))?;
        state_t
            .set(k.as_str(), lv)
            .map_err(|e| DialogError::StateInit(format!("seed key {k:?}: {e}")))?;
    }
    Ok(())
}

/// Reset the four runtime-owned fields on `state_t` for the block at `idx`.
/// Crucially, `state.next` is cleared to nil so a stale redirect from the
/// previous block doesn't re-fire.
pub(crate) fn refresh_managed_fields(
    state_t: &Table,
    idx: usize,
    current_heading: &Option<String>,
    previous_heading: &Option<String>,
) -> mlua::Result<()> {
    state_t.set("idx", idx as i64)?;
    match current_heading {
        Some(s) => state_t.set("current_heading", s.clone())?,
        None => state_t.set("current_heading", Value::Nil)?,
    }
    match previous_heading {
        Some(s) => state_t.set("previous_heading", s.clone())?,
        None => state_t.set("previous_heading", Value::Nil)?,
    }
    state_t.set("next", Value::Nil)?;
    Ok(())
}

/// Lift `source` into `function(state) … end`, refresh managed fields, call
/// the function, and read back `state.next`. The user's script source is
/// inserted verbatim between the function signature and `end`.
///
/// Returns `Ok(Some(next))` if the script set `state.next` to a goto/exit
/// redirect, `Ok(None)` if it didn't, and `Err(String)` with a formatted Lua
/// error on syntax or runtime failure.
pub(crate) fn run_code_block(
    lua: &Lua,
    state_t: &Table,
    source: &str,
    block_idx: usize,
    cursor: usize,
    current_heading: &Option<String>,
    previous_heading: &Option<String>,
) -> Result<Option<DialogNext>, String> {
    refresh_managed_fields(state_t, cursor, current_heading, previous_heading)
        .map_err(|e| format!("state setup: {e}"))?;

    let wrapped = format!("return function(state)\n{source}\nend");
    let f: Function = lua
        .load(&wrapped)
        .set_name(format!("dialog-block-{block_idx}"))
        .eval()
        .map_err(|e| e.to_string())?;

    f.call::<()>(state_t.clone()).map_err(|e| e.to_string())?;

    read_next(state_t).map_err(|e| format!("invalid state.next: {e}"))
}

/// Read `state.next`. Returns `None` if nil. Returns `Err` if the value is
/// neither nil nor a properly shaped `{ t, name? }` table.
pub(crate) fn read_next(state_t: &Table) -> mlua::Result<Option<DialogNext>> {
    let nv: Value = state_t.get("next")?;
    match nv {
        Value::Nil => Ok(None),
        Value::Table(nt) => {
            let t: String = nt.get("t")?;
            let name: Option<String> = match nt.get::<Value>("name")? {
                Value::String(s) => Some(s.to_str()?.to_string()),
                Value::Nil => None,
                _ => {
                    return Err(mlua::Error::RuntimeError(
                        "state.next.name must be a string or nil".to_string(),
                    ));
                }
            };
            Ok(Some(DialogNext { t, name }))
        }
        _ => Err(mlua::Error::RuntimeError(
            "state.next must be a table or nil".to_string(),
        )),
    }
}

/// Snapshot of every user-defined field on `state_t`. The four managed keys
/// are skipped; everything else is converted to a `serde_json::Value` for
/// cross-process persistence.
pub(crate) fn snapshot_extras(state_t: &Table) -> BTreeMap<String, serde_json::Value> {
    let mut out = BTreeMap::new();
    for pair in state_t.clone().pairs::<Value, Value>() {
        let Ok((k, v)) = pair else { continue };
        let Value::String(s) = &k else { continue };
        let Ok(key) = s.to_str() else { continue };
        let key_str: &str = &key;
        if MANAGED_KEYS.contains(&key_str) {
            continue;
        }
        out.insert(key_str.to_string(), lua_value_to_json(&v, 0));
    }
    out
}

/// Assemble a full `DialogState` snapshot from `state_t`. The four managed
/// fields are passed in explicitly so they reflect the cursor / heading state
/// at the trace step, not whatever stale value the table still holds.
pub(crate) fn build_state_snapshot(
    state_t: &Table,
    idx: usize,
    current_heading: &Option<String>,
    previous_heading: &Option<String>,
    next: Option<DialogNext>,
) -> DialogState {
    DialogState {
        idx,
        current_heading: current_heading.clone(),
        previous_heading: previous_heading.clone(),
        next,
        extras: snapshot_extras(state_t),
    }
}

/// Exact-text heading lookup. Returns the first heading block whose `text`
/// matches `name`, or `None` if no such heading exists.
pub(crate) fn resolve_goto<'a>(blocks: &'a [DialogBlock], name: &str) -> Option<&'a DialogBlock> {
    blocks
        .iter()
        .find(|b| b.kind == BlockKind::Heading && b.text == name)
}

/// Compress `text` to a single-line preview suitable for the trace UI.
/// Truncated with `…` if longer than `PREVIEW_MAX_CHARS` chars.
#[cfg(feature = "editor")]
pub(crate) fn preview_source(text: &str) -> String {
    let oneline: String = text
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let oneline = oneline.trim();
    if oneline.chars().count() > PREVIEW_MAX_CHARS {
        let truncated: String = oneline.chars().take(PREVIEW_MAX_CHARS).collect();
        format!("{truncated}…")
    } else {
        oneline.to_string()
    }
}

/// Convert an `mlua::Value` to `serde_json::Value`. Functions/threads/userdata
/// become stringified placeholders. Recurses with a depth cap.
pub(crate) fn lua_value_to_json(v: &Value, depth: usize) -> serde_json::Value {
    use serde_json::Value as J;
    if depth > JSON_MAX_DEPTH {
        return J::String("<depth-limit>".to_string());
    }
    match v {
        Value::Nil => J::Null,
        Value::Boolean(b) => J::Bool(*b),
        Value::Integer(i) => J::Number(serde_json::Number::from(*i)),
        Value::Number(n) => serde_json::Number::from_f64(*n)
            .map(J::Number)
            .unwrap_or(J::Null),
        Value::String(s) => s
            .to_str()
            .map(|b| J::String(b.to_string()))
            .unwrap_or_else(|_| J::String("<invalid utf8>".to_string())),
        Value::Table(t) => lua_table_to_json(t, depth),
        Value::Function(_) => J::String("<function>".to_string()),
        Value::Thread(_) => J::String("<thread>".to_string()),
        Value::LightUserData(_) | Value::UserData(_) => J::String("<userdata>".to_string()),
        Value::Error(e) => J::String(format!("<error: {e}>")),
        _ => J::String("<unknown>".to_string()),
    }
}

/// Serialize a Lua table as either a JSON array (when keys are exactly
/// `1..=raw_len`) or a JSON object.
fn lua_table_to_json(t: &Table, depth: usize) -> serde_json::Value {
    use serde_json::Value as J;
    let len = t.raw_len();
    if len > 0 {
        let mut arr = Vec::with_capacity(len);
        let mut ok = true;
        for i in 1..=len as i64 {
            match t.raw_get::<Value>(i) {
                Ok(v) => arr.push(lua_value_to_json(&v, depth + 1)),
                Err(_) => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            let mut has_extra = false;
            for (k, _) in t.clone().pairs::<Value, Value>().flatten() {
                match &k {
                    Value::Integer(i) if *i >= 1 && (*i as u64) <= len as u64 => {}
                    _ => {
                        has_extra = true;
                        break;
                    }
                }
            }
            if !has_extra {
                return J::Array(arr);
            }
        }
    }
    let mut obj = serde_json::Map::new();
    for pair in t.clone().pairs::<Value, Value>() {
        let Ok((k, v)) = pair else { continue };
        let key = match &k {
            Value::String(s) => s
                .to_str()
                .map(|b| b.to_string())
                .unwrap_or_else(|_| format!("{k:?}")),
            Value::Integer(i) => i.to_string(),
            Value::Number(n) => n.to_string(),
            Value::Boolean(b) => b.to_string(),
            _ => format!("{k:?}"),
        };
        obj.insert(key, lua_value_to_json(&v, depth + 1));
    }
    J::Object(obj)
}

/// Convert a `serde_json::Value` to `mlua::Value`, mirroring
/// [`lua_value_to_json`]. Used only by `seed_extras` for save/load.
fn json_to_lua(lua: &Lua, v: &serde_json::Value, depth: usize) -> mlua::Result<Value> {
    use serde_json::Value as J;
    if depth > JSON_MAX_DEPTH {
        return Ok(Value::String(lua.create_string("<depth-limit>")?));
    }
    Ok(match v {
        J::Null => Value::Nil,
        J::Bool(b) => Value::Boolean(*b),
        J::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Integer(i)
            } else if let Some(f) = n.as_f64() {
                Value::Number(f)
            } else {
                Value::Nil
            }
        }
        J::String(s) => Value::String(lua.create_string(s)?),
        J::Array(items) => {
            let t = lua.create_table()?;
            for (i, item) in items.iter().enumerate() {
                t.set(i as i64 + 1, json_to_lua(lua, item, depth + 1)?)?;
            }
            Value::Table(t)
        }
        J::Object(map) => {
            let t = lua.create_table()?;
            for (k, v) in map {
                t.set(k.as_str(), json_to_lua(lua, v, depth + 1)?)?;
            }
            Value::Table(t)
        }
    })
}

/// Pre-scan `[0, cursor)` to establish the heading state visible to the block
/// at `cursor`. Returns `(current_heading, previous_heading)`.
pub(crate) fn prescan_headings(
    blocks: &[DialogBlock],
    cursor: usize,
) -> (Option<String>, Option<String>) {
    let mut current: Option<String> = None;
    let mut previous: Option<String> = None;
    let limit = cursor.min(blocks.len());
    for b in &blocks[..limit] {
        if b.kind == BlockKind::Heading {
            previous = current.take();
            current = Some(b.text.clone());
        }
    }
    (current, previous)
}

/// Find the index of the first heading block. If no headings exist, returns
/// `blocks.len()` — the whole dialog is treated as prelude.
pub(crate) fn first_heading_idx(blocks: &[DialogBlock]) -> usize {
    blocks
        .iter()
        .position(|b| b.kind == BlockKind::Heading)
        .unwrap_or(blocks.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "editor")]
    #[test]
    fn preview_truncates_long_input() {
        let s = "a".repeat(200);
        let p = preview_source(&s);
        assert!(p.ends_with('…'));
        assert_eq!(p.chars().count(), PREVIEW_MAX_CHARS + 1);
    }

    #[cfg(feature = "editor")]
    #[test]
    fn preview_collapses_newlines() {
        assert_eq!(preview_source("foo\nbar\r\nbaz"), "foo bar  baz");
    }

    #[test]
    fn json_roundtrip_primitive() {
        let lua = Lua::new();
        let t = lua.create_table().unwrap();
        let mut extras = BTreeMap::new();
        extras.insert("flag".into(), serde_json::json!(true));
        extras.insert("n".into(), serde_json::json!(7));
        extras.insert("s".into(), serde_json::json!("hi"));
        seed_extras(&lua, &t, &extras).unwrap();
        let out = snapshot_extras(&t);
        assert_eq!(out["flag"], serde_json::json!(true));
        assert_eq!(out["n"], serde_json::json!(7));
        assert_eq!(out["s"], serde_json::json!("hi"));
    }

    #[test]
    fn json_roundtrip_array_and_object() {
        let lua = Lua::new();
        let t = lua.create_table().unwrap();
        let mut extras = BTreeMap::new();
        extras.insert("items".into(), serde_json::json!(["a", "b", "c"]));
        extras.insert("flags".into(), serde_json::json!({ "x": 1, "y": false }));
        seed_extras(&lua, &t, &extras).unwrap();
        let out = snapshot_extras(&t);
        assert_eq!(out["items"], serde_json::json!(["a", "b", "c"]));
        assert_eq!(out["flags"]["x"], serde_json::json!(1));
        assert_eq!(out["flags"]["y"], serde_json::json!(false));
    }

    #[test]
    fn snapshot_skips_managed_keys() {
        let lua = Lua::new();
        let t = lua.create_table().unwrap();
        t.set("idx", 7).unwrap();
        t.set("current_heading", "X").unwrap();
        t.set("previous_heading", "Y").unwrap();
        t.set("tag", "keep").unwrap();
        let out = snapshot_extras(&t);
        assert!(!out.contains_key("idx"));
        assert!(!out.contains_key("current_heading"));
        assert!(!out.contains_key("previous_heading"));
        assert_eq!(out["tag"], serde_json::json!("keep"));
    }
}
