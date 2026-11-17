//! Checking and cleaning what the model sends back (notes, flow chart, process chart), and the JSON schemas that tell
//! Gemini what shape to answer in. Port of validateNotes/validateProcess/parseNotes/parseFlow in server/gemini.ts.
//!
//! The notes are kept as `serde_json::Value` (not a struct) because the web version stores and sends the same JSON, and
//! these functions accept both the old shape (plain strings) and the new one (objects with an `at` time stamp).
use pechu_common::user_err;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

const KINDS: [&str; 5] = ["root", "topic", "point", "decision", "action"];
const STEP_KINDS: [&str; 4] = ["start", "step", "decision", "end"];
const MAX_STEPS: usize = 30;

/// The text of a value when it is a string, trimmed; "" for anything else.
fn text(v: &Value) -> String {
    v.as_str().map(|s| s.trim().to_string()).unwrap_or_default()
}

/// `at` is seconds into the recording: kept only as a non-negative whole number.
fn at_of(v: &Value) -> Option<Value> {
    let n = v.as_f64()?;
    (n.is_finite() && n >= 0.0).then(|| json!(n.round() as u64))
}

/// Adds `"at"` to an object when the value is a good time.
fn put_at(obj: &mut Map<String, Value>, v: &Value) {
    if let Some(at) = at_of(v) {
        obj.insert("at".into(), at);
    }
}

/// Adds a text field only when it is not empty after trimming (`owner`, `due`, `who`, `label`).
fn put_text(obj: &mut Map<String, Value>, name: &str, v: &Value) {
    let t = text(v);
    if !t.is_empty() {
        obj.insert(name.into(), json!(t));
    }
}

/// v1 notes have plain strings, v2 has { text, at }. Returns the v2 item, or None when there is no text.
fn item_of(v: &Value) -> Option<Map<String, Value>> {
    let (t, at) = match v.as_str() {
        Some(s) => (s.trim().to_string(), None),
        None => (text(&v["text"]), at_of(&v["at"])),
    };
    if t.is_empty() {
        return None;
    }
    let mut item = Map::new();
    item.insert("text".into(), json!(t));
    item.extend(at.map(|a| ("at".to_string(), a)));
    Some(item)
}

fn item_list(v: &Value) -> Vec<Value> {
    v.as_array().map(|a| a.iter().filter_map(item_of).map(Value::Object).collect()).unwrap_or_default()
}

fn check_flow(v: &Value, depth: u32) -> Result<Value, String> {
    let label = text(&v["label"]);
    if !v.is_object() || label.is_empty() {
        return Err("flow node has no label".into());
    }
    let kind = match v["kind"].as_str() {
        Some(k) if KINDS.contains(&k) => k,
        _ if depth == 1 => "root",
        _ => "point",
    };
    let mut node = Map::new();
    node.insert("label".into(), json!(label));
    node.insert("kind".into(), json!(kind));
    put_at(&mut node, &v["at"]);
    // the chart is at most 4 levels deep; anything below is dropped
    if depth < 4 {
        if let Some(children) = v["children"].as_array().filter(|c| !c.is_empty()) {
            let checked: Result<Vec<Value>, String> = children.iter().map(|c| check_flow(c, depth + 1)).collect();
            node.insert("children".into(), Value::Array(checked?));
        }
    }
    Ok(Value::Object(node))
}

/// Returns a cleaned process `{ steps }`, or None when fewer than 3 usable steps remain. Accepts `{ steps }` or the bare list.
pub fn validate_process(v: &Value) -> Option<Value> {
    let raw = if v.is_array() { v.as_array() } else { v["steps"].as_array() }?;
    let valid: Vec<&Value> = raw.iter().filter(|s| s.is_object() && !text(&s["label"]).is_empty()).take(MAX_STEPS).collect();
    if valid.len() < 3 {
        return None;
    }
    // Give every step a unique id. Arrows point to the first step that used a given id.
    let mut used: HashSet<String> = HashSet::new();
    let mut by_given: HashMap<String, String> = HashMap::new();
    let mut ids: Vec<String> = Vec::new();
    for (i, s) in valid.iter().enumerate() {
        let given = Some(text(&s["id"])).filter(|g| !g.is_empty()).unwrap_or_else(|| format!("s{}", i + 1));
        let mut id = given.clone();
        let mut n = 2;
        while used.contains(&id) {
            id = format!("{given}-{n}");
            n += 1;
        }
        used.insert(id.clone());
        by_given.entry(given).or_insert_with(|| id.clone());
        ids.push(id);
    }
    let steps: Vec<Value> = valid
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let kind = match s["kind"].as_str() {
                _ if i == 0 => "start",
                Some(k) if STEP_KINDS.contains(&k) => k,
                _ => "step",
            };
            // an "end" step has no arrows; arrows to unknown ids are dropped
            let next: Vec<Value> = if kind == "end" {
                vec![]
            } else {
                s["next"]
                    .as_array()
                    .map(|a| a.as_slice())
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|n| {
                        let to = by_given.get(&text(&n["to"]))?;
                        let mut arrow = Map::new();
                        arrow.insert("to".into(), json!(to));
                        put_text(&mut arrow, "label", &n["label"]);
                        Some(Value::Object(arrow))
                    })
                    .collect()
            };
            let mut step = Map::new();
            step.insert("id".into(), json!(ids[i]));
            step.insert("label".into(), json!(text(&s["label"])));
            step.insert("kind".into(), json!(kind));
            put_text(&mut step, "who", &s["who"]);
            put_at(&mut step, &s["at"]);
            step.insert("next".into(), Value::Array(next));
            Value::Object(step)
        })
        .collect();
    Some(json!({ "steps": steps }))
}

/// Returns a cleaned copy in the new (v2) shape, or an error saying what is wrong. Accepts old (v1) notes too.
pub fn validate_notes(v: &Value) -> anyhow::Result<Value> {
    validate_notes_inner(v).map_err(|m| anyhow::anyhow!(m))
}

fn validate_notes_inner(v: &Value) -> Result<Value, String> {
    if !v.is_object() {
        return Err("notes is not an object".into());
    }
    let title = text(&v["title"]);
    if title.is_empty() {
        return Err("notes.title missing".into());
    }
    let Some(summary) = v["summary"].as_str() else { return Err("notes.summary missing".into()) };
    if !v["keyPoints"].is_array() {
        return Err("notes.keyPoints must be a list".into());
    }

    let actions: Vec<Value> = v["actions"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .filter(|a| !text(&a["task"]).is_empty())
        .map(|a| {
            let mut action = Map::new();
            action.insert("task".into(), json!(text(&a["task"])));
            put_text(&mut action, "owner", &a["owner"]);
            put_text(&mut action, "due", &a["due"]);
            put_at(&mut action, &a["at"]);
            Value::Object(action)
        })
        .collect();

    let decisions: Vec<Value> = v["decisions"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .filter_map(|d| {
            let mut item = item_of(d)?;
            put_text(&mut item, "who", &d["who"]);
            Some(Value::Object(item))
        })
        .collect();

    let sections: Vec<Value> = v["sections"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .filter_map(|s| {
            let bullets: Vec<Value> = item_list(&s["bullets"]).into_iter().take(12).collect();
            if bullets.is_empty() {
                return None;
            }
            let heading = Some(text(&s["heading"])).filter(|h| !h.is_empty()).unwrap_or_else(|| "Details".into());
            Some(json!({ "heading": heading, "bullets": bullets }))
        })
        .take(10)
        .collect();

    let mut notes = Map::new();
    notes.insert("title".into(), json!(title));
    notes.insert("summary".into(), json!(summary.trim()));
    notes.insert("sections".into(), Value::Array(sections));
    notes.insert("keyPoints".into(), Value::Array(item_list(&v["keyPoints"])));
    notes.insert("decisions".into(), Value::Array(decisions));
    notes.insert("actions".into(), Value::Array(actions));
    notes.insert("flow".into(), check_flow(&v["flow"], 1)?);
    if let Some(process) = validate_process(&v["process"]) {
        notes.insert("process".into(), process);
    }
    Ok(Value::Object(notes))
}

static FENCE_START: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^```(?:json)?\s*").unwrap());
static FENCE_END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*```$").unwrap());

// The model sometimes wraps its JSON in a ```json fence even when asked not to.
fn parse_json(raw: &str) -> anyhow::Result<Value> {
    let trimmed = raw.trim();
    let json = FENCE_START.replace(trimmed, "");
    let json = FENCE_END.replace(&json, "");
    serde_json::from_str(&json).map_err(|_| user_err("Gemini sent back notes that could not be read. Try again."))
}

/// Parses and validates the model's JSON text.
pub fn parse_notes(raw: &str) -> anyhow::Result<Value> {
    let v = parse_json(raw)?;
    validate_notes_inner(&v).map_err(|m| user_err(format!("Gemini sent back notes in the wrong shape ({m}). Try again.")))
}

/// `{flow, process?}` from the model's JSON text (the flow-only answer).
pub fn parse_flow(raw: &str) -> anyhow::Result<Value> {
    let v = parse_json(raw)?;
    let flow = check_flow(&v["flow"], 1)
        .map_err(|m| user_err(format!("Gemini sent back a flow chart in the wrong shape ({m}). Try again.")))?;
    let mut out = Map::new();
    out.insert("flow".into(), flow);
    if let Some(process) = validate_process(&v["process"]) {
        out.insert("process".into(), process);
    }
    Ok(Value::Object(out))
}

/// Removes every `at` from a value. Gemini sometimes adds `at` even when asked not to; without stamps in the
/// transcript any value would be made up.
pub fn without_times(v: &mut Value) {
    match v {
        Value::Object(map) => {
            map.remove("at");
            map.values_mut().for_each(without_times);
        }
        Value::Array(list) => list.iter_mut().for_each(without_times),
        _ => {}
    }
}

// --- the shapes we ask Gemini to answer in (Gemini's schema type names are upper case) ---

fn item_schema(extra: Option<(&str, Value)>) -> Value {
    let mut props = json!({ "text": { "type": "STRING" }, "at": { "type": "INTEGER" } });
    if let Some((name, schema)) = extra {
        props[name] = schema;
    }
    json!({ "type": "OBJECT", "properties": props, "required": ["text"] })
}

fn flow_schema(levels: u32) -> Value {
    let mut props = json!({
        "label": { "type": "STRING" },
        "kind": { "type": "STRING", "enum": KINDS },
        "at": { "type": "INTEGER" },
    });
    if levels > 1 {
        props["children"] = json!({ "type": "ARRAY", "items": flow_schema(levels - 1) });
    }
    json!({ "type": "OBJECT", "properties": props, "required": ["label", "kind"] })
}

// A flat list: arrows refer to other steps by id, so no nesting is needed.
fn process_schema() -> Value {
    json!({
        "type": "OBJECT",
        "properties": { "steps": { "type": "ARRAY", "items": {
            "type": "OBJECT",
            "properties": {
                "id": { "type": "STRING" },
                "label": { "type": "STRING" },
                "kind": { "type": "STRING", "enum": STEP_KINDS },
                "who": { "type": "STRING" },
                "at": { "type": "INTEGER" },
                "next": { "type": "ARRAY", "items": {
                    "type": "OBJECT",
                    "properties": { "to": { "type": "STRING" }, "label": { "type": "STRING" } },
                    "required": ["to"],
                } },
            },
            "required": ["id", "label", "kind", "next"],
        } } },
        "required": ["steps"],
    })
}

pub fn notes_schema() -> Value {
    json!({
        "type": "OBJECT",
        "properties": {
            "title": { "type": "STRING" },
            "summary": { "type": "STRING" },
            "sections": { "type": "ARRAY", "items": {
                "type": "OBJECT",
                "properties": { "heading": { "type": "STRING" }, "bullets": { "type": "ARRAY", "items": item_schema(None) } },
                "required": ["heading", "bullets"],
            } },
            "keyPoints": { "type": "ARRAY", "items": item_schema(None) },
            "decisions": { "type": "ARRAY", "items": item_schema(Some(("who", json!({ "type": "STRING" })))) },
            "actions": { "type": "ARRAY", "items": {
                "type": "OBJECT",
                "properties": {
                    "task": { "type": "STRING" },
                    "owner": { "type": "STRING" },
                    "due": { "type": "STRING" },
                    "at": { "type": "INTEGER" },
                },
                "required": ["task"],
            } },
            "flow": flow_schema(4),
            "process": process_schema(),
        },
        "required": ["title", "summary", "sections", "keyPoints", "decisions", "actions", "flow"],
    })
}

pub fn flow_only_schema() -> Value {
    json!({
        "type": "OBJECT",
        "properties": { "flow": flow_schema(4), "process": process_schema() },
        "required": ["flow"],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- tests/notes.test.ts ---

    fn good() -> Value {
        json!({
            "title": "Budget review",
            "summary": "The team reviewed the budget.",
            "keyPoints": ["Costs are up", "Hiring paused"],
            "decisions": ["Pause hiring"],
            "actions": [{ "task": "Send the report", "owner": "Ravi" }, { "task": "Book room", "owner": "", "due": "" }],
            "flow": {
                "label": "Budget review",
                "kind": "root",
                "children": [
                    { "label": "Costs", "kind": "topic", "children": [
                        { "label": "Costs are up", "kind": "point" },
                        { "label": "Pause hiring", "kind": "decision" },
                    ] },
                ],
            },
        })
    }

    fn with(base: Value, field: &str, value: Value) -> Value {
        let mut v = base;
        v[field] = value;
        v
    }

    #[test]
    fn accepts_a_good_notes_object_and_drops_empty_owner_and_due() {
        let n = validate_notes(&good()).unwrap();
        assert_eq!(n["title"], "Budget review");
        assert_eq!(n["actions"], json!([{ "task": "Send the report", "owner": "Ravi" }, { "task": "Book room" }]));
        assert_eq!(n["flow"]["children"][0]["children"][1]["kind"], "decision");
    }

    #[test]
    fn rejects_malformed_shapes() {
        assert!(validate_notes(&Value::Null).is_err());
        assert!(validate_notes(&json!("text")).is_err());
        assert!(validate_notes(&with(good(), "title", json!(""))).is_err());
        assert!(validate_notes(&with(good(), "summary", json!(5))).is_err());
        assert!(validate_notes(&with(good(), "keyPoints", json!("one"))).is_err());
        let mut no_flow = good();
        no_flow.as_object_mut().unwrap().remove("flow");
        assert!(validate_notes(&no_flow).is_err());
        assert!(validate_notes(&with(good(), "flow", json!({ "kind": "root" }))).is_err());
        let bad_child = json!({ "label": "x", "kind": "root", "children": [{ "kind": "topic" }] });
        assert!(validate_notes(&with(good(), "flow", bad_child)).is_err());
    }

    #[test]
    fn flow_is_cut_at_depth_4_and_unknown_kinds_are_repaired() {
        let deep = json!({ "label": "a", "kind": "root", "children": [
            { "label": "b", "kind": "weird", "children": [
                { "label": "c", "kind": "point", "children": [
                    { "label": "d", "kind": "point", "children": [{ "label": "e", "kind": "point" }] },
                ] },
            ] },
        ] });
        let n = validate_notes(&with(good(), "flow", deep)).unwrap();
        assert_eq!(n["flow"]["children"][0]["kind"], "point");
        assert!(n["flow"]["children"][0]["children"][0]["children"][0].get("children").is_none());
    }

    #[test]
    fn parse_notes_reads_fenced_json_and_rejects_text_that_is_not_notes() {
        let fenced = format!("```json\n{}\n```", good());
        assert_eq!(parse_notes(&fenced).unwrap()["title"], "Budget review");
        assert!(parse_notes("not json").is_err());
        assert!(parse_notes(&with(good(), "title", json!("")).to_string()).is_err());
        // the message is written for the person
        assert_eq!(pechu_common::friendly(&parse_notes("not json").unwrap_err()), "Gemini sent back notes that could not be read. Try again.");
    }

    // --- tests/notes-v2.test.ts (the pure parts) ---

    fn flow() -> Value {
        json!({ "label": "Release", "kind": "root", "children": [{ "label": "Build", "kind": "topic", "at": 12.6 }] })
    }
    fn v1() -> Value {
        json!({
            "title": " Release plan ",
            "summary": "We plan the release.",
            "keyPoints": ["Build on Friday", "  ", "Test first"],
            "decisions": ["Ship Monday"],
            "actions": [{ "task": "Deploy", "owner": "Ravi" }],
            "flow": flow(),
        })
    }
    fn step(id: &str, kind: &str, next: &[(&str, Option<&str>)]) -> Value {
        let next: Vec<Value> =
            next.iter().map(|(to, label)| label.map_or(json!({ "to": to }), |l| json!({ "to": to, "label": l }))).collect();
        json!({ "id": id, "label": format!("Do {id}"), "kind": kind, "next": next })
    }

    #[test]
    fn v1_notes_come_back_as_v2() {
        let n = validate_notes(&v1()).unwrap();
        assert_eq!(n["title"], "Release plan");
        assert_eq!(n["keyPoints"], json!([{ "text": "Build on Friday" }, { "text": "Test first" }]));
        assert_eq!(n["decisions"], json!([{ "text": "Ship Monday" }]));
        assert_eq!(n["sections"], json!([]));
        assert!(n.get("process").is_none());
        assert_eq!(n["flow"]["children"][0]["at"], 13);
    }

    #[test]
    fn v2_items_keep_who_and_cleaned_at_and_a_bad_at_is_dropped() {
        let mut input = v1();
        input["keyPoints"] = json!([
            { "text": " a ", "at": 75.4 }, { "text": "b", "at": -3 }, { "text": "c", "at": "9" }, { "text": "d", "at": null }, { "text": "" },
        ]);
        input["decisions"] = json!([{ "text": "Go", "at": 0, "who": " Ravi " }, { "text": "Stop", "who": "  " }]);
        input["actions"] = json!([{ "task": "T", "at": 30.5 }]);
        let n = validate_notes(&input).unwrap();
        assert_eq!(n["keyPoints"], json!([{ "text": "a", "at": 75 }, { "text": "b" }, { "text": "c" }, { "text": "d" }]));
        assert_eq!(n["decisions"], json!([{ "text": "Go", "at": 0, "who": "Ravi" }, { "text": "Stop" }]));
        assert_eq!(n["actions"], json!([{ "task": "T", "at": 31 }]));
    }

    #[test]
    fn sections_empty_ones_dropped_at_most_10_sections_of_12_bullets() {
        let bullets = |n: usize| -> Vec<Value> { (0..n).map(|i| json!({ "text": format!("b{i}"), "at": i })).collect() };
        let mut sections = vec![
            json!({ "heading": "Empty", "bullets": [{ "text": " " }] }),
            json!({ "heading": "No list" }),
            json!({ "heading": " Big ", "bullets": bullets(20) }),
        ];
        sections.extend((0..15).map(|i| json!({ "heading": format!("S{i}"), "bullets": bullets(1) })));
        let n = validate_notes(&with(v1(), "sections", json!(sections))).unwrap();
        assert_eq!(n["sections"].as_array().unwrap().len(), 10);
        assert_eq!(n["sections"][0]["heading"], "Big");
        assert_eq!(n["sections"][0]["bullets"].as_array().unwrap().len(), 12);
        // a section without a heading is called "Details"
        let n = validate_notes(&with(v1(), "sections", json!([{ "bullets": ["x"] }]))).unwrap();
        assert_eq!(n["sections"][0]["heading"], "Details");
    }

    #[test]
    fn process_dangling_arrows_dropped_duplicate_ids_renamed_first_step_is_start() {
        let p = validate_process(&json!({ "steps": [
            step("a", "step", &[("b", None), ("ghost", None)]),
            step("b", "decision", &[("c", Some("Yes")), ("a", Some("No"))]),
            step("b", "step", &[("c", None)]),
            step("c", "end", &[("a", None)]),
        ] }))
        .unwrap();
        let steps = p["steps"].as_array().unwrap();
        let ids: Vec<&str> = steps.iter().map(|s| s["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["a", "b", "b-2", "c"]);
        assert_eq!(steps[0]["kind"], "start");
        assert_eq!(steps[0]["next"], json!([{ "to": "b" }]));
        assert_eq!(steps[1]["next"], json!([{ "to": "c", "label": "Yes" }, { "to": "a", "label": "No" }])); // cycle and labels kept
        assert_eq!(steps[3]["next"], json!([])); // end has no arrows
    }

    #[test]
    fn process_fewer_than_3_valid_steps_is_omitted_more_than_30_is_cut_unknown_kind_becomes_step() {
        assert!(validate_process(&json!({ "steps": [step("a", "step", &[]), step("b", "step", &[])] })).is_none());
        let blank = json!({ "id": "c", "label": " ", "kind": "step", "next": [] });
        assert!(validate_process(&json!({ "steps": [step("a", "step", &[]), step("b", "step", &[]), blank] })).is_none());
        assert!(validate_process(&Value::Null).is_none());
        let many: Vec<Value> = (0..40).map(|i| step(&format!("s{i}"), "step", &[])).collect();
        assert_eq!(validate_process(&json!(many)).unwrap()["steps"].as_array().unwrap().len(), 30);
        let weird = validate_process(&json!([step("a", "step", &[]), step("b", "weird", &[]), step("c", "step", &[])])).unwrap();
        assert_eq!(weird["steps"][1]["kind"], "step");
        let short = json!({ "steps": [step("a", "step", &[]), step("b", "step", &[])] });
        assert!(validate_notes(&with(v1(), "process", short)).unwrap().get("process").is_none());
    }

    #[test]
    fn parse_flow_reads_flow_and_process_only() {
        let raw = json!({ "flow": flow(), "process": { "steps": [step("a", "step", &[]), step("b", "step", &[]), step("c", "step", &[])] } });
        let out = parse_flow(&raw.to_string()).unwrap();
        assert_eq!(out["flow"]["label"], "Release");
        assert_eq!(out["process"]["steps"].as_array().unwrap().len(), 3);
        assert!(parse_flow(&json!({ "process": { "steps": [] } }).to_string()).is_err());
    }

    #[test]
    fn without_times_removes_every_at() {
        let mut v = json!({ "at": 1, "a": [{ "text": "x", "at": 2 }], "flow": { "label": "l", "at": 3, "children": [{ "at": 4 }] } });
        without_times(&mut v);
        assert!(!v.to_string().contains("\"at\""));
        assert_eq!(v["a"][0]["text"], "x");
    }

    #[test]
    fn schemas_have_the_expected_top_level_shape() {
        assert_eq!(notes_schema()["required"].as_array().unwrap().len(), 7);
        assert_eq!(flow_schema(4)["properties"]["children"]["items"]["properties"]["children"]["items"]["properties"]["children"]["items"]["properties"].get("children"), None);
        assert_eq!(flow_only_schema()["required"], json!(["flow"]));
    }
}
