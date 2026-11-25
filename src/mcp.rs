//! The lints as an MCP server (modelcontextprotocol.io), so an agent can run
//! them as a tool while it works: JSON-RPC 2.0 on stdin and stdout, one
//! message per line, with one tool, `check`. It answers requests in order and
//! notifications not at all. A `check` that finds errors has done its job and
//! says so in its result; one that cannot run is a tool error, as is a call
//! with arguments it does not take, so the agent can correct them.

use std::io::{BufRead, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::configuration::Configuration;
use crate::workspace::Workspace;
use crate::{Error, TOOL};

/// The protocol's versions it speaks, newest first. A client that asks for
/// another gets the newest, and decides whether it speaks that.
const PROTOCOL_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const PARSE_ERROR: i64 = -32700;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

const DESCRIPTION: &str = "\
Checks a Rust workspace's structure against deslop-please's architectural lints \
(deep modules, narrow interfaces): module-cycle, tangle, field-leak, \
shallow-module, wide-directory and long-file. Run it after changing which \
modules, types or functions use each other, and before committing. \
Fix each error from first principles: which layer of abstraction does each \
module belong at, and which narrow interface can hide the fields or functions \
it names? Fix related errors together, and never only move code so an error \
disappears. Errors listed in `.deslop-please.allow` at the workspace's root \
are known debt and are not reported; only the user adds to that file.";

/// Answers each message read from `input` on `output`, until `input` ends.
/// `directory` is where a `check` looks for the workspace unless its call
/// names another.
pub fn serve(directory: &Path, input: impl BufRead, mut output: impl Write) -> Result<(), Error> {
    for line in input.lines() {
        let line = line.map_err(|e| Error::new(format!("cannot read stdin: {e}")))?;
        if line.trim().is_empty() {
            continue;
        }
        let Some(reply) = answer(directory, &line) else {
            continue;
        };
        writeln!(output, "{reply}")
            .and_then(|()| output.flush())
            .map_err(|e| Error::new(format!("cannot write stdout: {e}")))?;
    }
    Ok(())
}

/// The reply to one message: none to a notification, or to a message that
/// is not a request.
fn answer(directory: &Path, line: &str) -> Option<Value> {
    let message: Value = match serde_json::from_str(line) {
        Ok(message) => message,
        Err(e) => return Some(failure(Value::Null, PARSE_ERROR, format!("not JSON: {e}"))),
    };
    let id = message.get("id")?.clone();
    let method = message.get("method")?.as_str()?;
    let params = message.get("params").unwrap_or(&Value::Null);
    let result = match method {
        "initialize" => Ok(initialize(params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": [tool()] })),
        "tools/call" => call(directory, params),
        _ => Err((METHOD_NOT_FOUND, format!("no method `{method}`"))),
    };
    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => failure(id, code, message),
    })
}

fn failure(id: Value, code: i64, message: String) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn initialize(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let version = PROTOCOL_VERSIONS
        .into_iter()
        .find(|&version| Some(version) == asked)
        .unwrap_or(PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": TOOL, "version": env!("CARGO_PKG_VERSION") },
        "instructions": "Run the `check` tool on Rust workspaces after structural \
            changes and before committing, and fix what it reports from first principles.",
    })
}

fn tool() -> Value {
    json!({
        "name": "check",
        "title": "Check the workspace's structure",
        "description": DESCRIPTION,
        "inputSchema": {
            "type": "object",
            "properties": {
                "directory": {
                    "type": "string",
                    "description": "A directory inside the Cargo workspace to check. \
                        Default: the project the server was started for."
                },
                "scope": {
                    "type": "string",
                    "description": "Check only this crate or module, as `my-crate` \
                        or `my_crate::net`."
                },
                "target": {
                    "type": "string",
                    "description": "Check for this target triple, not the host's."
                },
                "features": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Features to activate, as `cargo check --features`."
                },
                "all_features": {
                    "type": "boolean",
                    "description": "Activate all features of every member."
                },
                "no_default_features": {
                    "type": "boolean",
                    "description": "Do not activate the `default` feature."
                }
            },
            "additionalProperties": false
        },
        "annotations": { "readOnlyHint": true, "idempotentHint": true, "openWorldHint": false }
    })
}

fn call(directory: &Path, params: &Value) -> Result<Value, (i64, String)> {
    let name = params.get("name").and_then(Value::as_str);
    if name != Some("check") {
        return Err((INVALID_PARAMS, format!("no tool {}", json!(name))));
    }
    let empty = Map::new();
    let arguments = match params.get("arguments") {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(arguments)) => arguments,
        Some(_) => return Ok(tool_error("the arguments must be an object".to_owned())),
    };
    let checked = Check::parse(directory, arguments).and_then(|check| {
        // A bug in a lint fails this call, not the server.
        panic::catch_unwind(AssertUnwindSafe(|| check.run()))
            .unwrap_or_else(|_| Err(format!("{TOOL} panicked; see its stderr")))
    });
    Ok(checked.unwrap_or_else(tool_error))
}

fn tool_error(message: String) -> Value {
    json!({ "content": [{ "type": "text", "text": message }], "isError": true })
}

/// A `check` call, read as the command line reads its flags.
struct Check {
    directory: PathBuf,
    scope: Option<String>,
    configuration: Configuration,
}

impl Check {
    const ARGUMENTS: [&str; 6] = [
        "directory",
        "scope",
        "target",
        "features",
        "all_features",
        "no_default_features",
    ];

    fn parse(directory: &Path, arguments: &Map<String, Value>) -> Result<Check, String> {
        if let Some(unknown) = arguments
            .keys()
            .find(|name| !Check::ARGUMENTS.contains(&name.as_str()))
        {
            return Err(format!(
                "`check` takes no argument `{unknown}`; it takes {}",
                Check::ARGUMENTS.join(", ")
            ));
        }
        let text = |name| argument(arguments, name, Value::as_str, "a string");
        let flag = |name| argument(arguments, name, Value::as_bool, "true or false");
        let mut configuration = Configuration::default();
        if let Some(triple) = text("target")? {
            configuration.target(triple)?;
        }
        // A list, or one string as `--features` takes it.
        if let Some(features) = arguments.get("features").filter(|value| !value.is_null()) {
            let lists = match features {
                Value::String(list) => Some(vec![list.as_str()]),
                Value::Array(names) => names.iter().map(Value::as_str).collect::<Option<_>>(),
                _ => None,
            }
            .ok_or("`features` must be a list of strings")?;
            lists
                .into_iter()
                .for_each(|list| configuration.features(list));
        }
        if flag("all_features")? == Some(true) {
            configuration.all_features();
        }
        if flag("no_default_features")? == Some(true) {
            configuration.no_default_features();
        }
        Ok(Check {
            directory: text("directory")?
                .map_or_else(|| directory.to_owned(), |named| directory.join(named)),
            scope: text("scope")?.map(str::to_owned),
            configuration,
        })
    }

    /// The tool's result: the report as the command line prints it, without
    /// color, and the same as structured content.
    fn run(&self) -> Result<Value, String> {
        let error = |error: Error| error.to_string();
        // A client that started the server without expanding its directory,
        // or in another one, leaves the agent to name the workspace.
        let workspace = Workspace::discover_in(&self.directory).map_err(|e| {
            format!(
                "{e}\nin {}: call `check` again with `directory`, the absolute path of a \
                 directory inside the Cargo workspace to check",
                self.directory.display()
            )
        })?;
        let all = crate::diagnose(&workspace, self.scope.as_deref(), &self.configuration)
            .map_err(error)?;
        let allow = workspace.root.join(format!(".{TOOL}.allow"));
        let (reported, fixed_allowed, allow) = if allow.is_file() {
            let (reported, fixed) = crate::unallowed(all, &allow).map_err(error)?;
            (reported, fixed, Some(allow.display().to_string()))
        } else {
            (all, 0, None)
        };
        let report = crate::format::render(&reported, fixed_allowed, allow.as_deref(), false);
        Ok(json!({
            "content": [{ "type": "text", "text": report }],
            "structuredContent": {
                "ok": reported.is_empty(),
                "errors": crate::format::json(&reported),
                "fixed_allowed": fixed_allowed,
                "allow_file": allow,
            },
            "isError": false,
        }))
    }
}

/// `arguments`' `name` read by `read`, if it is there and not null.
fn argument<'a, T>(
    arguments: &'a Map<String, Value>,
    name: &str,
    read: impl Fn(&'a Value) -> Option<T>,
    kind: &str,
) -> Result<Option<T>, String> {
    match arguments.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => read(value)
            .map(Some)
            .ok_or_else(|| format!("`{name}` must be {kind}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The replies to `messages`, sent one per line.
    fn replies(messages: &[Value]) -> Vec<Value> {
        let input: String = messages
            .iter()
            .map(|message| format!("{message}\n"))
            .collect();
        let mut output = Vec::new();
        serve(Path::new("."), input.as_bytes(), &mut output).unwrap();
        String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn request(id: u64, method: &str, params: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }

    fn call_check(arguments: Value) -> Value {
        let reply = replies(&[request(
            1,
            "tools/call",
            json!({ "name": "check", "arguments": arguments }),
        )]);
        reply[0]["result"].clone()
    }

    #[test]
    fn initialize_agrees_on_a_version_it_speaks_and_else_offers_its_newest() {
        let reply = replies(&[
            request(1, "initialize", json!({ "protocolVersion": "2025-06-18" })),
            request(2, "initialize", json!({ "protocolVersion": "2099-01-01" })),
        ]);
        assert_eq!(reply[0]["id"], 1);
        assert_eq!(reply[0]["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(reply[0]["result"]["serverInfo"]["name"], TOOL);
        assert!(reply[0]["result"]["capabilities"]["tools"].is_object());
        assert_eq!(reply[1]["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
    }

    #[test]
    fn notifications_and_responses_get_no_reply() {
        let reply = replies(&[
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
            json!({ "jsonrpc": "2.0", "id": 7, "result": {} }),
            request(1, "ping", json!({})),
        ]);
        assert_eq!(reply, [json!({ "jsonrpc": "2.0", "id": 1, "result": {} })]);
    }

    #[test]
    fn tools_list_shows_check_and_the_arguments_it_takes() {
        let reply = replies(&[request(1, "tools/list", json!({}))]);
        let tools = reply[0]["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "check");
        let properties = tools[0]["inputSchema"]["properties"].as_object().unwrap();
        let mut names: Vec<&str> = properties.keys().map(String::as_str).collect();
        names.sort_unstable();
        let mut expected = Check::ARGUMENTS;
        expected.sort_unstable();
        assert_eq!(names, expected);
    }

    #[test]
    fn protocol_errors_answer_with_their_codes() {
        let mut output = Vec::new();
        serve(Path::new("."), "{not json\n".as_bytes(), &mut output).unwrap();
        let parse: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(parse["id"], Value::Null);
        assert_eq!(parse["error"]["code"], PARSE_ERROR);

        let reply = replies(&[
            request(1, "resources/list", json!({})),
            request(2, "tools/call", json!({ "name": "fix" })),
        ]);
        assert_eq!(reply[0]["id"], 1);
        assert_eq!(reply[0]["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(reply[1]["id"], 2);
        assert_eq!(reply[1]["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn arguments_it_does_not_take_are_a_tool_error_before_anything_runs() {
        for (arguments, says) in [
            (json!({ "feature": ["a"] }), "no argument `feature`"),
            (
                json!({ "features": 3 }),
                "`features` must be a list of strings",
            ),
            (
                json!({ "features": ["a", 3] }),
                "`features` must be a list of strings",
            ),
            (
                json!({ "all_features": "yes" }),
                "`all_features` must be true or false",
            ),
            (json!({ "directory": 1 }), "`directory` must be a string"),
            (json!({ "target": "" }), "target"),
            (json!(["check"]), "must be an object"),
        ] {
            let result = call_check(arguments.clone());
            assert_eq!(result["isError"], true, "{arguments}");
            let text = result["content"][0]["text"].as_str().unwrap();
            assert!(text.contains(says), "{arguments}: {text}");
        }
    }

    #[test]
    fn arguments_choose_the_configuration_as_the_flags_do() {
        let arguments = json!({
            "directory": "sub",
            "scope": "my-crate",
            "target": "aarch64-apple-darwin",
            "features": ["a,b", "c"],
            "no_default_features": true,
            "all_features": null,
        });
        let check = Check::parse(Path::new("/w"), arguments.as_object().unwrap()).unwrap();
        assert_eq!(check.directory, Path::new("/w/sub"));
        assert_eq!(check.scope.as_deref(), Some("my-crate"));
        assert_eq!(
            check.configuration.to_string(),
            "--target aarch64-apple-darwin --features a,b,c --no-default-features"
        );
        let default = Check::parse(Path::new("/w"), &Map::new()).unwrap();
        assert_eq!(default.directory, Path::new("/w"));
        assert!(default.configuration.is_default());
    }
}
