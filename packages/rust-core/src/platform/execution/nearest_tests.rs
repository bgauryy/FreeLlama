use super::*;
use serde_json::json;

fn request(value: Value) -> NearestRequest {
    serde_json::from_value(value).unwrap()
}

fn labels(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn task(value: Value) -> TaskInput {
    serde_json::from_value(value).unwrap()
}

#[test]
fn query_mode_ranks_items_by_closeness_and_skips_empty_vectors_last() {
    let vectors = vec![
        vec![1.0, 0.0],
        vec![0.0, 1.0],
        vec![0.9, 0.1],
        vec![0.0, 0.0],
    ];
    let ranked = nearest(
        &vectors,
        &labels(&["far", "near", "empty"]),
        &request(json!({"query":"q","top":2})),
    );
    assert_eq!(ranked["mode"], "query");
    assert_eq!(ranked["matches"][0]["label"], "near");
    assert_eq!(ranked["matches"][0]["index"], 1);
    assert_eq!(ranked["matches"][1]["label"], "far");
    assert_eq!(ranked["matches"].as_array().unwrap().len(), 2);
}

#[test]
fn scores_are_rounded_and_min_score_filters() {
    let vectors = vec![vec![1.0, 0.0], vec![1.0, 1.0], vec![0.0, 1.0]];
    let ranked = nearest(
        &vectors,
        &labels(&["diag", "orthogonal"]),
        &request(json!({"query":"q","min_score":0.5})),
    );
    let matches = ranked["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0]["score"], 0.707);
}

#[test]
fn equal_scores_keep_input_order() {
    let vectors = vec![
        vec![1.0, 0.0],
        vec![1.0, 0.0],
        vec![2.0, 0.0],
        vec![3.0, 0.0],
    ];
    let ranked = nearest(
        &vectors,
        &labels(&["a", "b", "c"]),
        &request(json!({"query":"q"})),
    );
    let order: Vec<_> = ranked["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["label"].as_str().unwrap())
        .collect();
    assert_eq!(order, ["a", "b", "c"]);
}

#[test]
fn a_file_is_listed_once_as_its_best_chunk() {
    let vectors = vec![
        vec![1.0, 0.0],
        vec![0.5, 0.5],
        vec![0.9, 0.1],
        vec![0.0, 1.0],
    ];
    let ranked = nearest(
        &vectors,
        &labels(&["a.rs:L1-10", "a.rs:L11-20", "b.rs:L1-5"]),
        &request(json!({"query":"q"})),
    );
    let found: Vec<_> = ranked["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["label"].as_str().unwrap())
        .collect();
    assert_eq!(found, ["a.rs:L11-20", "b.rs:L1-5"]);
}

#[test]
fn pairs_mode_returns_the_most_similar_pairs_and_skips_chunks_of_one_file() {
    let vectors = vec![
        vec![1.0, 0.0],
        vec![1.0, 0.01],
        vec![0.0, 1.0],
        vec![1.0, 0.0],
    ];
    let ranked = nearest(
        &vectors,
        &labels(&["a:L1-2", "a:L3-4", "x", "y"]),
        &request(json!({"top":2})),
    );
    assert_eq!(ranked["mode"], "pairs");
    let pairs = ranked["pairs"].as_array().unwrap();
    assert_eq!(pairs.len(), 2);
    assert_eq!(pairs[0]["a"]["label"], "a:L1-2");
    assert_eq!(pairs[0]["b"]["label"], "y");
    assert_eq!(pairs[0]["score"], 1.0);
    for pair in pairs {
        assert!(!(pair["a"]["label"] == "a:L1-2" && pair["b"]["label"] == "a:L3-4"));
    }
}

#[test]
fn top_is_capped_at_one_hundred() {
    let mut vectors = vec![vec![1.0, 0.0]];
    vectors.extend((0..150).map(|_| vec![1.0, 0.0]));
    let ranked = nearest(
        &vectors,
        &[],
        &NearestRequest {
            query: Some("q".into()),
            top: Some(500),
            ..Default::default()
        },
    );
    assert_eq!(ranked["matches"].as_array().unwrap().len(), 100);
    assert_eq!(ranked["matches"][0]["label"], "input[0]");
}

#[test]
fn overflow_and_underflow_safe_cosine() {
    let same = nearest(
        &[vec![1e308, 1e308], vec![1e308, 1e308]],
        &labels(&["same"]),
        &request(json!({"query":"q"})),
    );
    assert_eq!(same["matches"][0]["score"], 1.0);
    let opposite = nearest(
        &[vec![1e-300, 0.0], vec![-1e-300, 0.0]],
        &labels(&["o"]),
        &request(json!({"query":"q"})),
    );
    assert_eq!(opposite["matches"][0]["score"], -1.0);
}

#[test]
fn malformed_vectors_are_refused() {
    assert!(validate_vectors(None, 1, None).is_err());
    assert!(validate_vectors(Some(&json!([[1.0]])), 2, None).is_err());
    assert!(validate_vectors(Some(&json!([[]])), 1, None).is_err());
    assert!(validate_vectors(Some(&json!([[1.0], [1.0, 2.0]])), 2, None).is_err());
    assert!(validate_vectors(Some(&json!([["x"]])), 1, None).is_err());
    assert!(validate_vectors(Some(&json!([[1.0, 2.0]])), 1, Some(3)).is_err());
    assert_eq!(
        validate_vectors(Some(&json!([[1.0, 2.0]])), 1, None).unwrap(),
        vec![vec![1.0, 2.0]]
    );
}

#[test]
fn nearest_is_validated_before_a_job_is_accepted() {
    let ok = task(
        json!({"task":"embedding","input":["a","b"],"nearest":{"query":"q","top":3,"labels":["a","b"]}}),
    );
    assert!(validate(&ok).is_ok());
    for (body, why) in [
        (
            json!({"task":"completion","prompt":"x","nearest":{}}),
            "task",
        ),
        (
            json!({"task":"embedding","input":["a"],"nearest":{"top":0}}),
            "top",
        ),
        (
            json!({"task":"embedding","input":["a"],"nearest":{"top":101}}),
            "top",
        ),
        (
            json!({"task":"embedding","input":["a"],"nearest":{"min_score":2}}),
            "min",
        ),
        (
            json!({"task":"embedding","input":["a"],"nearest":{"labels":["a","b"]}}),
            "labels",
        ),
        (
            json!({"task":"embedding","input":["a"],"return_embeddings":true}),
            "return",
        ),
    ] {
        assert!(validate(&task(body)).is_err(), "{why}");
    }
    let many: Vec<_> = (0..=MAX_PAIRWISE_ITEMS).map(|i| i.to_string()).collect();
    assert!(validate(&task(json!({"task":"embedding","input":many,"nearest":{}}))).is_err());
    assert!(
        validate(&task(
            json!({"task":"embedding","input":many,"nearest":{"query":"q"}})
        ))
        .is_ok()
    );
}

#[test]
fn chunk_labels_name_their_file() {
    assert_eq!(file_of("src/a.rs:L1-20"), Some("src/a.rs"));
    assert_eq!(file_of("a:L1"), None);
    assert_eq!(file_of("plain"), None);
}

#[test]
fn ranking_matches_the_original_typescript_ranker() {
    let vectors = vec![
        vec![1.0, 0.0],
        vec![0.9, 0.1],
        vec![0.0, 1.0],
        vec![0.95, 0.05],
    ];
    let ranked = nearest(
        &vectors,
        &labels(&["a", "b", "c"]),
        &request(json!({"query":"q","top":2})),
    );
    assert_eq!(
        ranked,
        json!({"mode":"query","matches":[
            {"index":2,"label":"c","score":0.999},
            {"index":0,"label":"a","score":0.994},
        ]})
    );
    let pairs = nearest(
        &vectors,
        &labels(&["w", "x", "y", "z"]),
        &request(json!({"top":1})),
    );
    assert_eq!(
        pairs["pairs"],
        json!([{"a":{"index":0,"label":"w"},"b":{"index":3,"label":"z"},"score":0.999}])
    );
    let none = nearest(
        &vectors,
        &labels(&["w", "x", "y", "z"]),
        &request(json!({"top":3,"min_score":1.0})),
    );
    assert_eq!(none["pairs"], json!([]));
    let chunks = nearest(
        &[vec![1.0, 0.0], vec![1.0, 0.0], vec![0.99, 0.01]],
        &labels(&["a.rs:L1-9", "a.rs:L10-20", "b.rs:L1-5"]),
        &request(json!({"top":1})),
    );
    assert_eq!(chunks["pairs"][0]["b"]["label"], "b.rs:L1-5");
}
