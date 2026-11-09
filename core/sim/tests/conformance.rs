// SPDX-License-Identifier: Apache-2.0

//! Runs spec/testdata/scenarios/space-conformance-v1.json against the reference Space DO. The
//! TypeScript server runs the same file, so both implementations meet the same hand-written
//! expectations. The file's own conventions list describes every field read here.

use std::collections::{BTreeSet, HashMap};

use flarebox_engine::ids::IdGen;
use flarebox_proto::v1::{
    ChunkRef, Commit, Content, CreateDir, CreateFile, Delete, Error, ListVersions, Move, NodeState,
    NodeStatus, Op, PrepareUpload, Restore, RpcRequest, UpdateContent, UploadReceipt, VersionInfo,
    chunk_plan, error, op, op_result, rpc_request, rpc_response,
};
use flarebox_sim::space::{Role, Session, SpaceDo, node_states};
use serde_json::Value;
use sha2::{Digest, Sha256};

const FILE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../spec/testdata/scenarios/space-conformance-v1.json"
);

type Outcome = Result<(), String>;

fn main() {
    let text = std::fs::read_to_string(FILE).expect("read the conformance file");
    let doc: Value = serde_json::from_str(&text).expect("parse the conformance file");
    assert_eq!(doc["format"], "flarebox.space-conformance");
    assert_eq!(doc["version"], 1);
    let scenarios = doc["scenarios"].as_array().expect("scenarios");
    let mut failed = 0;
    for scenario in scenarios {
        let name = scenario["name"].as_str().unwrap_or("?");
        match Run::new(&doc, scenario).and_then(|mut run| run.steps(scenario)) {
            Ok(()) => println!("scenario {name} ... ok"),
            Err(e) => {
                failed += 1;
                println!("scenario {name} ... FAILED: {e}");
            }
        }
    }
    println!(
        "conformance: {} passed; {failed} failed",
        scenarios.len() - failed
    );
    if failed > 0 {
        std::process::exit(1);
    }
}

struct Run {
    now_ms: i64,
    mtime_ns: i64,
    spaces: HashMap<String, SpaceDo>,
    devices: HashMap<String, Session>,
    contents: HashMap<String, Content>,
    wallets: HashMap<(String, String), Vec<UploadReceipt>>,
    symbols: HashMap<String, String>,
    values: HashMap<String, String>,
    ids: IdGen,
    req_id: u64,
}

fn str_of<'a>(value: &'a Value, field: &str) -> Result<&'a str, String> {
    value[field]
        .as_str()
        .ok_or_else(|| format!("missing string field {field} in {value}"))
}

fn int_of(value: &Value, field: &str) -> Result<u64, String> {
    value[field]
        .as_u64()
        .ok_or_else(|| format!("missing integer field {field} in {value}"))
}

fn chunk_list(chunks: &Value) -> Result<Vec<ChunkRef>, String> {
    let mut out = Vec::new();
    for chunk in chunks.as_array().ok_or("chunks must be a list")? {
        let pieces: Vec<Vec<u8>> = if let Some(text) = chunk["text"].as_str() {
            vec![text.as_bytes().to_vec()]
        } else if let Some(n) = chunk["counter_le32"].as_u64() {
            (0..n as u32).map(|i| i.to_le_bytes().to_vec()).collect()
        } else {
            return Err(format!("unknown chunk {chunk}"));
        };
        for bytes in pieces {
            let size = match chunk["size"].as_u64() {
                Some(size) => size as u32,
                None => bytes.len() as u32,
            };
            out.push(ChunkRef {
                hash: Sha256::digest(&bytes).to_vec(),
                size,
                encoding: 0,
            });
        }
    }
    Ok(out)
}

fn error_name(code: i32) -> String {
    error::Code::try_from(code)
        .map(|c| c.as_str_name().trim_start_matches("CODE_").to_owned())
        .unwrap_or_else(|_| format!("{code}"))
}

impl Run {
    fn new(doc: &Value, scenario: &Value) -> Result<Self, String> {
        let now_ms = doc["now_ms"].as_i64().ok_or("now_ms")?;
        let mtime_ns: i64 = doc["mtime_ns"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .ok_or("mtime_ns must be an int64 string")?;
        let mut spaces = HashMap::new();
        let names: Vec<String> = match scenario["spaces"].as_array() {
            Some(list) => list
                .iter()
                .filter_map(|s| s.as_str().map(str::to_owned))
                .collect(),
            None => vec!["main".to_owned()],
        };
        for (i, name) in names.into_iter().enumerate() {
            spaces.insert(name, SpaceDo::new(1 + i as u64, now_ms));
        }
        let mut devices = HashMap::new();
        for (symbol, device) in scenario["devices"].as_object().ok_or("devices")? {
            let role = match device["role"].as_str().unwrap_or("owner") {
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
                    device_name: str_of(device, "name")?.to_owned(),
                    role,
                    max_format: device["max_format"].as_u64().unwrap_or(1) as u32,
                },
            );
        }
        let mut contents = HashMap::new();
        if let Some(map) = scenario["contents"].as_object() {
            for (symbol, def) in map {
                let chunks = chunk_list(&def["chunks"])?;
                let manifest: Vec<u8> = chunks
                    .iter()
                    .flat_map(|c| c.hash.iter().copied().chain(c.size.to_le_bytes()))
                    .collect();
                let size = match def["size"].as_u64() {
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
            symbols: HashMap::new(),
            values: HashMap::new(),
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

    /// The ID a symbol stands for in a request, minting one for a symbol not seen before.
    fn id(&mut self, space: &str, symbol: &str) -> Result<String, String> {
        if symbol == "root" {
            return Ok(self.space(space)?.space_id().to_owned());
        }
        if let Some(literal) = symbol.strip_prefix('=') {
            return Ok(literal.to_owned());
        }
        if let Some(value) = self.symbols.get(symbol) {
            return Ok(value.clone());
        }
        let value = self.ids.uuid_v7(self.now_ms as u64);
        self.bind(symbol, &value)?;
        Ok(value)
    }

    /// Checks a returned ID against a symbol, binding it when the symbol is new.
    fn expect_id(&mut self, space: &str, symbol: &str, value: &str) -> Outcome {
        if symbol == "root" {
            let root = self.space(space)?.space_id().to_owned();
            return if root == value {
                Ok(())
            } else {
                Err(format!("expected the root, got {value}"))
            };
        }
        self.bind(symbol, value)
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

    fn session(&self, step: &Value) -> Result<Session, String> {
        let device = str_of(step, "device")?;
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
        for (i, step) in scenario["steps"]
            .as_array()
            .ok_or("steps")?
            .iter()
            .enumerate()
        {
            self.step(step)
                .map_err(|e| format!("step {}: {e}", i + 1))?;
        }
        Ok(())
    }

    fn step(&mut self, step: &Value) -> Outcome {
        let space = step["space"].as_str().unwrap_or("main").to_owned();
        let at_ms = step["at_ms"].as_i64().unwrap_or(self.now_ms);
        if step.get("commit").is_some() {
            return self.commit(&space, at_ms, step);
        }
        if let Some(upload) = step.get("upload") {
            let session = self.session(step)?;
            let mut chunks = Vec::new();
            for symbol in upload["contents"].as_array().ok_or("upload.contents")? {
                chunks.extend(
                    self.content(symbol.as_str().ok_or("content symbol")?)?
                        .chunks,
                );
            }
            let receipts = self.upload(&space, &session, at_ms, chunks)?;
            self.wallets
                .entry((session.device_id.clone(), space))
                .or_default()
                .extend(receipts);
            return Ok(());
        }
        if let Some(prepare) = step.get("prepare_upload") {
            let session = self.session(step)?;
            let chunks = match prepare.get("contents") {
                Some(list) => {
                    let mut chunks = Vec::new();
                    for symbol in list.as_array().ok_or("prepare_upload.contents")? {
                        chunks.extend(self.content(symbol.as_str().ok_or("content")?)?.chunks);
                    }
                    chunks
                }
                None => chunk_list(&prepare["chunks"])?,
            };
            let result = self.rpc(
                &space,
                &session,
                at_ms,
                rpc_request::Call::PrepareUpload(PrepareUpload { chunks }),
            )?;
            let expect = &step["expect"];
            return match (result, expect["error"].as_str()) {
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
                    let expected: Vec<String> = expect["actions"]
                        .as_array()
                        .ok_or("expect.actions")?
                        .iter()
                        .filter_map(|a| a.as_str().map(str::to_owned))
                        .collect();
                    if actions == expected {
                        Ok(())
                    } else {
                        Err(format!("actions {actions:?}, expected {expected:?}"))
                    }
                }
                (_, Some(code)) => Err(format!("expected error {code}")),
                _ => Err("unexpected response".into()),
            };
        }
        if step.get("list_versions").is_some() {
            return self.list_versions(&space, at_ms, step);
        }
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

    fn ops(&mut self, space: &str, list: &Value) -> Result<(Vec<Op>, Vec<String>), String> {
        let mut ops = Vec::new();
        let mut contents = Vec::new();
        for entry in list.as_array().ok_or("ops must be a list")? {
            if let Some(n) = entry["repeat"].as_u64() {
                let (one, used) = self.ops(space, &Value::Array(vec![entry["op"].clone()]))?;
                for _ in 0..n {
                    ops.extend(one.iter().cloned());
                }
                contents.extend(used);
                continue;
            }
            if let Some(chain) = entry.get("create_dir_chain") {
                let prefix = str_of(chain, "prefix")?;
                let mut parent = self.id(space, str_of(chain, "parent")?)?;
                for i in 1..=int_of(chain, "count")? {
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
            let (kind, body) = entry
                .as_object()
                .and_then(|o| o.iter().next())
                .ok_or_else(|| format!("bad op {entry}"))?;
            let content = |run: &Run, contents: &mut Vec<String>| -> Result<Content, String> {
                let symbol = str_of(body, "content")?;
                contents.push(symbol.to_owned());
                run.content(symbol)
            };
            let op = match kind.as_str() {
                "create_dir" => op::Op::CreateDir(CreateDir {
                    node_id: self.id(space, str_of(body, "node")?)?,
                    parent_id: self.id(space, str_of(body, "parent")?)?,
                    name: str_of(body, "name")?.to_owned(),
                }),
                "create_file" => op::Op::CreateFile(CreateFile {
                    node_id: self.id(space, str_of(body, "node")?)?,
                    parent_id: self.id(space, str_of(body, "parent")?)?,
                    name: str_of(body, "name")?.to_owned(),
                    content: Some(content(self, &mut contents)?),
                }),
                "update_content" => op::Op::UpdateContent(UpdateContent {
                    node_id: self.id(space, str_of(body, "node")?)?,
                    content: Some(content(self, &mut contents)?),
                }),
                "move" => op::Op::Move(Move {
                    node_id: self.id(space, str_of(body, "node")?)?,
                    new_parent_id: self.id(space, str_of(body, "parent")?)?,
                    new_name: str_of(body, "name")?.to_owned(),
                }),
                "delete" => op::Op::Delete(Delete {
                    node_id: self.id(space, str_of(body, "node")?)?,
                }),
                "restore" => op::Op::Restore(Restore {
                    node_id: self.id(space, str_of(body, "node")?)?,
                    version_id: match body["version"].as_str() {
                        Some(v) => self.id(space, v)?,
                        None => String::new(),
                    },
                    target_parent_id: match body["parent"].as_str() {
                        Some(p) => self.id(space, p)?,
                        None => String::new(),
                    },
                    name: body["name"].as_str().unwrap_or_default().to_owned(),
                }),
                "empty" => {
                    ops.push(Op { op: None });
                    continue;
                }
                other => return Err(format!("unknown op {other}")),
            };
            ops.push(Op { op: Some(op) });
        }
        Ok((ops, contents))
    }

    fn commit(&mut self, space: &str, at_ms: i64, step: &Value) -> Outcome {
        let session = self.session(step)?;
        let body = &step["commit"];
        let (ops, used) = self.ops(space, &body["ops"])?;
        let commit_id = match body["commit_id"].as_str() {
            Some(symbol) => self.id(space, symbol)?,
            None => self.ids.uuid_v7(self.now_ms as u64),
        };
        let receipts = if body["upload"].as_bool() == Some(false) {
            let from = body["receipts_from"].as_str().unwrap_or(space).to_owned();
            let delta = body["receipt_size_delta"].as_u64().unwrap_or(0) as u32;
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
        let commit = Commit {
            commit_id,
            base_seq: int_of(body, "base_seq")?,
            ops,
            receipts,
        };
        let result = self.rpc(space, &session, at_ms, rpc_request::Call::Commit(commit))?;
        let expect = &step["expect"];
        match (result, expect["error"].as_str()) {
            (rpc_response::Result::Error(e), Some(code)) => {
                check_error(&e, code)?;
                if let Some(missing) = expect["missing"].as_array() {
                    let mut hashes = BTreeSet::new();
                    for symbol in missing {
                        for chunk in self.content(symbol.as_str().ok_or("missing")?)?.chunks {
                            hashes.insert(chunk.hash);
                        }
                    }
                    let got: BTreeSet<Vec<u8>> = e.missing_chunks.iter().cloned().collect();
                    if got != hashes {
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
                if let Some(seq) = expect["seq"].as_u64()
                    && result.seq != seq
                {
                    return Err(format!("seq {}, expected {seq}", result.seq));
                }
                if let Some(expected) = expect["results"].as_array() {
                    if expected.len() != result.results.len() {
                        return Err(format!(
                            "{} results, expected {}",
                            result.results.len(),
                            expected.len()
                        ));
                    }
                    for (i, (want, got)) in expected.iter().zip(&result.results).enumerate() {
                        let code = op_result::Code::try_from(got.code)
                            .map(|c| c.as_str_name().trim_start_matches("CODE_"))
                            .unwrap_or("?");
                        let want_code = str_of(want, "code")?;
                        if code != want_code {
                            return Err(format!(
                                "result {}: {code} {:?}, expected {want_code}",
                                i + 1,
                                got.reject_reason
                            ));
                        }
                        if let Some(name) = want["name"].as_str()
                            && got.name != name
                        {
                            return Err(format!(
                                "result {}: name {:?}, expected {name:?}",
                                i + 1,
                                got.name
                            ));
                        }
                        if let Some(reason) = want["reason"].as_str()
                            && got.reject_reason != reason
                        {
                            return Err(format!(
                                "result {}: reason {:?}, expected {reason}",
                                i + 1,
                                got.reject_reason
                            ));
                        }
                        if let Some(node) = want["node"].as_str() {
                            self.expect_id(space, node, &got.node_id)?;
                        }
                    }
                }
                if let Some(changed) = expect["changed"].as_array() {
                    let mut want = BTreeSet::new();
                    for symbol in changed {
                        let symbol = symbol.as_str().ok_or("changed")?;
                        want.insert(
                            self.symbols
                                .get(symbol)
                                .cloned()
                                .ok_or_else(|| format!("unbound symbol {symbol}"))?,
                        );
                    }
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
        if let Some(seq) = want["seq"].as_u64()
            && got.seq != seq
        {
            return Err(format!("version seq {}, expected {seq}", got.seq));
        }
        if let Some(symbol) = want["version"].as_str() {
            self.expect_id(space, symbol, &got.version_id)?;
        }
        if let Some(content) = want["content"].as_str() {
            self.check_content(got.content.as_ref(), content)?;
        }
        Ok(())
    }

    fn cursor(&self, value: &Value) -> Result<String, String> {
        Ok(match value {
            Value::Null => String::new(),
            Value::String(s) => match s.strip_prefix('=') {
                Some(literal) => literal.to_owned(),
                None if s.is_empty() => String::new(),
                None => return Err(format!("cursor {s:?} must be a pair or a literal")),
            },
            pair => {
                let version = str_of(pair, "version")?;
                let id = self
                    .symbols
                    .get(version)
                    .ok_or_else(|| format!("unbound symbol {version}"))?;
                format!("{}/{id}", int_of(pair, "seq")?)
            }
        })
    }

    fn list_versions(&mut self, space: &str, at_ms: i64, step: &Value) -> Outcome {
        let session = self.session(step)?;
        let body = &step["list_versions"];
        let request = ListVersions {
            node_id: self.id(space, str_of(body, "node")?)?,
            cursor: self.cursor(&body["cursor"])?,
            limit: body["limit"].as_u64().unwrap_or(0) as u32,
        };
        let result = self.rpc(
            space,
            &session,
            at_ms,
            rpc_request::Call::ListVersions(request),
        )?;
        let expect = &step["expect"];
        match (result, expect["error"].as_str()) {
            (rpc_response::Result::Error(e), Some(code)) => check_error(&e, code),
            (rpc_response::Result::Error(e), None) => {
                Err(format!("ListVersions failed: {}", error_name(e.code)))
            }
            (rpc_response::Result::Versions(list), None) => {
                if let Some(count) = expect["count"].as_u64()
                    && list.versions.len() as u64 != count
                {
                    return Err(format!(
                        "{} versions, expected {count}",
                        list.versions.len()
                    ));
                }
                if let Some(want) = expect["versions"].as_array() {
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
                if let Some(next) = expect.get("next_cursor") {
                    let want = self.cursor(next)?;
                    if list.next_cursor != want {
                        return Err(format!(
                            "next_cursor {:?}, expected {want:?}",
                            list.next_cursor
                        ));
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
        if let Some(head) = step["head"].as_u64() {
            known = true;
            let got = self.space(space)?.head_seq();
            if got != head {
                return Err(format!("head_seq {got}, expected {head}"));
            }
        }
        if let Some(nodes) = step["nodes"].as_array() {
            known = true;
            for want in nodes {
                self.check_node(space, want)?;
            }
        }
        if let Some(live) = step["live"].as_array() {
            known = true;
            let root = self.space(space)?.space_id().to_owned();
            let got: BTreeSet<String> = self
                .space(space)?
                .nodes()
                .into_iter()
                .filter(|n| n.status == NodeStatus::Live as i32 && n.node_id != root)
                .map(|n| n.node_id)
                .collect();
            let mut want = BTreeSet::new();
            for symbol in live {
                let symbol = symbol.as_str().ok_or("live")?;
                want.insert(
                    self.symbols
                        .get(symbol)
                        .cloned()
                        .ok_or_else(|| format!("unbound symbol {symbol}"))?,
                );
            }
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
            Err(format!("unknown step {step}"))
        }
    }

    fn check_node(&mut self, space: &str, want: &Value) -> Outcome {
        let symbol = str_of(want, "node")?;
        let id = self
            .symbols
            .get(symbol)
            .cloned()
            .ok_or_else(|| format!("unbound symbol {symbol}"))?;
        let got: Option<NodeState> = self.space(space)?.node(&id);
        if want["absent"].as_bool() == Some(true) {
            return match got {
                None => Ok(()),
                Some(_) => Err(format!("{symbol} exists, expected it absent")),
            };
        }
        let got = got.ok_or_else(|| format!("{symbol} does not exist"))?;
        let fail = |field: &str, got: &dyn std::fmt::Debug| {
            Err(format!(
                "{symbol}.{field} is {got:?}, expected {}",
                want[field]
            ))
        };
        if let Some(parent) = want["parent"].as_str() {
            self.expect_id(space, parent, &got.parent_id)
                .map_err(|e| format!("{symbol}.parent: {e}"))?;
        }
        if let Some(name) = want["name"].as_str()
            && got.name != name
        {
            return fail("name", &got.name);
        }
        if let Some(kind) = want["kind"].as_str() {
            let name = flarebox_proto::v1::NodeKind::try_from(got.kind)
                .map(|k| k.as_str_name().trim_start_matches("NODE_KIND_"))
                .unwrap_or("?");
            if name != kind {
                return fail("kind", &name);
            }
        }
        if let Some(status) = want["status"].as_str() {
            let name = NodeStatus::try_from(got.status)
                .map(|s| s.as_str_name().trim_start_matches("NODE_STATUS_"))
                .unwrap_or("?");
            if name != status {
                return fail("status", &name);
            }
        }
        if let Some(rev) = want["content_rev"].as_u64()
            && got.content_rev != rev
        {
            return fail("content_rev", &got.content_rev);
        }
        if let Some(rev) = want["meta_rev"].as_u64()
            && got.meta_rev != rev
        {
            return fail("meta_rev", &got.meta_rev);
        }
        match want.get("trash_group") {
            Some(Value::Null) if !got.trash_group.is_empty() => {
                return fail("trash_group", &got.trash_group);
            }
            Some(Value::String(group)) => self
                .expect_id(space, group, &got.trash_group)
                .map_err(|e| format!("{symbol}.trash_group: {e}"))?,
            _ => {}
        }
        if let Some(content) = want["content"].as_str() {
            self.check_content(
                got.current.as_ref().and_then(|v| v.content.as_ref()),
                content,
            )
            .map_err(|e| format!("{symbol}: {e}"))?;
        }
        Ok(())
    }
}

fn check_error(error: &Error, code: &str) -> Outcome {
    let got = error_name(error.code);
    if got == code {
        Ok(())
    } else {
        Err(format!("error {got} ({}), expected {code}", error.message))
    }
}
