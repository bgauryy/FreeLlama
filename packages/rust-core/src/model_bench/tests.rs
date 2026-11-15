use super::*;

#[test]
fn correct_text_cannot_pass_without_a_complete_error_free_assistant_message() {
    let case = BenchCase {
        id: "exact",
        capability: Capability::Completion,
        num_ctx: 2048,
        kind: CaseKind::Exact,
    };
    let complete = json!({"done":true,"message":{"role":"assistant","content":"FREELLAMA_OK"}});
    assert!(grade_completed_case(&case, 100, &complete).passed);
    for completion in [json!(false), Value::Null, json!("true")] {
        let mut incomplete = complete.clone();
        incomplete["done"] = completion;
        let result = grade_completed_case(&case, 100, &incomplete);
        assert!(!result.passed, "{incomplete}");
        assert!(result.error.unwrap().contains("done:true"));
    }
    for error in [
        json!("generation failed"),
        json!({"message":"generation failed"}),
    ] {
        let mut failed = complete.clone();
        failed["error"] = error;
        assert!(!grade_completed_case(&case, 100, &failed).passed);
    }
    for message in [
        json!({"role":"user","content":"FREELLAMA_OK"}),
        json!({"content":"FREELLAMA_OK"}),
        json!({"role":"assistant","content":42}),
    ] {
        let mut invalid = complete.clone();
        invalid["message"] = message;
        assert!(!grade_completed_case(&case, 100, &invalid).passed);
    }
}

#[test]
fn embedding_benchmark_keeps_its_non_chat_completion_contract() {
    let case = BenchCase {
        id: "embedding",
        capability: Capability::Embedding,
        num_ctx: 2048,
        kind: CaseKind::Embedding,
    };
    let mut response = json!({"embeddings":[[0.1,0.2],[0.2,0.3]]});
    assert!(grade_completed_case(&case, 100, &response).passed);
    response["error"] = json!("embedding failed");
    assert!(!grade_completed_case(&case, 100, &response).passed);
}

fn decision_case() -> BenchCase {
    BenchCase {
        id: "decision/noul",
        capability: Capability::Decision,
        num_ctx: 2048,
        kind: CaseKind::Decision,
    }
}

fn decision_answer(stopped: &Value, moving: &Value) -> Value {
    json!({
        "model": "d",
        "answers": {
            "stopped": {"type": "noul", "noul": stopped},
            "moving": {"type": "noul", "noul": moving}
        },
        "usage": {"input_tokens": 179, "output_tokens": 0}
    })
}

#[test]
fn decision_case_passes_only_on_confident_correct_probabilities() {
    let case = decision_case();
    // Live shape from Ollama 0.40.15 `/v1/systemone` with strands-decider.
    let correct = decision_answer(&json!(0.95), &json!(0.07));
    let result = grade_completed_case(&case, 50, &correct);
    assert!(result.passed, "{result:?}");
    assert_eq!(result.prompt_tokens, Some(179));
    assert_eq!(result.output_tokens, Some(0));
    assert_eq!(
        result.output_excerpt.as_deref(),
        Some("stopped=0.950 moving=0.070")
    );

    for (stopped, moving) in [
        (json!(0.07), json!(0.95)), // inverted
        (json!(0.9), json!(0.9)),   // always yes
        (json!(0.55), json!(0.45)), // hedging
        (json!(1.5), json!(0.0)),   // not a probability
        (json!("0.9"), json!(0.1)), // not a number
        (Value::Null, json!(0.1)),
    ] {
        let result = grade_completed_case(&case, 50, &decision_answer(&stopped, &moving));
        assert!(!result.passed, "{result:?}");
        assert!(result.error.is_some());
    }

    let mut missing = correct.clone();
    missing["answers"].as_object_mut().unwrap().remove("moving");
    assert!(!grade_completed_case(&case, 50, &missing).passed);
    let mut wrong_type = correct.clone();
    wrong_type["answers"]["moving"]["type"] = json!("choice");
    assert!(!grade_completed_case(&case, 50, &wrong_type).passed);
    let mut upstream_error = correct;
    upstream_error["error"] = json!("model is not a decision model");
    assert!(!grade_completed_case(&case, 50, &upstream_error).passed);
}

/// The bench must send what the platform's decision tasks send to `/v1/systemone`: a state
/// and named questions, with no chat or option fields the endpoint does not read.
#[test]
fn decision_request_uses_the_systemone_task_shape() {
    let model = ModelMetadata {
        name: "strands-decider:2b-nvfp4".to_owned(),
        digest: Some("sha".to_owned()),
        size: 1,
        format: String::new(),
        family: String::new(),
        parameter_size: String::new(),
        quantization: String::new(),
        capabilities: vec![Capability::Decision],
        model_type: ModelType::Decision,
        advertised_context: None,
    };
    let request = decision_request(&model);
    let keys = request
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        keys,
        BTreeSet::from(["keep_alive", "model", "questions", "state"])
    );
    for name in [DECISION_TRUE_QUESTION, DECISION_FALSE_QUESTION] {
        assert_eq!(request["questions"][name]["type"], "noul");
    }
}

#[test]
fn digest_observation_considers_every_same_name_entry() {
    let listing = json!({"models": [
        {"name": "m:1", "digest": "old"},
        {"name": "m:1", "digest": "new"},
        {"model": "n:1", "digest": "new"},
        {"name": "o:1"}
    ]});
    let observe = |name| observe_digest(Some(&listing), name, "new");
    assert_eq!(observe("m:1"), DigestObservation::Matches);
    assert_eq!(observe("n:1"), DigestObservation::Matches);
    assert_eq!(
        observe_digest(Some(&listing), "m:1", "other"),
        DigestObservation::Differs
    );
    assert_eq!(observe("o:1"), DigestObservation::Absent);
    assert_eq!(observe("absent:1"), DigestObservation::Absent);
    assert_eq!(
        observe_digest(Some(&json!({"error": "x"})), "m:1", "new"),
        DigestObservation::Unavailable
    );
    assert_eq!(
        observe_digest(None, "m:1", "new"),
        DigestObservation::Unavailable
    );
}

/// A model evicted by another client after a complete response is still graded when
/// `/api/tags` names the same digest; any observed different digest fails, and a digest that
/// neither endpoint shows fails with its own message.
#[test]
fn completion_identity_falls_back_to_tags_only_for_an_unobserved_runner() {
    use DigestObservation::{Absent, Differs, Matches, Unavailable};
    assert_eq!(completion_identity(Matches, Unavailable), Ok(None));
    let evicted = completion_identity(Absent, Matches).unwrap().unwrap();
    assert!(evicted.contains("evicted after the response completed"));
    let ps_down = completion_identity(Unavailable, Matches).unwrap().unwrap();
    assert!(ps_down.contains("/api/ps was unavailable"));
    for (resident, listed) in [
        (Differs, Unavailable),
        (Differs, Matches),
        (Absent, Differs),
    ] {
        assert_eq!(
            completion_identity(resident, listed),
            Err(DIGEST_CHANGED.to_owned())
        );
    }
    for (resident, listed) in [
        (Absent, Absent),
        (Absent, Unavailable),
        (Unavailable, Unavailable),
        (Unavailable, Absent),
    ] {
        assert_eq!(
            completion_identity(resident, listed),
            Err(DIGEST_UNOBSERVED.to_owned())
        );
    }
    assert_ne!(DIGEST_CHANGED, DIGEST_UNOBSERVED);
}

#[test]
fn benchmark_prefill_rate_uses_uncached_tokens() {
    let mut value = json!({"prompt_eval_count":10,"prompt_eval_duration":1_000_000_000});
    assert_eq!(
        rate(&value, "prompt_eval_count", "prompt_eval_duration"),
        None
    );
    value["prompt_eval_cached_count"] = json!(4);
    assert_eq!(
        rate(&value, "prompt_eval_count", "prompt_eval_duration"),
        Some(6.0)
    );
    value["prompt_eval_cached_count"] = json!(11);
    assert_eq!(
        rate(&value, "prompt_eval_count", "prompt_eval_duration"),
        None
    );
}
