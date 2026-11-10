// SPDX-License-Identifier: Apache-2.0

//! Runs spec/testdata/scenarios/space-conformance-v1.json against the reference Space DO. The
//! TypeScript server runs the same file, so both implementations meet the same hand-written
//! expectations. The file's own conventions list describes every field read here; a field that is
//! unknown or has the wrong type fails its scenario instead of being ignored.

use std::collections::{BTreeSet, HashMap};

use flarebox_engine::ids::IdGen;
use flarebox_proto::v1::{
    ChunkRef, Commit, Content, CreateDir, CreateFile, Delete, Error, ListVersions, Move, NodeKind,
    NodeState, NodeStatus, Op, PrepareUpload, Restore, RpcRequest, UpdateContent, UploadReceipt,
    VersionInfo, chunk_plan, error, op, op_result, rpc_request, rpc_response,
};
use flarebox_sim::space::{Role, Session, SpaceDo, node_states};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

const FILE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../spec/testdata/scenarios/space-conformance-v1.json"
);

type Outcome = Result<(), String>;

fn main() {
    let text = std::fs::read_to_string(FILE).expect("read the conformance file");
    let doc: Value = serde_json::from_str(&text).expect("parse the conformance file");
    let mut failed = 0;
    if let Err(e) = check_doc(&doc) {
        println!("conformance file ... FAILED: {e}");
        std::process::exit(1);
    }
    let scenarios = doc["scenarios"].as_array().expect("checked above");
    for scenario in scenarios {
        let name = scenario["name"].as_str().unwrap_or("?");
        let tag = if scenario.get("reference_only").is_some() {
            " (reference only)"
        } else {
            ""
        };
        match run(&doc, scenario) {
            Ok(()) => println!("scenario {name}{tag} ... ok"),
            Err(e) => {
                failed += 1;
                println!("scenario {name}{tag} ... FAILED: {e}");
            }
        }
    }
    println!(
        "conformance: {} passed; {failed} failed",
        scenarios.len() - failed
    );
    let self_tests = self_tests(&doc);
    for (name, outcome) in &self_tests {
        match outcome {
            Ok(()) => println!("self-test {name} ... ok"),
            Err(e) => {
                failed += 1;
                println!("self-test {name} ... FAILED: {e}");
            }
        }
    }
    println!("runner self-tests: {} run", self_tests.len());
    if failed > 0 {
        std::process::exit(1);
    }
}

/// The scenario whose missing-chunk list is longer than one error can carry.
const LONG_MISSING_LIST: &str = "a-long-missing-chunk-list-is-capped-and-the-resent-commit-applies";

/// Mutations of a scenario that a correct runner must reject, each with the failure it must
/// report. The first two and the total once passed.
fn self_tests(doc: &Value) -> Vec<(&'static str, Outcome)> {
    let base = &doc["scenarios"][0];
    let long = doc["scenarios"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|s| s["name"] == LONG_MISSING_LIST)
        .unwrap_or(&Value::Null);
    let mutate = |scenario: &Value, edit: &dyn Fn(&mut Value), reason: &str| -> Outcome {
        let mut scenario = scenario.clone();
        edit(&mut scenario);
        match run(doc, &scenario) {
            Ok(()) => Err("the mutated scenario passed".into()),
            Err(e) if e.contains(reason) => Ok(()),
            Err(e) => Err(format!("failed for another reason: {e}")),
        }
    };
    vec![
        (
            "a-literal-id-in-an-expectation-is-compared",
            mutate(
                base,
                &|s| s["steps"][2]["nodes"][0]["parent"] = "=not-the-root".into(),
                "step 3: f1.parent: expected not-the-root",
            ),
        ),
        (
            "an-assertion-of-the-wrong-type-fails",
            mutate(
                base,
                &|s| s["steps"][0]["expect"]["seq"] = "999".into(),
                "step 1: seq must be a non-negative integer",
            ),
        ),
        (
            "an-unknown-assertion-key-fails",
            mutate(
                base,
                &|s| {
                    let expect = s["steps"][0]["expect"].as_object_mut().unwrap();
                    let seq = expect.remove("seq").unwrap();
                    expect.insert("sqe".into(), seq);
                },
                "step 1: unknown field sqe in expect",
            ),
        ),
        (
            "a-field-for-the-other-branch-fails",
            mutate(
                base,
                &|s| s["steps"][0]["expect"]["missing"] = 123.into(),
                "step 1: unknown field missing in expect",
            ),
        ),
        (
            "a-wrong-numeric-assertion-fails",
            mutate(
                base,
                &|s| s["steps"][0]["expect"]["seq"] = 999.into(),
                "step 1: seq 1, expected 999",
            ),
        ),
        (
            "a-total-that-is-only-the-listed-count-fails",
            mutate(
                long,
                &|s| s["steps"][0]["expect"]["missing_some"]["total"] = 1000.into(),
                "does not give the total 1000 first",
            ),
        ),
        ("the-unmutated-scenario-passes", run(doc, base)),
    ]
}

fn run(doc: &Value, scenario: &Value) -> Outcome {
    Run::new(doc, scenario)?.steps(scenario)
}

fn object<'a>(value: &'a Value, what: &str) -> Result<&'a Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("{what} must be an object, got {value}"))
}

/// Fails on any key outside `allowed`, so that a misspelled assertion cannot vanish.
fn keys(value: &Value, what: &str, allowed: &[&str]) -> Outcome {
    for key in object(value, what)?.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(format!("unknown field {key} in {what}"));
        }
    }
    Ok(())
}

fn opt<'a, T>(
    value: &'a Value,
    field: &str,
    kind: &str,
    read: impl Fn(&'a Value) -> Option<T>,
) -> Result<Option<T>, String> {
    match value.get(field) {
        None => Ok(None),
        Some(v) => read(v)
            .map(Some)
            .ok_or_else(|| format!("{field} must be {kind}, got {v}")),
    }
}

fn opt_str<'a>(value: &'a Value, field: &str) -> Result<Option<&'a str>, String> {
    opt(value, field, "a string", Value::as_str)
}

fn opt_u64(value: &Value, field: &str) -> Result<Option<u64>, String> {
    opt(value, field, "a non-negative integer", Value::as_u64)
}

fn opt_i64(value: &Value, field: &str) -> Result<Option<i64>, String> {
    opt(value, field, "an integer", Value::as_i64)
}

fn opt_bool(value: &Value, field: &str) -> Result<Option<bool>, String> {
    opt(value, field, "a boolean", Value::as_bool)
}

fn opt_list<'a>(value: &'a Value, field: &str) -> Result<Option<&'a Vec<Value>>, String> {
    opt(value, field, "a list", Value::as_array)
}

fn req<T>(found: Result<Option<T>, String>, field: &str) -> Result<T, String> {
    found?.ok_or_else(|| format!("missing field {field}"))
}

fn strings<'a>(list: &'a [Value], what: &str) -> Result<Vec<&'a str>, String> {
    list.iter()
        .map(|v| {
            v.as_str()
                .ok_or_else(|| format!("{what} must list strings, got {v}"))
        })
        .collect()
}

fn check_doc(doc: &Value) -> Outcome {
    keys(
        doc,
        "the file",
        &[
            "format",
            "version",
            "spec",
            "unicode",
            "now_ms",
            "mtime_ns",
            "conventions",
            "scenarios",
        ],
    )?;
    if doc["format"] != "flarebox.space-conformance" || doc["version"] != 1 {
        return Err("unsupported format or version".into());
    }
    req(opt_list(doc, "scenarios"), "scenarios")?;
    Ok(())
}

/// A call step's expectations. With `error` only the fields of the error branch apply, without it
/// only those of the success branch, so a field for the other branch fails instead of being ignored.
fn expectation<'a>(
    step: &'a Value,
    on_error: &[&str],
    on_success: &[&str],
) -> Result<&'a Value, String> {
    let expect = step
        .get("expect")
        .ok_or("a call step needs expect, even if empty")?;
    if opt_str(expect, "error")?.is_some() {
        keys(expect, "an error expect", &[&["error"], on_error].concat())
    } else {
        keys(expect, "expect", on_success)
    }?;
    Ok(expect)
}

fn chunk_list(chunks: &[Value]) -> Result<Vec<ChunkRef>, String> {
    let mut out = Vec::new();
    for chunk in chunks {
        keys(chunk, "a chunk", &["text", "counter_le32", "size"])?;
        let pieces: Vec<Vec<u8>> = match (opt_str(chunk, "text")?, opt_u64(chunk, "counter_le32")?)
        {
            (Some(text), None) => vec![text.as_bytes().to_vec()],
            (None, Some(n)) => (0..n as u32).map(|i| i.to_le_bytes().to_vec()).collect(),
            _ => {
                return Err(format!(
                    "a chunk needs exactly one of text and counter_le32: {chunk}"
                ));
            }
        };
        let size = opt_u64(chunk, "size")?;
        for bytes in pieces {
            out.push(ChunkRef {
                hash: Sha256::digest(&bytes).to_vec(),
                size: size.map_or(bytes.len() as u32, |s| s as u32),
                encoding: 0,
            });
        }
    }
    Ok(out)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn error_name(code: i32) -> String {
    error::Code::try_from(code)
        .map(|c| c.as_str_name().trim_start_matches("CODE_").to_owned())
        .unwrap_or_else(|_| format!("{code}"))
}

fn check_error(error: &Error, code: &str) -> Outcome {
    let got = error_name(error.code);
    if got == code {
        Ok(())
    } else {
        Err(format!("error {got} ({}), expected {code}", error.message))
    }
}

struct Run {
    now_ms: i64,
    mtime_ns: i64,
    spaces: HashMap<String, SpaceDo>,
    devices: HashMap<String, Session>,
    contents: HashMap<String, Content>,
    wallets: HashMap<(String, String), Vec<UploadReceipt>>,
    /// Per device and space, the `device_seq` of the device's last commit there that was applied.
    slots: HashMap<(String, String), u64>,
    /// Per space, the highest seq a commit returned.
    top_seq: HashMap<String, u64>,
    last_missing: HashMap<(String, String), Vec<Vec<u8>>>,
    symbols: HashMap<String, String>,
    values: HashMap<String, String>,
    cursors: HashMap<String, String>,
    ids: IdGen,
    req_id: u64,
}

impl Run {
    fn new(doc: &Value, scenario: &Value) -> Result<Self, String> {
        keys(
            scenario,
            "a scenario",
            &[
                "name",
                "covers",
                "reference_only",
                "devices",
                "spaces",
                "contents",
                "steps",
            ],
        )?;
        req(opt_str(scenario, "name"), "name")?;
        opt_str(scenario, "reference_only")?;
        opt_list(scenario, "covers")?;
        let now_ms = req(opt_i64(doc, "now_ms"), "now_ms")?;
        let mtime_ns: i64 = req(opt_str(doc, "mtime_ns"), "mtime_ns")?
            .parse()
            .map_err(|_| "mtime_ns must be an int64 written as a string")?;
        let mut spaces = HashMap::new();
        let names = match opt_list(scenario, "spaces")? {
            Some(list) => strings(list, "spaces")?,
            None => vec!["main"],
        };
        for (i, name) in names.into_iter().enumerate() {
            spaces.insert(name.to_owned(), SpaceDo::new(1 + i as u64, now_ms));
        }
        let mut devices = HashMap::new();
        for (symbol, device) in object(&scenario["devices"], "devices")? {
            keys(device, "a device", &["name", "role", "max_format"])?;
            let role = match opt_str(device, "role")?.unwrap_or("owner") {
                "owner" => Role::Owner,
                "editor" => Role::Editor,
                "viewer" => Role::Viewer,
                other => return Err(format!("unknown role {other}")),
            };
            devices.insert(
                symbol.clone(),
                Session {
                    device_id: format!("device-{symbol}"),
                    user_id: "user".into(),
                    device_name: req(opt_str(device, "name"), "name")?.to_owned(),
                    role,
                    max_format: opt_u64(device, "max_format")?.unwrap_or(1) as u32,
                },
            );
        }
        let mut contents = HashMap::new();
        if let Some(map) = scenario.get("contents") {
            for (symbol, def) in object(map, "contents")? {
                keys(def, "a content", &["chunks", "size"])?;
                let chunks = chunk_list(req(opt_list(def, "chunks"), "chunks")?)?;
                let manifest: Vec<u8> = chunks
                    .iter()
                    .flat_map(|c| c.hash.iter().copied().chain(c.size.to_le_bytes()))
                    .collect();
                let size = match opt_u64(def, "size")? {
                    Some(size) => size,
                    None => chunks.iter().map(|c| u64::from(c.size)).sum(),
                };
                contents.insert(
                    symbol.clone(),
                    Content {
                        size,
                        mtime_ns,
                        executable: false,
                        chunk_count: chunks.len() as u32,
                        manifest_digest: Sha256::digest(manifest).to_vec(),
                        chunker: "fastcdc2020-1m-4m-16m".into(),
                        chunks,
                    },
                );
            }
        }
        Ok(Self {
            now_ms,
            mtime_ns,
            spaces,
            devices,
            contents,
            wallets: HashMap::new(),
            slots: HashMap::new(),
            top_seq: HashMap::new(),
            last_missing: HashMap::new(),
            symbols: HashMap::new(),
            values: HashMap::new(),
            cursors: HashMap::new(),
            ids: IdGen::new(7),
            req_id: 0,
        })
    }

    fn bind(&mut self, symbol: &str, value: &str) -> Outcome {
        if let Some(bound) = self.symbols.get(symbol) {
            return if bound == value {
                Ok(())
            } else {
                Err(format!("{symbol} is {bound}, got {value}"))
            };
        }
        if let Some(other) = self.values.get(value) {
            return Err(format!("{symbol} got {value}, which is already {other}"));
        }
        self.symbols.insert(symbol.to_owned(), value.to_owned());
        self.values.insert(value.to_owned(), symbol.to_owned());
        Ok(())
    }

    /// What `root` or a `=` literal stands for, which never binds.
    fn fixed(&self, space: &str, symbol: &str) -> Result<Option<String>, String> {
        if symbol == "root" {
            return Ok(Some(self.space(space)?.space_id().to_owned()));
        }
        Ok(symbol.strip_prefix('=').map(str::to_owned))
    }

    /// The ID a symbol stands for in a request, minting one for a symbol not seen before.
    fn id(&mut self, space: &str, symbol: &str) -> Result<String, String> {
        if let Some(value) = self.fixed(space, symbol)? {
            return Ok(value);
        }
        if let Some(value) = self.symbols.get(symbol) {
            return Ok(value.clone());
        }
        let value = self.ids.uuid_v7(self.now_ms as u64);
        self.bind(symbol, &value)?;
        Ok(value)
    }

    /// The ID a symbol in an expected set stands for; such a symbol must already be bound.
    fn known(&self, space: &str, symbol: &str) -> Result<String, String> {
        if let Some(value) = self.fixed(space, symbol)? {
            return Ok(value);
        }
        self.symbols
            .get(symbol)
            .cloned()
            .ok_or_else(|| format!("unbound symbol {symbol}"))
    }

    /// Checks a returned ID: `root` and literals compare exactly, a bound symbol must match, and an
    /// unbound one binds.
    fn expect_id(&mut self, space: &str, symbol: &str, value: &str) -> Outcome {
        match self.fixed(space, symbol)? {
            Some(fixed) if fixed == value => Ok(()),
            Some(fixed) => Err(format!("expected {fixed}, got {value}")),
            None => self.bind(symbol, value),
        }
    }

    fn known_set(
        &self,
        space: &str,
        list: &[Value],
        what: &str,
    ) -> Result<BTreeSet<String>, String> {
        strings(list, what)?
            .into_iter()
            .map(|symbol| self.known(space, symbol))
            .collect()
    }

    fn space(&self, name: &str) -> Result<&SpaceDo, String> {
        self.spaces
            .get(name)
            .ok_or_else(|| format!("unknown space {name}"))
    }

    fn content(&self, symbol: &str) -> Result<Content, String> {
        self.contents
            .get(symbol)
            .cloned()
            .ok_or_else(|| format!("unknown content {symbol}"))
    }

    fn contents_of(&self, list: &[Value]) -> Result<Vec<ChunkRef>, String> {
        let mut chunks = Vec::new();
        for symbol in strings(list, "contents")? {
            chunks.extend(self.content(symbol)?.chunks);
        }
        Ok(chunks)
    }

    fn session(&self, step: &Value) -> Result<Session, String> {
        let device = req(opt_str(step, "device"), "device")?;
        self.devices
            .get(device)
            .cloned()
            .ok_or_else(|| format!("unknown device {device}"))
    }

    fn rpc(
        &mut self,
        space: &str,
        session: &Session,
        at_ms: i64,
        call: rpc_request::Call,
    ) -> Result<rpc_response::Result, String> {
        self.req_id += 1;
        let request = RpcRequest {
            req_id: self.req_id,
            call: Some(call),
        };
        let space = self
            .spaces
            .get_mut(space)
            .ok_or_else(|| format!("unknown space {space}"))?;
        let (response, _) = space.rpc(session, at_ms, &request);
        response.result.ok_or_else(|| "empty response".to_owned())
    }

    fn steps(&mut self, scenario: &Value) -> Outcome {
        for (i, step) in req(opt_list(scenario, "steps"), "steps")?
            .iter()
            .enumerate()
        {
            self.step(step)
                .map_err(|e| format!("step {}: {e}", i + 1))?;
        }
        Ok(())
    }

    fn step(&mut self, step: &Value) -> Outcome {
        let space = opt_str(step, "space")?.unwrap_or("main").to_owned();
        let at_ms = opt_i64(step, "at_ms")?.unwrap_or(self.now_ms);
        opt_str(step, "reference_only")?;
        const CALL: &[&str] = &["device", "space", "at_ms", "reference_only", "expect"];
        let with = |extra: &'static str| [CALL, &[extra]].concat();
        if step.get("commit").is_some() {
            keys(step, "a step", &with("commit"))?;
            return self.commit(&space, at_ms, step);
        }
        if let Some(upload) = step.get("upload") {
            keys(
                step,
                "a step",
                &["device", "space", "at_ms", "reference_only", "upload"],
            )?;
            keys(upload, "upload", &["contents", "last_missing"])?;
            let session = self.session(step)?;
            let chunks = match (
                opt_list(upload, "contents")?,
                opt_bool(upload, "last_missing")?,
            ) {
                (Some(list), None) => self.contents_of(list)?,
                (None, Some(true)) => {
                    let listed = self
                        .last_missing
                        .get(&(session.device_id.clone(), space.clone()))
                        .ok_or("no MISSING_CHUNKS error was listed yet")?;
                    let known: HashMap<&[u8], &ChunkRef> = self
                        .contents
                        .values()
                        .flat_map(|content| &content.chunks)
                        .map(|chunk| (chunk.hash.as_slice(), chunk))
                        .collect();
                    listed
                        .iter()
                        .map(|hash| {
                            known
                                .get(hash.as_slice())
                                .map(|chunk| (*chunk).clone())
                                .ok_or_else(|| {
                                    format!("listed hash {} is no known chunk", hex(hash))
                                })
                        })
                        .collect::<Result<_, _>>()?
                }
                _ => {
                    return Err(
                        "upload needs exactly one of contents and last_missing: true".into(),
                    );
                }
            };
            let receipts = self.upload(&space, &session, at_ms, chunks)?;
            self.wallets
                .entry((session.device_id.clone(), space))
                .or_default()
                .extend(receipts);
            return Ok(());
        }
        if step.get("prepare_upload").is_some() {
            keys(step, "a step", &with("prepare_upload"))?;
            return self.prepare_upload(&space, at_ms, step);
        }
        if step.get("list_versions").is_some() {
            keys(step, "a step", &with("list_versions"))?;
            return self.list_versions(&space, at_ms, step);
        }
        keys(
            step,
            "a step",
            &["space", "reference_only", "head", "nodes", "live"],
        )?;
        self.check_state(&space, step)
    }

    fn upload(
        &mut self,
        space: &str,
        session: &Session,
        at_ms: i64,
        chunks: Vec<ChunkRef>,
    ) -> Result<Vec<UploadReceipt>, String> {
        let mut distinct = Vec::new();
        let mut seen = BTreeSet::new();
        for chunk in chunks {
            if seen.insert((chunk.hash.clone(), chunk.size)) {
                distinct.push(chunk);
            }
        }
        let mut receipts = Vec::new();
        for part in distinct.chunks(1000) {
            let result = self.rpc(
                space,
                session,
                at_ms,
                rpc_request::Call::PrepareUpload(PrepareUpload {
                    chunks: part.to_vec(),
                }),
            )?;
            let rpc_response::Result::PrepareUpload(plan) = result else {
                return Err(format!("PrepareUpload failed: {result:?}"));
            };
            let space = self.spaces.get_mut(space).ok_or("space")?;
            for (planned, chunk) in plan.chunks.iter().zip(part) {
                if planned.action == chunk_plan::Action::Upload as i32 {
                    receipts.push(space.put_chunk(&chunk.hash, chunk.size, at_ms));
                }
            }
        }
        Ok(receipts)
    }

    fn prepare_upload(&mut self, space: &str, at_ms: i64, step: &Value) -> Outcome {
        let session = self.session(step)?;
        let body = &step["prepare_upload"];
        keys(body, "prepare_upload", &["chunks", "contents"])?;
        let chunks = match (opt_list(body, "contents")?, opt_list(body, "chunks")?) {
            (Some(list), None) => self.contents_of(list)?,
            (None, Some(list)) => chunk_list(list)?,
            _ => return Err("prepare_upload needs exactly one of chunks and contents".into()),
        };
        let expect = expectation(step, &[], &["actions"])?;
        let result = self.rpc(
            space,
            &session,
            at_ms,
            rpc_request::Call::PrepareUpload(PrepareUpload { chunks }),
        )?;
        match (result, opt_str(expect, "error")?) {
            (rpc_response::Result::Error(e), Some(code)) => check_error(&e, code),
            (rpc_response::Result::Error(e), None) => {
                Err(format!("PrepareUpload failed: {}", error_name(e.code)))
            }
            (rpc_response::Result::PrepareUpload(plan), None) => {
                let actions: Vec<String> = plan
                    .chunks
                    .iter()
                    .map(|c| {
                        chunk_plan::Action::try_from(c.action)
                            .map(|a| a.as_str_name().trim_start_matches("ACTION_").to_owned())
                            .unwrap_or_default()
                    })
                    .collect();
                let expected = strings(req(opt_list(expect, "actions"), "actions")?, "actions")?;
                if actions == expected {
                    Ok(())
                } else {
                    Err(format!("actions {actions:?}, expected {expected:?}"))
                }
            }
            (_, Some(code)) => Err(format!("expected error {code}, got a result")),
            _ => Err("unexpected response".into()),
        }
    }

    fn ops(&mut self, space: &str, list: &[Value]) -> Result<(Vec<Op>, Vec<String>), String> {
        let mut ops = Vec::new();
        let mut contents = Vec::new();
        for entry in list {
            if entry.get("repeat").is_some() {
                keys(entry, "repeat", &["repeat", "op"])?;
                let n = req(opt_u64(entry, "repeat"), "repeat")?;
                let (one, used) = self.ops(space, std::slice::from_ref(&entry["op"]))?;
                for _ in 0..n {
                    ops.extend(one.iter().cloned());
                }
                contents.extend(used);
                continue;
            }
            let fields = object(entry, "an op")?;
            let [(kind, body)] = fields.iter().collect::<Vec<_>>()[..] else {
                return Err(format!("an op must have exactly one kind: {entry}"));
            };
            if kind == "create_dir_chain" {
                keys(body, kind, &["prefix", "parent", "count"])?;
                let prefix = req(opt_str(body, "prefix"), "prefix")?;
                let mut parent = self.id(space, req(opt_str(body, "parent"), "parent")?)?;
                for i in 1..=req(opt_u64(body, "count"), "count")? {
                    let symbol = format!("{prefix}{i}");
                    let node_id = self.id(space, &symbol)?;
                    ops.push(Op {
                        op: Some(op::Op::CreateDir(CreateDir {
                            node_id: node_id.clone(),
                            parent_id: parent,
                            name: symbol,
                        })),
                    });
                    parent = node_id;
                }
                continue;
            }
            let allowed: &[&str] = match kind.as_str() {
                "create_dir" | "move" => &["node", "parent", "name"],
                "create_file" => &["node", "parent", "name", "content"],
                "update_content" => &["node", "content"],
                "delete" => &["node"],
                "restore" => &["node", "version", "parent", "name"],
                "empty" => &[],
                other => return Err(format!("unknown op {other}")),
            };
            keys(body, kind, allowed)?;
            if kind == "empty" {
                ops.push(Op { op: None });
                continue;
            }
            let node = self.id(space, req(opt_str(body, "node"), "node")?)?;
            let parent = match opt_str(body, "parent")? {
                Some(p) => self.id(space, p)?,
                None => String::new(),
            };
            let name = opt_str(body, "name")?.unwrap_or_default().to_owned();
            let mut content = || -> Result<Option<Content>, String> {
                let Some(symbol) = opt_str(body, "content")? else {
                    return Ok(None);
                };
                contents.push(symbol.to_owned());
                self.content(symbol).map(Some)
            };
            let op = match kind.as_str() {
                "create_dir" => op::Op::CreateDir(CreateDir {
                    node_id: node,
                    parent_id: parent,
                    name,
                }),
                "create_file" => op::Op::CreateFile(CreateFile {
                    node_id: node,
                    parent_id: parent,
                    name,
                    content: Some(req(content(), "content")?),
                }),
                "update_content" => op::Op::UpdateContent(UpdateContent {
                    node_id: node,
                    content: Some(req(content(), "content")?),
                }),
                "move" => op::Op::Move(Move {
                    node_id: node,
                    new_parent_id: parent,
                    new_name: name,
                }),
                "delete" => op::Op::Delete(Delete { node_id: node }),
                _ => op::Op::Restore(Restore {
                    node_id: node,
                    version_id: match opt_str(body, "version")? {
                        Some(v) => self.id(space, v)?,
                        None => String::new(),
                    },
                    target_parent_id: parent,
                    name,
                }),
            };
            ops.push(Op { op: Some(op) });
        }
        Ok((ops, contents))
    }

    fn commit(&mut self, space: &str, at_ms: i64, step: &Value) -> Outcome {
        let session = self.session(step)?;
        let body = &step["commit"];
        keys(
            body,
            "commit",
            &[
                "commit_id",
                "base_seq",
                "history_id",
                "device_seq",
                "require_head",
                "ops",
                "upload",
                "receipts_from",
                "receipt_size_delta",
            ],
        )?;
        let (ops, used) = self.ops(space, req(opt_list(body, "ops"), "ops")?)?;
        let commit_id = match opt_str(body, "commit_id")? {
            Some(symbol) => self.id(space, symbol)?,
            None => self.ids.uuid_v7(self.now_ms as u64),
        };
        let upload = opt_bool(body, "upload")?.unwrap_or(true);
        if upload
            && (body.get("receipts_from").is_some() || body.get("receipt_size_delta").is_some())
        {
            return Err("receipts_from and receipt_size_delta apply only with upload false".into());
        }
        let receipts = if !upload {
            let from = opt_str(body, "receipts_from")?.unwrap_or(space).to_owned();
            let delta = opt_u64(body, "receipt_size_delta")?.unwrap_or(0) as u32;
            self.wallets
                .get(&(session.device_id.clone(), from))
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|r| UploadReceipt {
                    size: r.size + delta,
                    ..r
                })
                .collect()
        } else {
            let mut chunks = Vec::new();
            for symbol in &used {
                chunks.extend(self.content(symbol)?.chunks);
            }
            self.upload(space, &session, at_ms, chunks)?
        };
        let history_id = match opt_str(body, "history_id")? {
            Some(symbol) => self.id(space, symbol)?,
            None => self.space(space)?.history_id().to_owned(),
        };
        let slot = (session.device_id.clone(), space.to_owned());
        let device_seq = match opt_u64(body, "device_seq")? {
            Some(device_seq) => device_seq,
            None => self.slots.get(&slot).copied().unwrap_or(0) + 1,
        };
        let commit = Commit {
            commit_id,
            base_seq: req(opt_u64(body, "base_seq"), "base_seq")?,
            ops,
            receipts,
            history_id,
            device_seq,
            require_head: opt_bool(body, "require_head")?.unwrap_or(false),
        };
        let expect = expectation(
            step,
            &["missing", "missing_some"],
            &["seq", "results", "changed"],
        )?;
        let result = self.rpc(space, &session, at_ms, rpc_request::Call::Commit(commit))?;
        if let rpc_response::Result::Error(e) = &result
            && !e.missing_chunks.is_empty()
        {
            self.last_missing.insert(
                (session.device_id.clone(), space.to_owned()),
                e.missing_chunks.clone(),
            );
        }
        match (result, opt_str(expect, "error")?) {
            (rpc_response::Result::Error(e), Some(code)) => {
                check_error(&e, code)?;
                if let Some(some) = expect.get("missing_some") {
                    keys(some, "missing_some", &["contents", "count", "total"])?;
                    let from: BTreeSet<Vec<u8>> = self
                        .contents_of(req(opt_list(some, "contents"), "contents")?)?
                        .into_iter()
                        .map(|c| c.hash)
                        .collect();
                    let count = req(opt_u64(some, "count"), "count")?;
                    if e.missing_chunks.len() as u64 != count {
                        return Err(format!(
                            "{} missing chunks listed, expected {count}",
                            e.missing_chunks.len()
                        ));
                    }
                    if let Some(hash) = e.missing_chunks.iter().find(|h| !from.contains(*h)) {
                        return Err(format!("missing chunk {} is not in contents", hex(hash)));
                    }
                    let total = req(opt_u64(some, "total"), "total")?.to_string();
                    let first = e
                        .message
                        .split(|c: char| !c.is_ascii_digit())
                        .find(|number| !number.is_empty());
                    if first != Some(total.as_str()) {
                        return Err(format!(
                            "message {:?} does not give the total {total} first",
                            e.message
                        ));
                    }
                }
                if let Some(missing) = opt_list(expect, "missing")? {
                    let want: BTreeSet<Vec<u8>> = self
                        .contents_of(missing)?
                        .into_iter()
                        .map(|c| c.hash)
                        .collect();
                    let got: BTreeSet<Vec<u8>> = e.missing_chunks.iter().cloned().collect();
                    if got != want {
                        return Err(format!("missing_chunks differ: {} listed", got.len()));
                    }
                }
                Ok(())
            }
            (rpc_response::Result::Error(e), None) => Err(format!(
                "commit failed with {}: {}",
                error_name(e.code),
                e.message
            )),
            (rpc_response::Result::Commit(result), None) => {
                let top = self.top_seq.entry(space.to_owned()).or_default();
                if result.seq > *top {
                    *top = result.seq;
                    self.slots.insert(slot, device_seq);
                }
                if let Some(seq) = opt_u64(expect, "seq")?
                    && result.seq != seq
                {
                    return Err(format!("seq {}, expected {seq}", result.seq));
                }
                if let Some(expected) = opt_list(expect, "results")? {
                    if expected.len() != result.results.len() {
                        return Err(format!(
                            "{} results, expected {}",
                            result.results.len(),
                            expected.len()
                        ));
                    }
                    for (i, (want, got)) in expected.iter().zip(&result.results).enumerate() {
                        self.check_result(space, want, got)
                            .map_err(|e| format!("result {}: {e}", i + 1))?;
                    }
                }
                if let Some(changed) = opt_list(expect, "changed")? {
                    let want = self.known_set(space, changed, "changed")?;
                    let got: BTreeSet<String> = node_states(&result.changes)
                        .map(|n| n.node_id.clone())
                        .collect();
                    if got != want {
                        return Err(format!(
                            "changed {:?}, expected {:?}",
                            self.names(&got),
                            self.names(&want)
                        ));
                    }
                }
                Ok(())
            }
            (_, Some(code)) => Err(format!("expected error {code}, got a result")),
            _ => Err("unexpected response".into()),
        }
    }

    fn check_result(
        &mut self,
        space: &str,
        want: &Value,
        got: &flarebox_proto::v1::OpResult,
    ) -> Outcome {
        keys(want, "a result", &["code", "node", "name", "reason"])?;
        let code = op_result::Code::try_from(got.code)
            .map(|c| c.as_str_name().trim_start_matches("CODE_"))
            .unwrap_or("?");
        let want_code = req(opt_str(want, "code"), "code")?;
        if code != want_code {
            return Err(format!(
                "{code} {:?}, expected {want_code}",
                got.reject_reason
            ));
        }
        if let Some(name) = opt_str(want, "name")?
            && got.name != name
        {
            return Err(format!("name {:?}, expected {name:?}", got.name));
        }
        if let Some(reason) = opt_str(want, "reason")?
            && got.reject_reason != reason
        {
            return Err(format!("reason {:?}, expected {reason}", got.reject_reason));
        }
        if let Some(node) = opt_str(want, "node")? {
            self.expect_id(space, node, &got.node_id)?;
        }
        Ok(())
    }

    fn names(&self, ids: &BTreeSet<String>) -> Vec<String> {
        ids.iter()
            .map(|id| self.values.get(id).cloned().unwrap_or_else(|| id.clone()))
            .collect()
    }

    fn check_content(&self, got: Option<&Content>, symbol: &str) -> Outcome {
        let want = self.content(symbol)?;
        let got = got.ok_or_else(|| format!("no content, expected {symbol}"))?;
        if got.size == want.size
            && got.manifest_digest == want.manifest_digest
            && got.executable == want.executable
            && got.mtime_ns == self.mtime_ns
        {
            Ok(())
        } else {
            Err(format!("content differs from {symbol}"))
        }
    }

    fn check_version(&mut self, space: &str, got: &VersionInfo, want: &Value) -> Outcome {
        keys(want, "a version", &["version", "seq", "content"])?;
        if let Some(seq) = opt_u64(want, "seq")?
            && got.seq != seq
        {
            return Err(format!("version seq {}, expected {seq}", got.seq));
        }
        if let Some(symbol) = opt_str(want, "version")? {
            self.expect_id(space, symbol, &got.version_id)?;
        }
        if let Some(content) = opt_str(want, "content")? {
            self.check_content(got.content.as_ref(), content)?;
        }
        Ok(())
    }

    fn list_versions(&mut self, space: &str, at_ms: i64, step: &Value) -> Outcome {
        let session = self.session(step)?;
        let body = &step["list_versions"];
        keys(body, "list_versions", &["node", "cursor", "limit"])?;
        let cursor = match body.get("cursor") {
            None => String::new(),
            Some(Value::String(s)) => s
                .strip_prefix('=')
                .ok_or_else(|| format!("a cursor string must be a = literal, got {s:?}"))?
                .to_owned(),
            Some(pair) => {
                keys(pair, "cursor", &["returned"])?;
                let name = req(opt_str(pair, "returned"), "returned")?;
                self.cursors
                    .get(name)
                    .cloned()
                    .ok_or_else(|| format!("no returned cursor {name}"))?
            }
        };
        let request = ListVersions {
            node_id: self.id(space, req(opt_str(body, "node"), "node")?)?,
            cursor,
            limit: opt_u64(body, "limit")?.unwrap_or(0) as u32,
        };
        let expect = expectation(step, &[], &["count", "versions", "next_cursor"])?;
        let result = self.rpc(
            space,
            &session,
            at_ms,
            rpc_request::Call::ListVersions(request),
        )?;
        match (result, opt_str(expect, "error")?) {
            (rpc_response::Result::Error(e), Some(code)) => check_error(&e, code),
            (rpc_response::Result::Error(e), None) => {
                Err(format!("ListVersions failed: {}", error_name(e.code)))
            }
            (rpc_response::Result::Versions(list), None) => {
                if let Some(count) = opt_u64(expect, "count")?
                    && list.versions.len() as u64 != count
                {
                    return Err(format!(
                        "{} versions, expected {count}",
                        list.versions.len()
                    ));
                }
                if let Some(want) = opt_list(expect, "versions")? {
                    if want.len() != list.versions.len() {
                        return Err(format!(
                            "{} versions, expected {}",
                            list.versions.len(),
                            want.len()
                        ));
                    }
                    for (got, want) in list.versions.iter().zip(want) {
                        self.check_version(space, got, want)?;
                    }
                }
                match expect.get("next_cursor") {
                    None => {}
                    Some(Value::String(s)) if s.is_empty() => {
                        if !list.next_cursor.is_empty() {
                            return Err(format!(
                                "next_cursor {:?}, expected the last page",
                                list.next_cursor
                            ));
                        }
                    }
                    Some(pair) => {
                        keys(pair, "next_cursor", &["bind"])?;
                        let name = req(opt_str(pair, "bind"), "bind")?;
                        if list.next_cursor.is_empty() {
                            return Err("next_cursor is empty, expected another page".into());
                        }
                        if self.cursors.contains_key(name) {
                            return Err(format!("cursor {name} is bound twice"));
                        }
                        self.cursors.insert(name.to_owned(), list.next_cursor);
                    }
                }
                Ok(())
            }
            (_, Some(code)) => Err(format!("expected error {code}, got a result")),
            _ => Err("unexpected response".into()),
        }
    }

    fn check_state(&mut self, space: &str, step: &Value) -> Outcome {
        let mut known = false;
        if let Some(head) = opt_u64(step, "head")? {
            known = true;
            let got = self.space(space)?.head_seq();
            if got != head {
                return Err(format!("head_seq {got}, expected {head}"));
            }
        }
        if let Some(nodes) = opt_list(step, "nodes")? {
            known = true;
            for want in nodes {
                self.check_node(space, want)?;
            }
        }
        if let Some(live) = opt_list(step, "live")? {
            known = true;
            let root = self.space(space)?.space_id().to_owned();
            let got: BTreeSet<String> = self
                .space(space)?
                .nodes()
                .into_iter()
                .filter(|n| n.status == NodeStatus::Live as i32 && n.node_id != root)
                .map(|n| n.node_id)
                .collect();
            let want = self.known_set(space, live, "live")?;
            if got != want {
                return Err(format!(
                    "live nodes {:?}, expected {:?}",
                    self.names(&got),
                    self.names(&want)
                ));
            }
        }
        if known {
            Ok(())
        } else {
            Err(format!("a step does nothing: {step}"))
        }
    }

    fn check_node(&mut self, space: &str, want: &Value) -> Outcome {
        keys(
            want,
            "a node",
            &[
                "node",
                "absent",
                "parent",
                "name",
                "kind",
                "status",
                "content_rev",
                "meta_rev",
                "trash_group",
                "content",
            ],
        )?;
        let symbol = req(opt_str(want, "node"), "node")?;
        let id = self.known(space, symbol)?;
        let got: Option<NodeState> = self.space(space)?.node(&id);
        if let Some(absent) = opt_bool(want, "absent")? {
            if !absent {
                return Err(format!("{symbol}: absent may only be true"));
            }
            keys(want, "an absent node", &["node", "absent"])?;
            return match got {
                None => Ok(()),
                Some(_) => Err(format!("{symbol} exists, expected it absent")),
            };
        }
        let got = got.ok_or_else(|| format!("{symbol} does not exist"))?;
        let differs = |field: &str, got: &dyn std::fmt::Debug| {
            format!("{symbol}.{field} is {got:?}, expected {}", want[field])
        };
        if let Some(parent) = opt_str(want, "parent")? {
            self.expect_id(space, parent, &got.parent_id)
                .map_err(|e| format!("{symbol}.parent: {e}"))?;
        }
        if let Some(name) = opt_str(want, "name")?
            && got.name != name
        {
            return Err(differs("name", &got.name));
        }
        if let Some(kind) = opt_str(want, "kind")? {
            let name = NodeKind::try_from(got.kind)
                .map(|k| k.as_str_name().trim_start_matches("NODE_KIND_"))
                .unwrap_or("?");
            if name != kind {
                return Err(differs("kind", &name));
            }
        }
        if let Some(status) = opt_str(want, "status")? {
            let name = NodeStatus::try_from(got.status)
                .map(|s| s.as_str_name().trim_start_matches("NODE_STATUS_"))
                .unwrap_or("?");
            if name != status {
                return Err(differs("status", &name));
            }
        }
        if let Some(rev) = opt_u64(want, "content_rev")?
            && got.content_rev != rev
        {
            return Err(differs("content_rev", &got.content_rev));
        }
        if let Some(rev) = opt_u64(want, "meta_rev")?
            && got.meta_rev != rev
        {
            return Err(differs("meta_rev", &got.meta_rev));
        }
        match want.get("trash_group") {
            None => {}
            Some(Value::Null) => {
                if !got.trash_group.is_empty() {
                    return Err(differs("trash_group", &got.trash_group));
                }
            }
            Some(Value::String(group)) => self
                .expect_id(space, group, &got.trash_group)
                .map_err(|e| format!("{symbol}.trash_group: {e}"))?,
            Some(other) => {
                return Err(format!("trash_group must be a string or null, got {other}"));
            }
        }
        if let Some(content) = opt_str(want, "content")? {
            self.check_content(
                got.current.as_ref().and_then(|v| v.content.as_ref()),
                content,
            )
            .map_err(|e| format!("{symbol}: {e}"))?;
        }
        Ok(())
    }
}
