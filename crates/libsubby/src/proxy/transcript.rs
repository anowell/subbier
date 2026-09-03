//! Splicing a resolved chain back into a Codex request body. That backend keeps
//! no server-side state, so `previous_response_id` is emulated: the stored
//! conversation is spliced into `input` and the id deleted before the body goes
//! upstream.

use serde_json::{Map, Value, json};

use crate::store::transcripts::Chain;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "unknown previous_response_id: subbier has no transcript for that id \
     (never served through this proxy, or expired)"
)]
pub struct UnknownPreviousResponse {
    /// Deliberately not in the `Display` message, which is client-facing.
    pub id: String,
}

impl UnknownPreviousResponse {
    /// A client mistake, not an upstream failure.
    pub const HTTP_STATUS: u16 = 400;
}

/// Replace `body["input"]` with `[chain.items…, …the items the client sent]`
/// and drop `previous_response_id`, which upstream would reject.
pub fn splice(body: &mut Map<String, Value>, chain: &Chain) {
    let sent = response_input_items(body.get("input").unwrap_or(&Value::Null));
    let mut input = Vec::with_capacity(chain.items.len() + sent.len());
    input.extend(chain.items.iter().cloned());
    input.extend(sent);
    body.insert("input".to_string(), Value::Array(input));
    body.remove("previous_response_id");
}

/// `None` for absent and `null`, which both mean "not a chained request".
pub fn previous_response_id(body: &Map<String, Value>) -> Option<String> {
    match body.get("previous_response_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(id)) => Some(id.clone()),
        Some(other) => Some(other.to_string()),
    }
}

/// Normalise a Responses API `input` field into a list of items: a bare string
/// lifts to one user message, `null` is empty, anything else is a single item.
pub fn response_input_items(input: &Value) -> Vec<Value> {
    match input {
        Value::Array(items) => items.clone(),
        Value::Null => Vec::new(),
        Value::String(text) => vec![json!({ "role": "user", "content": text })],
        other => vec![other.clone()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Provider, SubKey};

    fn body(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            other => panic!("test body must be an object, got {other}"),
        }
    }

    fn items(body: &Map<String, Value>) -> &Vec<Value> {
        body["input"].as_array().expect("input should be an array")
    }

    fn chain(items: Vec<Value>) -> Chain {
        Chain {
            items,
            sub: SubKey::new(Provider::Codex, "acct-1"),
        }
    }

    #[test]
    fn input_normalises_to_a_list_of_items() {
        for (input, expected) in [
            (
                json!([{ "type": "function_call_output", "call_id": "c1" }]),
                vec![json!({ "type": "function_call_output", "call_id": "c1" })],
            ),
            (
                json!("turn one"),
                vec![json!({ "role": "user", "content": "turn one" })],
            ),
            (json!({ "role": "user" }), vec![json!({ "role": "user" })]),
            (Value::Null, Vec::new()),
        ] {
            assert_eq!(response_input_items(&input), expected, "{input}");
        }
    }

    #[test]
    fn splice_puts_the_chain_before_this_turn_and_drops_the_id() {
        let prior = chain(vec![
            json!({ "role": "user", "content": "turn one" }),
            json!({ "type": "message", "content": "answer" }),
        ]);
        let mut request = body(json!({
            "model": "gpt-5.4",
            "previous_response_id": "resp_1",
            "input": [{ "type": "function_call_output", "call_id": "c1", "output": "ok" }],
        }));
        splice(&mut request, &prior);

        assert!(!request.contains_key("previous_response_id"));
        let forwarded = items(&request);
        assert_eq!(forwarded.len(), 3);
        assert_eq!(
            forwarded[0],
            json!({ "role": "user", "content": "turn one" })
        );
        assert_eq!(forwarded[1]["type"], "message");
        assert_eq!(forwarded[2]["type"], "function_call_output");
        assert_eq!(request["model"], "gpt-5.4", "nothing else is touched");
    }

    #[test]
    fn splice_normalises_the_chained_turns_own_input() {
        let mut request = body(json!({ "previous_response_id": "resp_1", "input": "turn two" }));
        splice(&mut request, &chain(vec![json!({ "type": "message" })]));
        assert_eq!(
            items(&request)[1],
            json!({ "role": "user", "content": "turn two" })
        );

        let mut request = body(json!({ "previous_response_id": "resp_1" }));
        splice(&mut request, &chain(vec![json!({ "role": "user" })]));
        assert_eq!(*items(&request), vec![json!({ "role": "user" })]);
    }

    #[test]
    fn a_body_without_previous_response_id_names_no_chain() {
        for value in [
            json!({ "input": "hi" }),
            json!({ "input": "hi", "previous_response_id": null }),
        ] {
            let request = body(value);
            assert_eq!(previous_response_id(&request), None);
        }
        assert_eq!(
            previous_response_id(&body(json!({ "previous_response_id": "resp_1" }))),
            Some("resp_1".to_owned())
        );
    }

    #[test]
    fn the_client_facing_rejection_does_not_echo_the_id() {
        let error = UnknownPreviousResponse {
            id: "resp_previous".to_owned(),
        };
        assert!(!error.to_string().contains("resp_previous"), "{error}");
    }
}
