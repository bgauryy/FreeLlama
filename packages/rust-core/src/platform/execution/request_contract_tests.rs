//! Payload and control requirements are established before any model load.

use super::*;
use crate::platform::{RouteInput, SessionAffinity, select_route};

fn input(value: Value) -> TaskInput {
    serde_json::from_value(value).unwrap()
}

fn model(name: &str, capabilities: &[&str], thinking: Value) -> CatalogModel {
    let mut value = json!({
        "name":name,"digest":name,"size":1,"capabilities":capabilities,
        "advertised_context":32768,"resident":false,"benchmark":{},"policy_rank":{}
    });
    value["thinking"] = thinking;
    serde_json::from_value(value).unwrap()
}

#[test]
fn requested_thinking_is_an_eligibility_requirement_and_named_levels_are_model_defined() {
    let plain = model(
        "plain",
        &["completion"],
        json!({"values":[false],"default":false}),
    );
    let reasoning = model(
        "reasoning",
        &["completion", "thinking"],
        json!({"values":[false,"deep"],"default":"deep"}),
    );
    for think in [json!(true), json!("deep")] {
        let mut task =
            input(json!({"task":"completion","prompt":"answer","request_options":{"think":think}}));
        derive_input_capabilities(&mut task).unwrap();
        let selected = select_route(
            &task.route,
            &[plain.clone(), reasoning.clone()],
            &SessionAffinity::default(),
        )
        .unwrap();
        assert_eq!(selected.selected_model, "reasoning");
        validate_model_controls(&reasoning, &task.request_options).unwrap();
    }
    let unsupported =
        input(json!({"task":"completion","prompt":"answer","request_options":{"think":"medium"}}));
    assert!(validate_model_controls(&reasoning, &unsupported.request_options).is_err());
    let mut off =
        input(json!({"task":"completion","prompt":"answer","request_options":{"think":false}}));
    derive_input_capabilities(&mut off).unwrap();
    assert!(
        !off.route
            .required_capabilities
            .contains(&Capability::Thinking)
    );
}

#[test]
fn generic_profiles_use_the_advertised_default_when_their_preference_is_unsupported() {
    let reasoning = model(
        "reasoning",
        &["completion", "thinking"],
        json!({"values":["deep"],"default":"deep"}),
    );
    for task in [TaskKind::Coding, TaskKind::Completion] {
        let route = select_route(
            &RouteInput {
                task,
                ..RouteInput::default()
            },
            std::slice::from_ref(&reasoning),
            &SessionAffinity::default(),
        )
        .unwrap();
        assert_eq!(route.think, "deep");
    }
}

#[test]
fn embedding_media_require_the_corresponding_modality_before_selection() {
    let mut task = input(
        json!({"task":"embedding","input":["text",{"text":"caption","image":"cG5n"},{"audio":"UklGRjAwMDBXQVZF"}]}),
    );
    derive_input_capabilities(&mut task).unwrap();
    assert!(
        task.route
            .required_capabilities
            .contains(&Capability::Vision)
    );
    assert!(
        task.route
            .required_capabilities
            .contains(&Capability::Audio)
    );
    let text_only = model("text", &["embedding"], Value::Null);
    assert!(select_route(&task.route, &[text_only], &SessionAffinity::default()).is_err());
    for payload in [
        json!({"video":"x"}),
        json!({"image":3}),
        json!({"text":""}),
        json!([17]),
    ] {
        let mut invalid = input(json!({"task":"embedding","input":payload}));
        assert!(derive_input_capabilities(&mut invalid).is_err());
    }
}

#[test]
fn wav_in_the_legacy_images_field_is_audio_for_decision_and_chat() {
    let wav = base64::engine::general_purpose::STANDARD.encode(b"RIFF0000WAVEpayload");
    for task in ["decision", "completion"] {
        let mut request = input(json!({"task":task,"images":[wav]}));
        derive_input_capabilities(&mut request).unwrap();
        assert!(
            request
                .route
                .required_capabilities
                .contains(&Capability::Audio)
        );
        assert!(
            !request
                .route
                .required_capabilities
                .contains(&Capability::Vision)
        );
        assert!(!task_has_images(&request));
    }
    let mut mixed = input(json!({"task":"decision","images":[wav,"cG5n"]}));
    derive_input_capabilities(&mut mixed).unwrap();
    assert!(
        mixed
            .route
            .required_capabilities
            .contains(&Capability::Audio)
    );
    assert!(
        mixed
            .route
            .required_capabilities
            .contains(&Capability::Vision)
    );
    assert!(task_has_images(&mixed));
}

#[test]
fn image_embeddings_are_not_chat_models_and_unused_embedding_fields_are_rejected() {
    let image_embedder = model("image-embed", &["embedding", "vision"], Value::Null);
    assert!(
        select_route(
            &RouteInput {
                task: TaskKind::Vision,
                ..RouteInput::default()
            },
            &[image_embedder],
            &SessionAffinity::default()
        )
        .is_err()
    );
    let request = input(json!({"task":"embedding","input":"text","images":["cG5n"]}));
    assert!(require_payload(&request).is_err());
}

#[test]
fn vision_without_an_image_is_refused_before_admission() {
    let error = require_payload(&input(json!({"task":"vision","prompt":"describe"}))).unwrap_err();
    assert!(
        error.body.error.contains("vision task requires an image"),
        "{}",
        error.body.error
    );
    // A WAV in `images` is audio, not an image.
    let wav = "UklGRjAwMDBXQVZF";
    assert!(require_payload(&input(json!({"task":"vision","prompt":"x","images":[wav]}))).is_err());
    assert!(
        require_payload(&input(
            json!({"task":"vision","prompt":"x","images":["cG5n"]})
        ))
        .is_ok()
    );
    assert!(
        require_payload(&input(
            json!({"task":"vision","messages":[{"role":"user","content":"x","images":["cG5n"]}]})
        ))
        .is_ok()
    );
    // Completion needs no image.
    assert!(require_payload(&input(json!({"prompt":"describe"}))).is_ok());
}
