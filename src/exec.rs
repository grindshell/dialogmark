//! Shared Luau-execution primitives used by both the runtime stepper
//! ([`crate::walker`]) and the editor simulator ([`crate::sim`]).
//!
//! Nothing here is public. The two consumer modules call into these helpers so
//! a script change (e.g. tightening `read_next_directive` or the per-walk
//! environment) stays in lock-step across the runtime and editor surfaces.

use std::collections::BTreeMap;

use mlua::{Lua, Table, Value};

use crate::blocks::{BlockKind, DialogBlock};
use crate::error::DialogError;
use crate::state::{DialogNext, DialogState, PresentedOption};

/// Maximum number of block visits before a walk aborts. Catches goto loops
/// between headings; does *not* catch infinite loops inside a single code
/// block.
pub(crate) const MAX_SIMULATION_STEPS: usize = 1000;

/// Trace previews longer than this get truncated with a trailing ellipsis.
#[cfg(feature = "editor")]
pub(crate) const PREVIEW_MAX_CHARS: usize = 120;

/// Depth cap on Lua↔JSON conversion. Defends against cycles in user tables.
pub(crate) const JSON_MAX_DEPTH: usize = 10;

const MANAGED_KEYS: &[&str] = &[
    "idx",
    "current_heading",
    "previous_heading",
    "next",
    "choice",
    // A per-segment heading-visibility override (surfaced on `DialogState`, not as
    // an extra): runtime-owned so it is neither restored on resume nor snapshotted
    // into `extras`, which is what makes it reset each segment.
    "show_heading",
];

/// A parsed `state.next` directive read after a code block runs. The walker /
/// simulator decides what each means (goto/exit terminate or redirect; present
/// pauses for the caller). Kept apart from the serialized [`DialogNext`] so the
/// `present` form — with its options — never has to round-trip through a saved
/// snapshot (it is always resolved within the producing segment).
pub(crate) enum NextDirective {
    /// `{ t = "goto", name = X }` — jump to heading `X` (None if `name` absent).
    Goto { name: Option<String> },
    /// `{ t = "exit" }` — end the walk.
    Exit,
    /// `{ t = "present", options = … }` — pause and surface choices.
    Present(Vec<PresentedOption>),
}

/// Allocate the persistent `state` table for a fresh walk.
pub(crate) fn create_state_table(lua: &Lua) -> Result<Table, DialogError> {
    lua.create_table()
        .map_err(|e| DialogError::StateInit(format!("create_table: {e}")))
}

/// Build a per-walk environment table (CHOICES_AND_SEGMENTED_WALK.md §4.1): it
/// carries `state` as the block-visible global and chains its `__index` to the
/// caller-supplied `base_env` (stdlib + any host modules). A block's global
/// writes land here — visible to later blocks of the *same* walk only — and
/// reads fall through to `base_env`, so many walks share one VM without
/// colliding and nothing a walk writes leaks into the base.
pub(crate) fn create_walk_env(
    lua: &Lua,
    base_env: &Table,
    state_t: &Table,
) -> Result<Table, DialogError> {
    let env = lua
        .create_table()
        .map_err(|e| DialogError::StateInit(format!("walk env: {e}")))?;
    env.set("state", state_t.clone())
        .map_err(|e| DialogError::StateInit(format!("walk env state: {e}")))?;
    let mt = lua
        .create_table()
        .map_err(|e| DialogError::StateInit(format!("walk env meta: {e}")))?;
    mt.set("__index", base_env.clone())
        .map_err(|e| DialogError::StateInit(format!("walk env __index: {e}")))?;
    env.set_metatable(Some(mt))
        .map_err(|e| DialogError::StateInit(format!("walk env metatable: {e}")))?;
    Ok(env)
}

/// Push every entry of `extras` onto `state_t` as a user-defined field.
/// Used by a resume to restore a saved snapshot. Managed keys are silently
/// filtered — they're owned by the runtime, not the caller.
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

/// Run `source` as a chunk against the per-walk environment `env`, refreshing
/// the managed fields first and reading back `state.next` after. The source
/// runs verbatim — `state` is reached as an env global (set by
/// [`create_walk_env`]), not as a parameter.
///
/// Returns `Ok(Some(directive))` if the script set `state.next`, `Ok(None)` if
/// it didn't, and `Err(String)` with a formatted Lua error on syntax / runtime
/// failure or a malformed `state.next`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_code_block(
    lua: &Lua,
    env: &Table,
    state_t: &Table,
    source: &str,
    block_idx: usize,
    cursor: usize,
    current_heading: &Option<String>,
    previous_heading: &Option<String>,
) -> Result<Option<NextDirective>, String> {
    refresh_managed_fields(state_t, cursor, current_heading, previous_heading)
        .map_err(|e| format!("state setup: {e}"))?;

    lua.load(source)
        .set_name(format!("dialog-block-{block_idx}"))
        .set_environment(env.clone())
        .exec()
        .map_err(|e| e.to_string())?;

    read_next_directive(state_t).map_err(|e| format!("invalid state.next: {e}"))
}

/// Read `state.next`. Returns `None` if nil, the parsed [`NextDirective`] for a
/// `{ t = "goto"|"exit"|"present", … }` table, and `Err` for any other value or
/// a malformed directive (mapped to an execution error by the caller).
pub(crate) fn read_next_directive(state_t: &Table) -> mlua::Result<Option<NextDirective>> {
    let nv: Value = state_t.get("next")?;
    match nv {
        Value::Nil => Ok(None),
        Value::Table(nt) => {
            let t: String = nt.get("t")?;
            match t.as_str() {
                "goto" => {
                    let name: Option<String> = match nt.get::<Value>("name")? {
                        Value::String(s) => Some(s.to_str()?.to_string()),
                        Value::Nil => None,
                        _ => {
                            return Err(mlua::Error::RuntimeError(
                                "state.next.name must be a string or nil".to_string(),
                            ));
                        }
                    };
                    Ok(Some(NextDirective::Goto { name }))
                }
                "exit" => Ok(Some(NextDirective::Exit)),
                "present" => Ok(Some(NextDirective::Present(read_present_options(&nt)?))),
                other => Err(mlua::Error::RuntimeError(format!(
                    "unsupported state.next.t {other:?}"
                ))),
            }
        }
        _ => Err(mlua::Error::RuntimeError(
            "state.next must be a table or nil".to_string(),
        )),
    }
}

/// Read and validate a `present` directive's `options` array
/// (CHOICES_AND_SEGMENTED_WALK.md §3): a non-empty array of
/// `{ id, label, target, note?, disabled? }` with unique ids.
fn read_present_options(nt: &Table) -> mlua::Result<Vec<PresentedOption>> {
    let Value::Table(opts_t) = nt.get::<Value>("options")? else {
        return Err(mlua::Error::RuntimeError(
            "state.next.options must be an array".to_string(),
        ));
    };
    let mut seen: BTreeMap<String, ()> = BTreeMap::new();
    let mut out = Vec::new();
    for (i, item) in opts_t.sequence_values::<Table>().enumerate() {
        let item = item?;
        let id = req_string(&item, "id", i)?;
        let label = req_string(&item, "label", i)?;
        let target = req_string(&item, "target", i)?;
        let note: Option<String> = item.get::<Option<String>>("note")?;
        let disabled: bool = item.get::<Option<bool>>("disabled")?.unwrap_or(false);
        if seen.insert(id.clone(), ()).is_some() {
            return Err(mlua::Error::RuntimeError(format!(
                "duplicate present option id {id:?}"
            )));
        }
        out.push(PresentedOption {
            id,
            label,
            target,
            note,
            disabled,
        });
    }
    if out.is_empty() {
        return Err(mlua::Error::RuntimeError(
            "state.next.options must be a non-empty array".to_string(),
        ));
    }
    Ok(out)
}

/// Read a required string field of a `present` option, with a clear error.
fn req_string(t: &Table, key: &str, i: usize) -> mlua::Result<String> {
    match t.get::<Value>(key)? {
        Value::String(s) => Ok(s.to_str()?.to_string()),
        _ => Err(mlua::Error::RuntimeError(format!(
            "present option #{} is missing string `{key}`",
            i + 1
        ))),
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
        show_heading: read_show_heading(state_t),
        extras: snapshot_extras(state_t),
    }
}

/// Read the script-set `state.show_heading` per-segment heading override: `Some`
/// only for an explicit boolean, `None` for anything else (unset / non-bool),
/// meaning "use the frontmatter default".
fn read_show_heading(state_t: &Table) -> Option<bool> {
    match state_t.get::<Value>("show_heading") {
        Ok(Value::Boolean(b)) => Some(b),
        _ => None,
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

    // --- Phase 0 feasibility gate (CHOICES_AND_SEGMENTED_WALK.md §4.1 / §8) -----
    //
    // The per-walk environment isolation rests on `Chunk::set_environment` on
    // Luau. These two tests pin the behavior the segmented walk depends on so an
    // mlua/Luau upgrade that breaks it fails loudly. They model the target design
    // (a block runs as a raw chunk against a per-walk env table whose `__index`
    // chains to a caller base, with `state` injected as an env field) — the same
    // sandbox shape the editor's skill runner uses in production.

    /// A block's global writes land in its per-walk env (not VM globals); a later
    /// block of the *same* walk reads them; a *fresh* walk's env cannot; reads of
    /// host-injected values and stdlib fall through `__index` to the base.
    #[test]
    fn gate_per_walk_env_isolation_and_fallthrough() {
        let lua = Lua::new();

        // The caller-supplied base: stdlib via `__index -> globals`, plus a
        // host-injected read-only value (stands in for `ctx` / a host module).
        let base = lua.create_table().unwrap();
        base.set("HOST_CONST", 100i64).unwrap();
        let base_mt = lua.create_table().unwrap();
        base_mt.set("__index", lua.globals()).unwrap();
        base.set_metatable(Some(base_mt)).unwrap();

        let make_env = |state_t: &Table| -> Table {
            let env = lua.create_table().unwrap();
            env.set("state", state_t.clone()).unwrap();
            let mt = lua.create_table().unwrap();
            mt.set("__index", base.clone()).unwrap();
            env.set_metatable(Some(mt)).unwrap();
            env
        };

        // Walk 1: block A defines helper globals; block B reads them back, plus
        // the host const and a stdlib module (proving fallthrough both levels).
        let s1 = lua.create_table().unwrap();
        let env1 = make_env(&s1);
        lua.load("HELPER = function() return 42 end\nLESSONS = { 1, 2, 3 }")
            .set_environment(env1.clone())
            .exec()
            .unwrap();
        lua.load(
            "state.helper = HELPER()\nstate.count = #LESSONS\nstate.host = HOST_CONST\nstate.has_str = (string ~= nil)",
        )
        .set_environment(env1.clone())
        .exec()
        .unwrap();
        assert_eq!(
            s1.get::<i64>("helper").unwrap(),
            42,
            "later block sees earlier block's helper"
        );
        assert_eq!(
            s1.get::<i64>("count").unwrap(),
            3,
            "later block sees earlier block's table"
        );
        assert_eq!(
            s1.get::<i64>("host").unwrap(),
            100,
            "host value falls through __index"
        );
        assert!(
            s1.get::<bool>("has_str").unwrap(),
            "stdlib falls through to globals"
        );

        // Walk 2 in a fresh env: walk 1's helpers are invisible; base still is.
        let s2 = lua.create_table().unwrap();
        let env2 = make_env(&s2);
        lua.load("state.helper_nil = (HELPER == nil)\nstate.host = HOST_CONST")
            .set_environment(env2.clone())
            .exec()
            .unwrap();
        assert!(
            s2.get::<bool>("helper_nil").unwrap(),
            "a fresh walk cannot see another walk's globals"
        );
        assert_eq!(s2.get::<i64>("host").unwrap(), 100);

        // Neither the shared base nor the VM globals are polluted by a walk's writes.
        assert!(
            lua.globals().get::<Value>("HELPER").unwrap().is_nil(),
            "writes must not leak to VM globals"
        );
        assert!(
            base.get::<Value>("HELPER").unwrap().is_nil(),
            "writes must not leak to the base env"
        );
    }

    /// The §8 optimization probe: a block source compiled to Luau bytecode
    /// **once** (via `mlua::Compiler` — Luau has no `Function::dump`) can be
    /// re-bound to a fresh per-walk env on each run, each run seeing its own
    /// injected values and writing only into its own env. If this fails on the
    /// pinned mlua, the implementation falls back to recompiling source per
    /// segment (correct, slightly slower) — so this test documents which path is
    /// available rather than gating the feature.
    #[test]
    fn gate_bytecode_chunk_rebinds_env_per_run() {
        let lua = Lua::new();
        let bytecode = mlua::Compiler::new()
            .compile("state.seen = MARK\nWROTE = true")
            .expect("Luau compiles the block to bytecode");

        let run = |mark: i64| -> (Table, Table) {
            let st = lua.create_table().unwrap();
            let env = lua.create_table().unwrap();
            env.set("state", st.clone()).unwrap();
            env.set("MARK", mark).unwrap();
            let mt = lua.create_table().unwrap();
            mt.set("__index", lua.globals()).unwrap();
            env.set_metatable(Some(mt)).unwrap();
            lua.load(bytecode.as_slice())
                .set_environment(env.clone())
                .exec()
                .expect("precompiled bytecode re-binds to a fresh env");
            (st, env)
        };

        let (sa, ea) = run(7);
        assert_eq!(sa.get::<i64>("seen").unwrap(), 7);
        assert!(ea.get::<bool>("WROTE").unwrap());
        let (sb, eb) = run(9);
        assert_eq!(
            sb.get::<i64>("seen").unwrap(),
            9,
            "same bytecode reads the new env's value"
        );
        assert!(eb.get::<bool>("WROTE").unwrap());
    }
}
