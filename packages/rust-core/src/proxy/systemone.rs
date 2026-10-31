//! System One dialects on `/v1/systemone`: `TypeSafe` names images in a keyed `assets` map that the
//! state or a question references; Ollama takes one `images` list shared by every question and
//! reads no `assets` at all, so a forwarded `assets` map would be answered as if no image existed.

use axum::body::Bytes;
use serde_json::{Map, Value};

const FORMATS: [&str; 3] = ["png", "jpg", "jpeg"];

/// The body with `TypeSafe` `assets` moved into Ollama `images`, `None` when it has no `assets`.
/// The move keeps the meaning only when every image applies to every question: a request with one
/// question, or images referenced from the state alone. An image tied to one question of several
/// cannot be expressed in Ollama's shared list, so it is refused instead of sent to all of them.
/// # Errors
/// Returns the refusal message for a request whose assets cannot be translated faithfully.
pub(super) fn assets_as_images(body: &[u8]) -> Result<Option<Bytes>, String> {
    let Ok(Value::Object(mut request)) = serde_json::from_slice::<Value>(body) else {
        return Ok(None);
    };
    let Some(assets) = request.remove("assets") else {
        return Ok(None);
    };
    let Value::Object(assets) = assets else {
        return Err("assets must be an object of named images".into());
    };
    if request.contains_key("images") {
        return Err("send images either as assets or as images, not both".into());
    }
    let questions = request.get("questions").and_then(Value::as_object);
    let shared_by_all = questions.is_some_and(|questions| questions.len() == 1);
    let mut images = Vec::with_capacity(assets.len());
    for (name, asset) in &assets {
        if !shared_by_all {
            let per_question = questions
                .into_iter()
                .flat_map(Map::values)
                .any(|question| mentions(question, name));
            if per_question
                || !request
                    .get("state")
                    .is_some_and(|state| mentions(state, name))
            {
                return Err(format!(
                    "asset {name:?} belongs to one question, but Ollama shares images across all \
                     questions; send that question in its own request"
                ));
            }
        }
        images.push(image_data(name, asset)?);
    }
    request.insert("images".into(), Value::Array(images));
    serde_json::to_vec(&request)
        .map(|bytes| Some(Bytes::from(bytes)))
        .map_err(|error| format!("could not encode the translated request: {error}"))
}

fn image_data(name: &str, asset: &Value) -> Result<Value, String> {
    let kind = asset.get("type").and_then(Value::as_str);
    let format = asset.get("format").and_then(Value::as_str);
    match (kind, format, asset.get("data_base64")) {
        (Some("image"), Some(format), Some(data @ Value::String(_)))
            if FORMATS.contains(&format.to_ascii_lowercase().as_str()) =>
        {
            Ok(data.clone())
        }
        _ => Err(format!(
            "asset {name:?} must be {{\"type\":\"image\",\"format\":\"png\"|\"jpeg\",\"data_base64\":...}}"
        )),
    }
}

/// Whether `value` holds the string `name` anywhere, as `TypeSafe` references an asset by key.
fn mentions(value: &Value, name: &str) -> bool {
    match value {
        Value::String(text) => text == name,
        Value::Array(items) => items.iter().any(|item| mentions(item, name)),
        Value::Object(fields) => fields.values().any(|field| mentions(field, name)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn translate(body: &Value) -> Result<Option<Value>, String> {
        assets_as_images(&serde_json::to_vec(body).unwrap())
            .map(|bytes| bytes.map(|bytes| serde_json::from_slice(&bytes).unwrap()))
    }

    fn png(data: &str) -> Value {
        json!({"type":"image","format":"png","data_base64":data})
    }

    #[test]
    fn a_request_without_assets_is_forwarded_unchanged() {
        assert_eq!(
            translate(&json!({"model":"m","state":"s","questions":{}})),
            Ok(None)
        );
        assert_eq!(assets_as_images(b"not json"), Ok(None));
    }

    #[test]
    fn state_images_become_the_shared_images_list() {
        let translated = translate(&json!({
            "model":"m","state":{"shot":"a","logo":"b"},
            "assets":{"a":png("AAA"),"b":png("BBB")},
            "questions":{"x":{"type":"noul","instructions":"Is it red?"},
                         "y":{"type":"noul","instructions":"Is it round?"}}
        }))
        .unwrap()
        .unwrap();
        assert_eq!(translated["images"], json!(["AAA", "BBB"]));
        assert!(translated.get("assets").is_none());
        assert_eq!(translated["state"], json!({"shot":"a","logo":"b"}));
    }

    #[test]
    fn one_question_may_reference_its_image_anywhere() {
        let translated = translate(&json!({
            "model":"m","state":"compare",
            "assets":{"a":png("AAA")},
            "questions":{"answer":{"type":"noul","instructions":{"look":"a"}}}
        }))
        .unwrap()
        .unwrap();
        assert_eq!(translated["images"], json!(["AAA"]));
    }

    #[test]
    fn an_image_of_one_question_among_several_is_refused() {
        let refused = translate(&json!({
            "model":"m","state":"compare",
            "assets":{"a":png("AAA")},
            "questions":{"x":{"type":"noul","instructions":{"look":"a"}},
                         "y":{"type":"noul","instructions":"Plain?"}}
        }));
        assert!(refused.unwrap_err().contains("own request"));
    }

    #[test]
    fn malformed_or_doubled_images_are_refused() {
        let base = json!({"model":"m","state":"a","questions":{"answer":{"type":"noul","instructions":"?"}}});
        for (assets, extra) in [
            (json!([png("AAA")]), None),
            (
                json!({"a":{"type":"audio","format":"wav","data_base64":"AAA"}}),
                None,
            ),
            (
                json!({"a":{"type":"image","format":"gif","data_base64":"AAA"}}),
                None,
            ),
            (json!({"a":png("AAA")}), Some(json!(["BBB"]))),
        ] {
            let mut body = base.clone();
            body["assets"] = assets;
            if let Some(images) = extra {
                body["images"] = images;
            }
            assert!(translate(&body).is_err(), "{body}");
        }
    }
}
