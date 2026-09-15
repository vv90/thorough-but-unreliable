//! Pure conversion to/from the non-streaming chat-completions wire format.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::InferenceError;
use crate::harness::{
    presentation::command_report_view,
    types::{AssistantResponse, Message, Tool, ToolCall},
};
use crate::target::SessionState;

// Serde structs can otherwise accept JSON arrays positionally. Wire objects
// and tool argument objects must actually be maps, including nested values.
#[derive(Serialize)]
#[serde(transparent)]
struct JsonObject<T>(T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for JsonObject<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ObjectVisitor<T>(std::marker::PhantomData<T>);
        impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for ObjectVisitor<T> {
            type Value = JsonObject<T>;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> Result<Self::Value, A::Error> {
                T::deserialize(serde::de::value::MapAccessDeserializer::new(map)).map(JsonObject)
            }
        }
        deserializer.deserialize_map(ObjectVisitor(std::marker::PhantomData))
    }
}

const TOOL_DEFINITIONS: &str = r#"[
  {"type":"function","function":{
    "name":"execute_target_command",
    "description":"Execute a shell command inside the designated target environment.",
    "parameters":{"type":"object","properties":{"command":{"type":"string"}},"required":["command"],"additionalProperties":false}
  }},
  {"type":"function","function":{
    "name":"submit",
    "description":"Finish the sample by submitting your answer.",
    "parameters":{"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false}
  }}
]"#;

#[derive(Serialize)]
struct Request {
    model: String,
    messages: Vec<WireMessage>,
    tools: serde_json::Value,
    tool_choice: &'static str,
    stream: bool,
    n: u32,
    max_tokens: u32,
}

#[derive(Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
enum WireMessage {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<WireCall>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

#[derive(Serialize, Deserialize)]
struct WireCall {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: JsonObject<WireFunction>,
}

#[derive(Serialize, Deserialize)]
struct WireFunction {
    name: String,
    arguments: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandArgs {
    command: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SubmitArgs {
    answer: String,
}

pub fn encode_request(
    model: &str,
    max_tokens: u32,
    history: &[Message],
) -> Result<Vec<u8>, InferenceError> {
    if model.trim().is_empty() || max_tokens == 0 {
        return Err(InferenceError::Configuration(
            "model and positive max_tokens are required".into(),
        ));
    }
    if history.is_empty() {
        return Err(InferenceError::InvalidHistory("conversation is empty"));
    }
    let messages = history
        .iter()
        .map(encode_message)
        .collect::<Result<_, _>>()?;
    Ok(serde_json::to_vec(&Request {
        model: model.into(),
        messages,
        tools: serde_json::from_str(TOOL_DEFINITIONS)?,
        tool_choice: "auto",
        stream: false,
        n: 1,
        max_tokens,
    })?)
}

fn encode_message(message: &Message) -> Result<WireMessage, InferenceError> {
    Ok(match message {
        Message::System(content) => WireMessage::System {
            content: content.clone(),
        },
        Message::User(content) => WireMessage::User {
            content: content.clone(),
        },
        Message::Assistant(response) => WireMessage::Assistant {
            content: response.text.clone(),
            tool_calls: response
                .tool_calls
                .iter()
                .map(encode_call)
                .collect::<Result<_, _>>()?,
        },
        Message::Tool { call_id, result } => {
            let result = result.as_ref().map_err(|_| {
                InferenceError::InvalidHistory("cannot resume after a command-client failure")
            })?;
            if result.session_state() == SessionState::Unusable {
                return Err(InferenceError::InvalidHistory(
                    "cannot resume an unusable target session",
                ));
            }
            WireMessage::Tool {
                tool_call_id: call_id.as_str().to_owned(),
                content: serde_json::to_string(&command_report_view(result))?,
            }
        }
    })
}

fn encode_call(call: &ToolCall) -> Result<WireCall, InferenceError> {
    let (name, arguments) = match &call.tool {
        Tool::ExecuteTargetCommand { command } => (
            "execute_target_command",
            serde_json::to_string(&CommandArgs {
                command: command.clone(),
            })?,
        ),
        Tool::Submit { answer } => (
            "submit",
            serde_json::to_string(&SubmitArgs {
                answer: answer.clone(),
            })?,
        ),
    };
    Ok(WireCall {
        id: call.id.clone(),
        kind: "function".into(),
        function: JsonObject(WireFunction {
            name: name.into(),
            arguments,
        }),
    })
}

// Ordinary provider metadata (usage, model, timestamps, etc.) is allowed.
// Tool argument objects, in contrast, have a strict schema.
#[derive(Deserialize)]
struct Response {
    choices: Vec<JsonObject<Choice>>,
}

#[derive(Deserialize)]
struct Choice {
    index: u32,
    finish_reason: String,
    message: JsonObject<AssistantMessage>,
}

#[derive(Deserialize)]
struct AssistantMessage {
    role: String,
    content: Option<String>,
    tool_calls: Option<Vec<JsonObject<WireCall>>>,
}

pub fn decode_response(bytes: &[u8]) -> Result<AssistantResponse, InferenceError> {
    let JsonObject(response): JsonObject<Response> = serde_json::from_slice(bytes)?;
    let mut choices = response.choices.into_iter();
    let JsonObject(choice) = choices
        .next()
        .ok_or(InferenceError::InvalidResponse("expected one choice"))?;
    if choices.next().is_some() || choice.index != 0 {
        return Err(InferenceError::InvalidResponse(
            "expected one choice at index zero",
        ));
    }
    match choice.finish_reason.as_str() {
        "length" => return Err(InferenceError::TruncatedResponse),
        "stop" | "tool_calls" => {}
        _ => return Err(InferenceError::InvalidResponse("unsupported finish reason")),
    }
    let JsonObject(message) = choice.message;
    if message.role != "assistant" {
        return Err(InferenceError::InvalidResponse("expected assistant role"));
    }
    let mut ids = BTreeSet::new();
    let mut tool_calls = Vec::new();
    for JsonObject(call) in message.tool_calls.into_iter().flatten() {
        if call.kind != "function" {
            return Err(InferenceError::InvalidResponse(
                "expected function tool type",
            ));
        }
        if call.id.is_empty() || !ids.insert(call.id.clone()) {
            return Err(InferenceError::InvalidResponse(
                "tool IDs must be nonempty and unique",
            ));
        }
        let JsonObject(function) = call.function;
        let tool = match function.name.as_str() {
            "execute_target_command" => {
                let JsonObject(args): JsonObject<CommandArgs> =
                    serde_json::from_str(&function.arguments)?;
                Tool::ExecuteTargetCommand {
                    command: args.command,
                }
            }
            "submit" => {
                let JsonObject(args): JsonObject<SubmitArgs> =
                    serde_json::from_str(&function.arguments)?;
                Tool::Submit {
                    answer: args.answer,
                }
            }
            _ => return Err(InferenceError::UnsupportedTool(function.name)),
        };
        tool_calls.push(ToolCall { id: call.id, tool });
    }
    if choice.finish_reason == "tool_calls" && tool_calls.is_empty() {
        return Err(InferenceError::InvalidResponse(
            "tool_calls finish reason without calls",
        ));
    }
    Ok(AssistantResponse {
        text: message.content,
        tool_calls,
    })
}
